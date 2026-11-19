//! Аудио-движок: очередь, загрузка и предзагрузка треков, фидбек волне, воспроизведение.
//! Живёт в своём потоке, UI шлёт ему команды и читает `Shared`.

use crate::api::{Account, Api, Batch, Res, Track, WAVE};
use crate::config::log;
use crate::eq::{Eq, EqParams};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source as _};
use std::collections::{HashSet, VecDeque};
use std::io::Cursor;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Default, Debug)]
pub enum Status {
    #[default]
    LoggedOut,
    Connecting,
    Ready,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ListKind {
    Playlist,
    Album,
    Artist,
    Likes,
    Tracks,
}

/// Что сейчас играет.
#[derive(Clone, PartialEq, Debug, Default)]
pub enum Source {
    #[default]
    Wave,
    /// Волна по станции: `artist:ID`, `track:ID`, `album:ID`, `playlist:UID_KIND`.
    Seed { station: String, header: String, title: String },
    List { kind: ListKind, id: String, title: String },
}

impl Source {
    pub fn station(&self) -> Option<&str> {
        match self {
            Source::Wave => Some(WAVE),
            Source::Seed { station, .. } => Some(station),
            Source::List { .. } => None,
        }
    }
}

/// Что играть, когда список закончится.
#[derive(Clone, Debug)]
pub enum After {
    Loop,
    Wave(Source),
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Default)]
pub struct Shared {
    pub(crate) like_error: Option<String>,
    pub(crate) changed_likes: HashSet<String>,
    pub status: Status,
    pub uid: String,
    pub login: String,
    pub has_plus: bool,
    pub track: Option<Track>,
    pub source: Source,
    pub upcoming: Vec<Track>,
    pub playing: bool,
    pub(crate) paused: bool,
    pub loading: bool,
    pub position: f32,
    pub duration: f32,
    pub volume: f32,
    pub repeat: Repeat,
    pub shuffle: bool,
    pub error: Option<(String, Instant)>,
    pub(crate) audio_output_error: Option<(String, Instant)>,
}

impl Shared {
    pub(crate) fn can_pause(&self) -> bool {
        self.playing || (self.loading && !self.paused)
    }
}

pub enum Cmd {
    #[cfg(all(windows, feature = "asio"))]
    Output(Option<String>, Box<dyn FnOnce(Option<String>, Option<String>) + Send>),
    Login(String),
    Logout,
    PlayPause,
    /// Снять с паузы (если уже играет — ничего не делать).
    Resume,
    Pause,
    Next,
    Prev,
    Seek(f32),
    #[cfg(target_os = "linux")]
    SeekBy(f32),
    #[cfg(target_os = "linux")]
    SeekTrack(mpris_server::TrackId, i64),
    Volume(f32),
    LikeTrack(Track),
    Dislike,
    Wave(Source),
    PlayList { source: Source, tracks: Vec<Track>, start: usize, after: After },
    /// Перейти к треку из очереди (индекс в `upcoming`).
    Jump(usize),
    Repeat(Repeat),
    Shuffle(bool),
    /// «Играть следующим» (`next`) или «Добавить в конец очереди».
    Enqueue { track: Track, next: bool },
    /// Переключатель «Треки 18+».
    AllowExplicit(bool),
}

enum Msg {
    Cmd(Cmd),
    Account(u64, Res<Account>),
    Likes(u64, Res<Vec<String>>),
    Batch { res: Res<Batch>, gen: u64 },
    Audio { gen: u64, res: Res<Vec<u8>> },
    Prefetch { generation: u64, id: String, res: Res<Vec<u8>> },
    LikeSaved { id: String, seq: u64, liked: bool, result: Res<()> },
    Error(String),
}

#[derive(Clone)]
struct AudioSender {
    channel: Sender<Msg>,
    #[cfg(all(windows, feature = "asio"))]
    wake: Option<Arc<std::os::windows::io::OwnedHandle>>,
}

impl From<Sender<Msg>> for AudioSender {
    fn from(channel: Sender<Msg>) -> Self {
        #[cfg(all(windows, feature = "asio"))]
        let wake = {
            use std::os::windows::io::{FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::Threading::CreateEventW;
            // Auto-reset: одно пробуждение достаточно для всех накопившихся команд.
            let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
            (!handle.is_null()).then(|| Arc::new(unsafe { OwnedHandle::from_raw_handle(handle) }))
        };
        Self { channel, #[cfg(all(windows, feature = "asio"))] wake }
    }
}

impl AudioSender {
    fn send(&self, msg: Msg) -> Result<(), ()> {
        self.channel.send(msg).map_err(|_| ())?;
        #[cfg(all(windows, feature = "asio"))]
        if let Some(event) = &self.wake {
            use std::os::windows::io::AsRawHandle;
            unsafe { windows_sys::Win32::System::Threading::SetEvent(event.as_raw_handle()); }
        }
        Ok(())
    }

    #[cfg(not(all(windows, feature = "asio")))]
    fn recv(&self, rx: &Receiver<Msg>, timeout: Duration) -> Result<Msg, RecvTimeoutError> {
        rx.recv_timeout(timeout)
    }

    #[cfg(all(windows, feature = "asio"))]
    fn recv(&self, rx: &Receiver<Msg>, timeout: Duration) -> Result<Msg, RecvTimeoutError> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{Foundation::WAIT_FAILED, UI::WindowsAndMessaging::*};
        let deadline = Instant::now() + timeout;
        loop {
            // Обслуживаем HWND драйвера и окна COM на создавшем их потоке.
            // Ограничиваем пачку, чтобы поток также успевал принять команды плеера.
            unsafe {
                let mut msg = std::mem::zeroed();
                for _ in 0..32 {
                    if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) == 0 { break; }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            match rx.try_recv() {
                Ok(msg) => return Ok(msg),
                Err(mpsc::TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Err(RecvTimeoutError::Timeout); }
            let Some(event) = &self.wake else { return rx.recv_timeout(remaining); };
            let handle = event.as_raw_handle();
            if unsafe { MsgWaitForMultipleObjectsEx(1, &handle, remaining.as_millis().max(1) as u32, QS_ALLINPUT, MWMO_INPUTAVAILABLE) } == WAIT_FAILED {
                return rx.recv_timeout(remaining);
            }
        }
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;
type Job = Box<dyn FnOnce() + Send>;
pub type Likes = Arc<Mutex<HashSet<String>>>;

#[derive(Clone)]
pub struct Player {
    tx: AudioSender,
    shared: Arc<Mutex<Shared>>,
    likes: Likes,
    pub eq: Arc<EqParams>,
    source_request: Arc<Mutex<u64>>,
}

impl Player {
    pub fn spawn(notify: Notify, asio_driver: Option<String>) -> Player {
        let (tx, rx) = mpsc::channel();
        let tx = AudioSender::from(tx);
        let shared = Arc::new(Mutex::new(Shared { volume: 0.6, ..Default::default() }));
        let likes: Likes = Default::default();
        let eq: Arc<EqParams> = Default::default();
        let p = Player { tx: tx.clone(), shared: shared.clone(), likes: likes.clone(), eq: eq.clone(), source_request: Default::default() };
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || Engine::new(tx, shared, likes, notify, eq, asio_driver).run(rx))
            .expect("audio thread");
        p
    }

    pub fn send(&self, c: Cmd) {
        if matches!(c, Cmd::Login(_) | Cmd::Logout | Cmd::Jump(_) | Cmd::Next | Cmd::Prev | Cmd::Wave(_) | Cmd::PlayList { .. }) {
            let mut request = self.source_request.lock().unwrap();
            *request = request.wrapping_add(1);
            let _ = self.tx.send(Msg::Cmd(c));
        } else {
            let _ = self.tx.send(Msg::Cmd(c));
        }
    }

    pub(crate) fn begin_source_request(&self) -> u64 {
        let mut request = self.source_request.lock().unwrap();
        *request = request.wrapping_add(1);
        *request
    }

    pub(crate) fn is_current_request(&self, request: u64) -> bool {
        *self.source_request.lock().unwrap() == request
    }

    pub(crate) fn send_for_request(&self, request: u64, command: Cmd) -> bool {
        let current = self.source_request.lock().unwrap();
        *current == request && self.tx.send(Msg::Cmd(command)).is_ok()
    }

    pub fn state(&self) -> Shared {
        self.shared.lock().unwrap().clone()
    }

    pub(crate) fn take_like_error(&self) -> Option<String> {
        self.shared.lock().unwrap().like_error.take()
    }

    pub(crate) fn take_changed_likes(&self) -> HashSet<String> {
        std::mem::take(&mut self.shared.lock().unwrap().changed_likes)
    }

    pub fn is_liked(&self, id: &str) -> bool {
        self.likes.lock().unwrap().contains(id)
    }
}

enum Output {
    System {
        _stream: OutputStream,
        handle: OutputStreamHandle,
        #[cfg(windows)]
        device: rodio::cpal::platform::WasapiDevice,
        #[cfg(windows)]
        observed_default: Option<rodio::cpal::platform::WasapiDevice>,
        #[cfg(windows)]
        last_progress: Option<(Duration, Instant)>,
        #[cfg(windows)]
        retry_default: Instant,
    },
    #[cfg(all(windows, feature = "asio"))]
    Asio(crate::asio::Stream),
}

impl Output {
    fn track_sink(&self, track: impl rodio::Source<Item = f32> + Send + 'static, volume: f32) -> Result<Sink, String> {
        match self {
            Self::System { handle, .. } => {
                let sink = Sink::try_new(handle).map_err(|e| e.to_string())?;
                sink.pause();
                sink.set_volume(volume);
                sink.append(track);
                Ok(sink)
            }
            #[cfg(all(windows, feature = "asio"))]
            Self::Asio(stream) => Ok(stream.sink(track, volume)),
        }
    }

    #[cfg(windows)]
    fn needs_reopen(&mut self, playing_position: Option<Duration>) -> bool {
        match self {
            Self::System { device, observed_default, last_progress, retry_default, .. } => {
                // Поток WASAPI может остановиться при прежнем идентификаторе устройства.
                let now = Instant::now();
                match (*last_progress, playing_position) {
                    (Some((previous, since)), Some(pos)) if previous == pos => {
                        if now.duration_since(since) >= Duration::from_secs(3) {
                            return true;
                        }
                    }
                    (_, position) => *last_progress = position.map(|pos| (pos, now)),
                }
                let next = system_device();
                if next != *observed_default {
                    *observed_default = next;
                    return true;
                }
                // Проверяем освободившийся выход до остановки работающего.
                if observed_default.as_ref() != Some(device) && now >= *retry_default {
                    *retry_default = now + Duration::from_secs(10);
                    if next.is_some_and(|target| open_system_device(target, None).is_ok()) {
                        return true;
                    }
                }
                use rodio::cpal::{platform::DeviceInner, traits::HostTrait};
                observed_default.as_ref() != Some(device)
                    && !rodio::cpal::default_host().output_devices().is_ok_and(|mut devices| {
                        devices.any(|d| matches!(d.as_inner(), DeviceInner::Wasapi(d) if d == device))
                    })
            }
            #[cfg(feature = "asio")]
            Self::Asio(stream) => stream.needs_reopen(),
        }
    }
}

#[cfg(windows)]
fn system_device() -> Option<rodio::cpal::platform::WasapiDevice> {
    use rodio::cpal::{platform::DeviceInner, traits::HostTrait};
    match rodio::cpal::default_host().default_output_device()?.into_inner() {
        DeviceInner::Wasapi(device) => Some(device),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

fn open_output(asio_driver: Option<&str>) -> Result<Output, String> {
    #[cfg(all(windows, feature = "asio"))]
    if let Some(name) = asio_driver {
        return crate::asio::Stream::open(name).map(Output::Asio);
    }
    #[cfg(not(all(windows, feature = "asio")))]
    let _ = asio_driver;
    #[cfg(windows)]
    {
        open_system_output(None)
    }
    #[cfg(not(windows))]
    {
        let (stream, handle) = OutputStream::try_default()
            .map_err(|e| format!("Не удалось открыть системный аудиовыход: {e}"))?;
        Ok(Output::System { _stream: stream, handle })
    }
}

#[cfg(windows)]
fn open_system_output(previous: Option<rodio::cpal::platform::WasapiDevice>) -> Result<Output, String> {
    use rodio::cpal::{platform::DeviceInner, traits::HostTrait};
    let default = system_device();
    first_system_output(previous, default.clone(), || {
        rodio::cpal::default_host().output_devices().into_iter().flatten().filter_map(|device| {
            #[allow(unreachable_patterns)]
            match device.into_inner() { DeviceInner::Wasapi(d) => Some(d), _ => None }
        })
    }, |device| open_system_device(device, default.clone()))
}

#[cfg(any(windows, test))]
fn first_system_output<D: Clone + PartialEq, O, I: IntoIterator<Item = D>>(previous: Option<D>, default: Option<D>,
    others: impl FnOnce() -> I, open: impl FnMut(D) -> Result<O, String>) -> Result<O, String> {
    let mut open = open;
    let mut tried = Vec::new();
    let mut error = String::from("Системный аудиовыход недоступен");
    let mut try_device = |device: D| {
        if tried.contains(&device) { return None; }
        tried.push(device.clone());
        open(device).map_err(|e| error = e).ok()
    };
    if let Some(output) = previous.into_iter().chain(default).find_map(&mut try_device) { return Ok(output); }
    others().into_iter().find_map(try_device).ok_or(error)
}

#[cfg(test)]
#[test]
fn system_falls_back_to_third_device_in_order() {
    let mut tried = Vec::new();
    let out = first_system_output(Some("old"), Some("busy"), || ["busy", "old", "free"], |d| {
        tried.push(d);
        if d == "free" { Ok(d) } else { Err(d.to_string()) }
    });
    assert_eq!(out.as_deref(), Ok("free"));
    assert_eq!(tried, ["old", "busy", "free"]);
}

#[cfg(windows)]
fn open_system_device(
    device: rodio::cpal::platform::WasapiDevice,
    observed_default: Option<rodio::cpal::platform::WasapiDevice>,
) -> Result<Output, String> {
    let (stream, handle) = OutputStream::try_from_device(&rodio::Device::from(device.clone()))
        .map_err(|e| format!("Не удалось открыть системный аудиовыход: {e}"))?;
    Ok(Output::System { _stream: stream, handle, device, observed_default, last_progress: None, retry_default: Instant::now() + Duration::from_secs(10) })
}

/// Сколько живёт сессия волны без прослушивания: оригинал начинает новую через 9 часов.
const WAVE_SESSION_TTL: u64 = 9 * 3600;

struct WaveState {
    uid: String,
    session: String,
    played: Vec<String>,
    at: u64,
}

fn load_wave_state() -> Option<WaveState> {
    let text = std::fs::read_to_string(crate::config::dir().join("wave.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let str_at = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    Some(WaveState {
        uid: str_at("uid"),
        session: str_at("session"),
        played: v
            .get("played")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        at: v.get("at").and_then(|x| x.as_u64()).unwrap_or(0),
    })
}

fn load_track(api: &Api, t: &Track) -> Res<Vec<u8>> {
    let url = api.stream_url(&t.id)?;
    api.fetch_bytes(&url)
}

/// Перемешивание без лишних зависимостей (xorshift от времени).
pub fn shuffle<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

/// Сортируем оставшиеся треки списка. Вставки из других источников сохраняют свои места.
fn restore_queue_order(queue: &mut [Track], start: usize) {
    let mut remaining: Vec<_> = queue.iter().filter_map(|t| Some((t.list_position?, t.clone()))).collect();
    remaining.sort_by_key(|(i, _)| (i < &start, *i));
    let mut remaining = remaining.into_iter();
    for track in queue.iter_mut().filter(|t| t.list_position.is_some()) {
        if let Some((_, value)) = remaining.next() { *track = value; }
    }
}

#[cfg(test)]
#[test]
fn unshuffle_keeps_manual_and_consumed_tracks_in_place() {
    let mut tracks: Vec<_> = (0..5).map(|id| crate::api::parse_track(&serde_json::json!({"id":id, "title":"T"})).unwrap()).collect();
    for (i, track) in tracks[..4].iter_mut().enumerate() { track.list_position = Some(i); }
    let manual = crate::api::parse_track(&serde_json::json!({"id":2, "title":"ручная вставка"})).unwrap();
    let mut queue = vec![manual, tracks[2].clone(), tracks[4].clone(), tracks[1].clone()];
    restore_queue_order(&mut queue, 1);
    assert_eq!(queue[0].title, "ручная вставка");
    assert_eq!(queue.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["2", "1", "4", "2"]);
}

enum End {
    Finished,
    Skip,
    Dislike,
    /// Волну запустили заново: пропуск, но очередь уже свежая.
    Restart,
}

struct ListPlay {
    deferred: VecDeque<Track>,
    all: Vec<Track>,
    after: After,
}

struct PendingLike {
    first: u64,
    latest: u64,
    confirmed: bool,
}

struct Engine {
    tx: AudioSender,
    bg: Sender<Job>,
    like_seq: u64,
    local_likes: HashSet<String>,
    pending_likes: std::collections::HashMap<String, PendingLike>,
    shared: Arc<Mutex<Shared>>,
    likes: Likes,
    notify: Notify,
    eq: Arc<EqParams>,
    asio_driver: Option<String>,
    audio: Option<Output>,
    sink: Option<Sink>,
    position_base: Duration,
    #[cfg(windows)]
    next_output_check: Instant,
    api: Option<Api>,
    account_generation: u64,
    uid: String,
    source: Source,
    list: Option<ListPlay>,
    queue: VecDeque<Track>,
    history: Vec<Track>,
    current: Option<Track>,
    current_bytes: Option<Arc<[u8]>>,
    batch_id: String,
    /// Сессия ротора текущей волны; radioStarted уходит в неё, когда она появится.
    session: String,
    radio_started_pending: bool,
    /// Пропуски и дослушанные треки, которые уйдут со следующим запросом треков волны.
    deferred: Vec<serde_json::Value>,
    /// Сколько пакетов подряд состояло только из уже сыгранных треков.
    stale_batches: u32,
    /// Сыгранные треки текущей волны («id:альбом», до 100): уходят серверу как queue, чтобы он не повторялся.
    played: Vec<String>,
    /// Станция, к которой относится `played`.
    played_station: String,
    /// Сессия «Моей волны» с прошлого запуска уже подхвачена (или не нашлась).
    wave_restored: bool,
    fetching: bool,
    batch_gen: u64,
    // Выбранная станция заменяет старый трек, когда придёт первая пригодная запись.
    starting_wave: bool,
    retry_at: Option<Instant>,
    gen: u64,
    loaded: bool,
    paused: bool,
    volume: f32,
    repeat: Repeat,
    shuffle: bool,
    prefetch: Option<(String, Vec<u8>)>,
    prefetching: Option<String>,
    errors_in_row: u32,
    was_playing: bool,
}

impl Engine {
    fn new(tx: impl Into<AudioSender>, shared: Arc<Mutex<Shared>>, likes: Likes, notify: Notify, eq: Arc<EqParams>, asio_driver: Option<String>) -> Self {
        // Фидбек и лайки — по порядку в отдельном потоке, чтобы не блокировать плеер.
        let (bg, bg_rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("bg".into())
            .spawn(move || {
                for job in bg_rx {
                    job();
                }
            })
            .expect("bg thread");
        Engine {
            tx: tx.into(),
            bg,
            like_seq: 0,
            local_likes: HashSet::new(),
            pending_likes: Default::default(),
            shared,
            likes,
            notify,
            eq,
            asio_driver,
            audio: None,
            sink: None,
            position_base: Duration::ZERO,
            #[cfg(windows)]
            next_output_check: Instant::now(),
            api: None,
            account_generation: 0,
            uid: String::new(),
            source: Source::Wave,
            list: None,
            queue: VecDeque::new(),
            history: Vec::new(),
            current: None,
            current_bytes: None,
            batch_id: String::new(),
            session: String::new(),
            radio_started_pending: false,
            deferred: Vec::new(),
            stale_batches: 0,
            played: Vec::new(),
            played_station: String::new(),
            wave_restored: false,
            fetching: false,
            batch_gen: 0,
            starting_wave: false,
            retry_at: None,
            gen: 0,
            loaded: false,
            paused: true,
            volume: 0.6,
            repeat: Repeat::Off,
            shuffle: false,
            prefetch: None,
            prefetching: None,
            errors_in_row: 0,
            was_playing: false,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match self.tx.recv(&rx, Duration::from_millis(200)) {
                Ok(msg) => {
                    self.handle(msg);
                    self.sync();
                    (self.notify)();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
    }

    // ---------- вспомогательное ----------

    fn sh(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    fn sink(&self) -> Option<&Sink> {
        self.sink.as_ref()
    }

    fn ensure_audio(&mut self) -> Result<(), String> {
        if self.audio.is_none() {
            let output = open_output(self.asio_driver.as_deref());
            #[cfg(windows)]
            let output = if self.loaded && self.asio_driver.is_some() {
                output.or_else(|_| open_system_output(None))
            } else { output };
            self.audio = Some(output?);
        }
        Ok(())
    }

    fn pos(&self) -> f32 {
        (self.position_base + self.sink().map(Sink::get_pos).unwrap_or_default()).as_secs_f32()
    }

    fn attach(&mut self, bytes: Arc<[u8]>, mut pos: Duration) -> Result<(), String> {
        let decode = || Decoder::new(Cursor::new(bytes.clone())).map_err(|e| format!("Не удалось декодировать трек: {e}"));
        let mut decoder = decode()?;
        if !pos.is_zero() {
            if let Err(e) = decoder.try_seek(pos) {
                let message = format!("Не удалось восстановить позицию: {e}");
                if self.loaded { return Err(message); }
                // Перемотка пришла до загрузки. Сам трек можно воспроизвести с начала.
                log(message);
                decoder = decode()?;
                pos = Duration::ZERO;
            }
        }
        let track = Eq::new(decoder.convert_samples::<f32>(), self.eq.clone());
        let sink = self.audio.as_ref().ok_or("Аудиовыход недоступен")?.track_sink(track, self.volume * self.volume)?;
        self.sink = None;
        if !self.paused {
            sink.play();
        }
        self.sink = Some(sink);
        self.position_base = pos;
        Ok(())
    }

    fn seek(&mut self, pos: Duration) {
        if self.current.is_none() { return; }
        if self.audio.is_none() || !self.loaded {
            self.position_base = pos;
        } else if let Some(bytes) = self.current_bytes.clone() {
            if let Err(e) = self.attach(bytes, pos) { self.error(e); }
        }
    }

    fn output_error(&mut self, message: String) {
        log(format!("audio output: {message}"));
        self.sh().audio_output_error = Some((message, Instant::now()));
    }

    #[cfg(windows)]
    fn detach_output(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.pause();
            self.position_base += sink.get_pos();
        }
        self.audio = None;
    }

    #[cfg(windows)]
    fn open_at_position(&mut self, output: Result<Output, String>) -> Result<(), String> {
        self.audio = Some(output?);
        if self.loaded {
            let bytes = self.current_bytes.clone().ok_or("Нет данных текущего трека")?;
            self.attach(bytes, self.position_base)?;
        } else {
            // До первого Play устройство открывается только для проверки.
            self.audio = None;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn change_output(&mut self, driver: Option<String>, manual: bool) {
        let previous_system = match &self.audio {
            Some(Output::System { device, .. }) => Some(device.clone()),
            _ => None,
        };
        self.detach_output();
        match self.open_at_position(open_output(driver.as_deref())) {
            Ok(()) => {
                self.asio_driver = driver;
                self.sh().audio_output_error = None;
            }
            Err(mut message) => {
                self.audio = None;
                let old = self.asio_driver.clone();
                let restored_old = manual && old.is_some() && old != driver
                    && self.open_at_position(open_output(old.as_deref())).is_ok();
                if !restored_old && (self.loaded || !self.paused || old.is_none()) {
                    self.audio = None;
                    match self.open_at_position(open_system_output(if manual { previous_system } else { None })) {
                        Ok(()) => {
                            if manual { self.asio_driver = None; }
                            message.push_str(". Используется системный выход");
                        }
                        Err(e) => {
                            message = format!("{message}. Системный выход недоступен: {e}");
                            self.audio = None;
                            self.paused = true;
                        }
                    }
                }
                self.output_error(message);
            }
        }
    }

    fn error(&self, e: impl Into<String>) {
        let e = e.into();
        log(format!("error: {e}"));
        self.sh().error = Some((e, Instant::now()));
        (self.notify)();
    }

    fn feedback(&mut self, kind: &'static str, track: Option<&Track>, played: Option<f64>) {
        let Some(station) = self.source.station().map(String::from) else { return };
        let Some(api) = self.api.clone() else { return };
        if self.session.is_empty() && kind == "radioStarted" {
            self.radio_started_pending = true;
            return;
        }
        // Фидбек идёт с пакетом самого трека: следующий пакет часто приходит, пока доигрывает прошлый.
        let batch = track
            .map(|t| t.batch_id.clone())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| self.batch_id.clone());
        let length = track
            .filter(|t| kind == "trackFinished" && t.duration_ms > 0)
            .map(|t| t.duration_ms as f64 / 1000.0);
        let track = track.map(|t| t.full_id());
        let session = self.session.clone();
        if session.is_empty() {
            let _ = self.bg.send(Box::new(move || {
                if let Err(e) = api.feedback(&station, kind, track.as_deref(), played, &batch) {
                    log(format!("feedback {kind}: {e}"));
                }
            }));
            return;
        }
        let from = format!("desktop_win-radio-{}-default", station.replace(':', "-"));
        let body = crate::api::session_event(kind, track.as_deref(), played, length, &batch, &from);
        if matches!(kind, "skip" | "dislike" | "trackFinished") {
            self.deferred.push(body);
            return;
        }
        let _ = self.bg.send(Box::new(move || {
            if let Err(e) = api.session_feedback(&session, &body) {
                log(format!("feedback {kind}: {e}"));
            }
        }));
    }

    /// Отложенные фидбеки, которые не ушли с запросом треков, отправляются отдельно.
    fn flush_feedback(&mut self) {
        if self.deferred.is_empty() {
            return;
        }
        let list = std::mem::take(&mut self.deferred);
        let (Some(api), false) = (self.api.clone(), self.session.is_empty()) else { return };
        let session = self.session.clone();
        let _ = self.bg.send(Box::new(move || {
            for body in list {
                if let Err(e) = api.session_feedback(&session, &body) {
                    log(format!("feedback: {e}"));
                }
            }
        }));
    }

    fn stop_audio(&mut self) {
        self.sink = None;
        self.position_base = Duration::ZERO;
        self.loaded = false;
    }

    fn clear_queue(&mut self) {
        self.flush_feedback();
        self.stale_batches = 0;
        self.queue.clear();
        self.prefetch = None;
        self.prefetching = None;
        self.fetching = false;
        self.batch_gen += 1;
        self.starting_wave = false;
        self.retry_at = None;
        self.batch_id.clear();
        self.session.clear();
    }

    fn reset(&mut self) {
        self.account_generation += 1;
        self.pending_likes.clear();
        self.local_likes.clear();
        self.stop_audio();
        if cfg!(windows) {
            self.audio = None;
            self.paused = true;
        }
        self.gen += 1;
        self.clear_queue();
        self.history.clear();
        self.played.clear();
        self.played_station.clear();
        self.wave_restored = false;
        self.current = None;
        self.current_bytes = None;
        self.list = None;
        self.source = Source::Wave;
        let mut s = self.sh();
        s.track = None;
        s.source = Source::Wave;
        s.loading = false;
        s.audio_output_error = None;
    }

    fn set_source(&mut self, src: Source) {
        self.source = src.clone();
        self.sh().source = src;
    }

    fn active_repeat(&self) -> Repeat {
        if self.source.station().is_some() { Repeat::Off } else { self.repeat }
    }

    /// Состояние движка → Shared для UI.
    fn sync(&mut self) {
        let playing = self.loaded && !self.paused && self.sink().map(|s| !s.is_paused()).unwrap_or(false);
        let pos = if self.current.is_some() { self.pos() } else { 0.0 };
        let upcoming: Vec<Track> = self.queue.iter().take(30).cloned().collect();
        let mut s = self.shared.lock().unwrap();
        s.playing = playing;
        s.paused = self.paused;
        s.position = pos;
        s.volume = self.volume;
        s.repeat = self.active_repeat();
        s.shuffle = self.shuffle;
        s.upcoming = upcoming;
    }

    // ---------- главный цикл ----------

    fn tick(&mut self) {
        #[cfg(windows)]
        if Instant::now() >= self.next_output_check {
            self.next_output_check = Instant::now() + Duration::from_millis(750);
            let position = self.sink.as_ref().filter(|_| self.loaded && !self.paused).map(Sink::get_pos);
            if self.audio.as_mut().is_some_and(|out| out.needs_reopen(position)) {
                self.change_output(self.asio_driver.clone(), false);
                self.sync();
                (self.notify)();
            }
        }
        if let Some(at) = self.retry_at {
            if Instant::now() >= at {
                self.retry_at = None;
                self.fetch_batch();
            }
        }
        if self.loaded && !self.paused && self.sink().map(|s| s.empty()).unwrap_or(false) {
            if self.active_repeat() == Repeat::One && self.replay() {
                return;
            }
            self.advance(End::Finished);
            self.sync();
            (self.notify)();
            return;
        }
        self.sync();
        if self.loaded && self.prefetch.is_none() && self.prefetching.is_none() {
            self.prefetch_next();
        }
        let playing = self.sh().playing;
        if playing != self.was_playing {
            self.was_playing = playing;
            (self.notify)();
        }
    }

    fn replay(&mut self) -> bool {
        let Some(bytes) = self.current_bytes.clone() else { return false };
        self.attach(bytes, Duration::ZERO).is_ok()
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Cmd(c) => self.cmd(c),
            Msg::Account(gen, _) | Msg::Likes(gen, _) if gen != self.account_generation => {}
            Msg::Account(gen, res) => match res {
                Ok(acc) => {
                    self.uid = acc.uid.clone();
                    {
                        let mut s = self.sh();
                        s.status = Status::Ready;
                        s.uid = acc.uid.clone();
                        s.login = acc.login;
                        s.has_plus = acc.has_plus;
                        s.error = None;
                    }
                    self.feedback("radioStarted", None, None);
                    self.fetch_batch();
                    if let Some(api) = self.api.clone() {
                        let tx = self.tx.clone();
                        let uid = acc.uid;
                        thread::spawn(move || {
                            let _ = tx.send(Msg::Likes(gen, api.liked_ids(&uid)));
                        });
                    }
                }
                Err(e) => {
                    log(format!("account: {e}"));
                    self.api = None;
                    self.sh().status = Status::Failed(e);
                }
            },
            Msg::Likes(_, res) => match res {
                Ok(ids) => self.merge_likes(ids),
                Err(e) => log(format!("likes: {e}")),
            },
            Msg::Batch { res, gen } => {
                if gen != self.batch_gen {
                    return;
                }
                self.fetching = false;
                match res {
                    Ok(b) => self.on_batch(b, crate::api::allow_explicit()),
                    Err(e) => {
                        self.error(format!("Не удалось получить треки волны: {e}"));
                        self.retry_at = Some(Instant::now() + Duration::from_secs(5));
                    }
                }
            }
            Msg::Audio { gen, res } => {
                if gen == self.gen {
                    match res {
                        Ok(b) => self.play_loaded(b.into()),
                        Err(e) => self.on_load_error(e),
                    }
                }
            }
            Msg::Prefetch { generation, id, res } => {
                if generation != self.account_generation { return; }
                if self.prefetching.as_deref() == Some(id.as_str()) {
                    self.prefetching = None;
                }
                let waiting = !self.loaded && self.current.as_ref().map(|t| t.id == id).unwrap_or(false);
                match res {
                    Ok(b) if waiting => self.play_loaded(b.into()),
                    Ok(b) => self.prefetch = Some((id, b)),
                    Err(e) => {
                        log(format!("prefetch {id}: {e}"));
                        if waiting {
                            self.start_download();
                        }
                    }
                }
            }
            Msg::LikeSaved { id, seq, liked, result } => {
                let Some(pending) = self.pending_likes.get_mut(&id) else { return };
                if seq < pending.first { return; }
                if result.is_ok() {
                    pending.confirmed = liked;
                    self.local_likes.insert(id.clone());
                }
                if seq == pending.latest {
                    let confirmed = pending.confirmed;
                    self.pending_likes.remove(&id);
                    {
                        let mut likes = self.likes.lock().unwrap();
                        if confirmed { likes.insert(id.clone()); } else { likes.remove(&id); }
                    }
                    self.sh().changed_likes.insert(id);
                }
                if let Err(e) = result {
                    let message = format!("Лайк не сохранился: {e}");
                    log(&message);
                    self.sh().like_error = Some(message);
                }
            }
            Msg::Error(e) => self.error(e),
        }
    }

    fn cmd(&mut self, c: Cmd) {
        match c {
            #[cfg(all(windows, feature = "asio"))]
            Cmd::Output(driver, reply) => {
                self.change_output(driver, true);
                let error = self.sh().audio_output_error.take().map(|(message, _)| message);
                reply(self.asio_driver.clone(), error);
            }
            Cmd::Login(token) => {
                self.reset();
                let api = Api::new(&token);
                self.api = Some(api.clone());
                {
                    let mut s = self.sh();
                    s.status = Status::Connecting;
                    s.error = None;
                }
                let tx = self.tx.clone();
                let gen = self.account_generation;
                thread::spawn(move || {
                    let _ = tx.send(Msg::Account(gen, api.account()));
                });
            }
            Cmd::Logout => {
                self.reset();
                self.api = None;
                self.uid.clear();
                self.likes.lock().unwrap().clear();
                let mut s = self.sh();
                s.status = Status::LoggedOut;
                s.login.clear();
                s.uid.clear();
            }
            Cmd::Resume => self.resume(),
            Cmd::Pause => self.pause(),
            Cmd::PlayPause => {
                if self.paused || (self.audio.is_none() && self.current_bytes.is_some()) {
                    self.resume();
                } else {
                    self.pause();
                }
            }
            Cmd::Next => {
                self.errors_in_row = 0;
                if self.current.is_some() {
                    self.advance(End::Skip);
                }
            }
            Cmd::Prev => self.prev(),
            #[cfg(target_os = "linux")]
            Cmd::SeekTrack(id, micros) => {
                if self.current.as_ref().is_some_and(|t| crate::mpris::can_set_position(t, &id, micros)) {
                    self.cmd(Cmd::Seek(micros as f32 / 1_000_000.0));
                }
            }
            #[cfg(target_os = "linux")]
            Cmd::SeekBy(offset) => self.cmd(Cmd::Seek(self.pos() + offset)),
            Cmd::Seek(sec) => self.seek(Duration::from_secs_f32(sec.max(0.0))),
            Cmd::Volume(v) => {
                self.volume = v.clamp(0.0, 1.0);
                if let Some(s) = self.sink() {
                    s.set_volume(self.volume * self.volume);
                }
            }
            Cmd::LikeTrack(t) => self.toggle_like(t),
            Cmd::Dislike => {
                if let (Some(api), Some(t)) = (self.api.clone(), self.current.as_ref()) {
                    let id = t.full_id();
                    let uid = self.uid.clone();
                    let tx = self.tx.clone();
                    let _ = self.bg.send(Box::new(move || {
                        if let Err(e) = api.dislike(&uid, &id) {
                            let _ = tx.send(Msg::Error(format!("Дизлайк не сохранился: {e}")));
                        }
                    }));
                }
                self.advance(End::Dislike);
            }
            Cmd::Wave(src) => self.start_wave(src),
            Cmd::PlayList { source, mut tracks, start, after } => {
                for (index, track) in tracks.iter_mut().enumerate() { track.list_position = Some(index); }
                if tracks.is_empty() {
                    return;
                }
                if let (Some(cur), true) = (self.current.clone(), self.loaded) {
                    self.feedback("skip", Some(&cur), Some(self.pos() as f64));
                }
                self.clear_queue();
                let start = start.min(tracks.len() - 1);
                let mut order: Vec<Track> = tracks.clone();
                order.rotate_left(start);
                let first = order.remove(0);
                if self.shuffle {
                    shuffle(&mut order);
                }
                self.queue.push_back(first);
                self.queue.extend(order);
                self.list = Some(ListPlay { deferred: VecDeque::new(), all: tracks, after });
                self.set_source(source);
                self.advance(End::Skip);
            }
            Cmd::Jump(i) => {
                if i >= self.queue.len() {
                    return;
                }
                let skipped: Vec<Track> = self.queue.drain(..i).collect();
                if let Some(cur) = self.current.take() {
                    if self.loaded {
                        self.feedback("skip", Some(&cur), Some(self.pos() as f64));
                    }
                    self.history.push(cur);
                }
                self.history.extend(skipped);
                self.trim_history();
                self.stop_audio();
                self.play_from_queue();
                self.refresh_wave();
                self.flush_feedback();
            }
            Cmd::Repeat(r) => self.repeat = r,
            Cmd::Enqueue { mut track, next } => {
                track.list_position = None;
                if next {
                    self.queue.push_front(track);
                    self.prefetch = None;
                    if self.loaded {
                        self.prefetch_next();
                    }
                } else {
                    self.queue.push_back(track);
                }
            }
            Cmd::AllowExplicit(allow) => self.set_allow_explicit(allow),
            Cmd::Shuffle(on) => {
                self.shuffle = on;
                if let Some(list) = &mut self.list {
                    if let Some((first, tail)) = list.deferred.make_contiguous().split_first_mut() {
                        if on {
                            shuffle(tail);
                        } else {
                            restore_queue_order(tail, first.list_position.map_or(0, |i| i + 1));
                        }
                    }
                    let mut v: Vec<Track> = self.queue.drain(..).collect();
                    if on {
                        shuffle(&mut v);
                    } else if let Some(cur) = &self.current {
                        let i = cur.list_position.map_or(0, |i| (i + 1) % list.all.len());
                        restore_queue_order(&mut v, i);
                    }
                    self.queue.extend(v);
                    self.prefetch = None;
                    if self.loaded {
                        self.prefetch_next();
                    }
                }
            }
        }
    }

    fn merge_likes(&mut self, ids: Vec<String>) {
        let mut likes = self.likes.lock().unwrap();
        for id in ids {
            if self.local_likes.contains(&id) { continue; }
            if let Some(pending) = self.pending_likes.get_mut(&id) {
                pending.confirmed = true;
            } else if likes.insert(id.clone()) {
                self.shared.lock().unwrap().changed_likes.insert(id);
            }
        }
    }

    fn toggle_like(&mut self, t: Track) {
        let Some(api) = self.api.clone() else { return };
        let remove = {
            let mut l = self.likes.lock().unwrap();
            let had = l.remove(&t.id);
            if !had {
                l.insert(t.id.clone());
            }
            had
        };
        self.sh().changed_likes.insert(t.id.clone());
        self.like_seq += 1;
        let seq = self.like_seq;
        self.pending_likes.entry(t.id.clone()).or_insert(PendingLike {
            first: seq, latest: seq, confirmed: remove,
        }).latest = seq;
        let (uid, id, full_id, tx) = (self.uid.clone(), t.id.clone(), t.full_id(), self.tx.clone());
        let _ = self.bg.send(Box::new(move || {
            let result = api.like(&uid, &full_id, remove);
            let _ = tx.send(Msg::LikeSaved { id, seq, liked: !remove, result });
        }));
        // Лайк трека из волны оригинал сообщает и самой волне, чтобы она его учла.
        if self.session.is_empty() {
            return;
        }
        let in_wave = self
            .current
            .iter()
            .chain(self.queue.iter())
            .chain(self.history.iter())
            .find(|w| w.id == t.id && !w.batch_id.is_empty())
            .cloned();
        if let Some(w) = in_wave {
            self.feedback(if remove { "unlike" } else { "like" }, Some(&w), None);
        }
    }

    // ---------- очередь ----------

    fn start_wave(&mut self, src: Source) {
        self.clear_queue();
        self.list = None;
        self.set_source(src);
        self.starting_wave = true;
        self.feedback("radioStarted", None, None);
        self.fetch_batch();
    }

    fn fetch_batch(&mut self) {
        if self.fetching || self.retry_at.is_some() || self.list.is_some() {
            return;
        }
        let Some(station) = self.source.station().map(String::from) else { return };
        let Some(api) = self.api.clone() else { return };
        self.fetching = true;
        if self.played_station != station {
            self.played_station = station.clone();
            self.played.clear();
            if station == WAVE {
                self.restore_wave();
            }
        }
        // queue, как у оригинала: всё, что уже звучало в этой волне, и текущий трек.
        // Ещё не сыгранные треки сюда не попадают.
        let mut queue = self.played.clone();
        if let Some(c) = &self.current {
            let id = c.full_id();
            if queue.last() != Some(&id) {
                queue.push(id);
            }
        }
        let session = self.session.clone();
        let feedbacks = std::mem::take(&mut self.deferred);
        let tx = self.tx.clone();
        let gen = self.batch_gen;
        // В очереди фонового потока: запрос уйдёт после уже отправленных фидбеков.
        let _ = self.bg.send(Box::new(move || {
            let res = api.wave_session(&station, Some(&session), &queue, &feedbacks);
            let _ = tx.send(Msg::Batch { res, gen });
        }));
    }

    fn on_batch(&mut self, b: Batch, allow_explicit: bool) {
        self.batch_id = b.batch_id;
        if !b.session.is_empty() {
            // Сессия могла подхватиться с прошлого запуска — radioStarted нужен и ей.
            let changed = b.session != self.session;
            self.session = b.session;
            if std::mem::take(&mut self.radio_started_pending) {
                self.feedback("radioStarted", None, None);
            }
            if changed {
                self.save_wave();
            }
        }
        self.merge_likes(b.liked);
        let replace = self.starting_wave;
        if !allow_explicit {
            self.queue.retain(|t| !t.explicit);
        }
        let my_wave = self.source.station() == Some(WAVE);
        let mut seen: HashSet<&str> = self.queue.iter().map(|t| t.id.as_str()).collect();
        if my_wave || !replace {
            seen.extend(self.current.iter().map(|t| t.id.as_str()));
        }
        let mut played: HashSet<&str> = HashSet::new();
        if my_wave || !replace {
            played.extend(self.history.iter().map(|t| t.id.as_str()));
            played.extend(self.played.iter().map(|id| id.split(':').next().unwrap_or(id)));
        }
        let (fresh, recent): (Vec<_>, Vec<_>) = b.tracks.iter()
            .filter(|t| (allow_explicit || !t.explicit) && seen.insert(&t.id))
            .partition(|t| !played.contains(t.id.as_str()));
        let tracks = if !fresh.is_empty() {
            fresh
        } else if !self.queue.is_empty() {
            // Уже есть что играть. Следующий переход снова пополнит очередь.
            Vec::new()
        } else if self.stale_batches < 2 {
            self.stale_batches += 1;
            self.fetch_batch();
            return;
        } else if !my_wave && !recent.is_empty() {
            // У узкой станции (например, по одному исполнителю) может закончиться каталог.
            recent
        } else {
            if my_wave && self.current.is_none() && self.stale_batches == 2 && !self.session.is_empty() {
                // Одинаковая история в той же сессии может давать один и тот же пакет.
                self.flush_feedback();
                self.session.clear();
                self.radio_started_pending = true;
                self.stale_batches += 1;
                self.error("В подборке нет подходящих треков. Обновляю Волну.");
            }
            self.retry_at = Some(Instant::now() + Duration::from_secs(5));
            return;
        };
        self.stale_batches = 0;
        self.queue.extend(tracks.into_iter().cloned());
        if replace {
            self.advance(End::Restart);
        } else if self.current.is_none() {
            self.play_from_queue();
        } else if self.loaded {
            self.prefetch_next();
        }
    }

    fn trim_history(&mut self) {
        if self.history.len() > 50 {
            let n = self.history.len() - 50;
            self.history.drain(..n);
        }
    }

    fn advance(&mut self, end: End) {
        if let Some(cur) = self.current.take() {
            if self.loaded {
                let played = self.pos() as f64;
                let kind = match end {
                    End::Finished => "trackFinished",
                    End::Skip | End::Restart => "skip",
                    End::Dislike => "dislike",
                };
                self.feedback(kind, Some(&cur), Some(played));
            }
            self.history.push(cur);
            self.trim_history();
        }
        self.stop_audio();
        self.play_from_queue();
        if matches!(end, End::Skip | End::Dislike) {
            self.refresh_wave();
        }
        self.flush_feedback();
    }

    /// Пропуск в волне, как у оригинала: ещё не сыгранные треки волны убираем и сразу просим
    /// новые, а пропуск уходит в том же запросе. Так волна подстраивается сразу, а не через пакет.
    /// Треки, добавленные вручную, и уже сыгранные (после «назад») остаются.
    fn refresh_wave(&mut self) {
        if self.list.is_some() || self.source.station().is_none() || self.session.is_empty() {
            return;
        }
        let played = &self.played;
        self.queue
            .retain(|t| t.batch_id.is_empty() || played.iter().any(|p| p.split(':').next() == Some(t.id.as_str())));
        if let Some((id, _)) = &self.prefetch {
            if !self.queue.iter().any(|t| t.id == *id) {
                self.prefetch = None;
            }
        }
        self.fetch_batch();
    }

    /// Список закончился: по кругу (исполнитель) или волна по нему (плейлист, альбом).
    fn list_finished(&mut self) {
        let Some(l) = self.list.take() else { return };
        match l.after {
            After::Loop => {
                let mut all = l.all.clone();
                if self.shuffle {
                    shuffle(&mut all);
                }
                self.queue.extend(all);
                self.list = Some(l);
            }
            After::Wave(src) => {
                self.flush_feedback();
                self.set_source(src);
                self.session.clear();
                self.feedback("radioStarted", None, None);
            }
        }
    }

    /// Как у оригинала при смене настроек волны: текущий трек доигрывает (если он не 18+, а их
    /// выключили), а несыгранные треки волны заменяет новая сессия уже с этим флагом.
    fn set_allow_explicit(&mut self, allow: bool) {
        crate::api::ALLOW_EXPLICIT.store(allow, std::sync::atomic::Ordering::Relaxed);
        let wave = self.list.is_none() && self.source.station().is_some();
        let skip_current = !allow && self.current.as_ref().is_some_and(|t| t.explicit);
        if wave {
            self.queue.retain(|t| t.batch_id.is_empty());
        }
        if !allow {
            if self.blocked_list() {
                if skip_current {
                    self.queue.push_front(self.current.as_ref().unwrap().clone());
                }
                self.defer_explicit();
            } else {
                self.queue.retain(|t| !t.explicit);
            }
        }
        self.prefetch = None;
        if skip_current {
            if let Some(cur) = self.current.take() {
                if self.loaded {
                    self.feedback("skip", Some(&cur), Some(self.pos() as f64));
                }
                self.history.push(cur);
                self.trim_history();
            }
            self.stop_audio();
        }
        if wave {
            self.flush_feedback();
            self.session.clear();
            self.fetching = false;
            self.retry_at = None;
            self.stale_batches = 0;
            self.batch_gen += 1;
        }
        if skip_current {
            self.play_from_queue();
        }
        if wave {
            self.fetch_batch();
        }
    }

    fn blocked_list(&self) -> bool {
        !crate::api::allow_explicit()
            && self.list.as_ref().is_some_and(|list| !list.all.is_empty() && list.all.iter().all(|t| t.explicit))
    }

    fn defer_explicit(&mut self) {
        if let Some(list) = &mut self.list {
            self.queue.retain(|track| {
                if track.explicit { list.deferred.push_back(track.clone()); }
                !track.explicit
            });
        }
    }

    fn play_from_queue(&mut self) {
        let blocked_list = self.blocked_list();
        if blocked_list {
            self.defer_explicit();
        } else if crate::api::allow_explicit() {
            if let Some(list) = &mut self.list { self.queue.append(&mut list.deferred); }
        } else {
            self.queue.retain(|t| !t.explicit);
        }
        if self.queue.is_empty() && self.list.is_some() && !blocked_list {
            match (&self.list, self.repeat) {
                (Some(l), Repeat::All) => {
                    let all = l.all.clone();
                    self.queue.extend(all);
                }
                _ => self.list_finished(),
            }
        }
        if !crate::api::allow_explicit() {
            self.queue.retain(|t| !t.explicit);
        }
        match self.queue.pop_front() {
            Some(t) => {
                self.current = Some(t);
                self.start_current();
            }
            None => {
                self.current = None;
                self.current_bytes = None;
                if self.list.is_some() { self.paused = true; }
                self.gen += 1;
                let mut s = self.sh();
                s.track = None;
                s.loading = self.list.is_none() && self.source.station().is_some();
            }
        }
        if self.queue.len() < 2 {
            self.fetch_batch();
        }
    }

    fn prev(&mut self) {
        if self.loaded && self.pos() > 4.0 {
            self.seek(Duration::ZERO);
            return;
        }
        if !crate::api::allow_explicit() {
            self.history.retain(|t| !t.explicit);
        }
        if let Some(p) = self.history.pop() {
            if let Some(c) = self.current.take() {
                if self.loaded {
                    self.feedback("skip", Some(&c), Some(self.pos() as f64));
                }
                self.queue.push_front(c);
            }
            self.stop_audio();
            self.current = Some(p);
            self.start_current();
        } else if self.loaded {
            self.seek(Duration::ZERO);
        }
    }

    // ---------- загрузка ----------

    fn start_current(&mut self) {
        self.starting_wave = false;
        self.position_base = Duration::ZERO;
        self.gen += 1;
        self.loaded = false;
        self.current_bytes = None;
        let Some(t) = self.current.clone() else { return };
        {
            let mut s = self.sh();
            s.playing = false;
            s.paused = self.paused;
            s.track = Some(t.clone());
            s.position = 0.0;
            s.duration = t.duration_ms as f32 / 1000.0;
            s.loading = true;
        }
        (self.notify)();
        if self.prefetch.as_ref().map(|(id, _)| *id == t.id).unwrap_or(false) {
            let (_, b) = self.prefetch.take().unwrap();
            self.play_loaded(b.into());
            return;
        }
        if self.prefetching.as_deref() == Some(t.id.as_str()) {
            return;
        }
        self.start_download();
    }

    fn start_download(&mut self) {
        let (Some(api), Some(t)) = (self.api.clone(), self.current.clone()) else { return };
        let gen = self.gen;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = load_track(&api, &t);
            let _ = tx.send(Msg::Audio { gen, res });
        });
    }

    fn prefetch_next(&mut self) {
        // Следующий трек качаем под конец текущего: не держать в памяти два файла всё время.
        let near_end = {
            let s = self.shared.lock().unwrap();
            s.duration > 0.0 && s.duration - s.position <= 45.0
        };
        if !near_end {
            return;
        }
        let Some(next) = self.queue.front().cloned() else { return };
        let have = self.prefetch.as_ref().map(|(id, _)| *id == next.id).unwrap_or(false);
        if have || self.prefetching.is_some() {
            return;
        }
        let Some(api) = self.api.clone() else { return };
        let generation = self.account_generation;
        self.prefetching = Some(next.id.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = load_track(&api, &next);
            let _ = tx.send(Msg::Prefetch { generation, id: next.id, res });
        });
    }

    fn pause(&mut self) {
        self.paused = true;
        if let Some(sink) = self.sink() { sink.pause(); }
    }

    fn resume(&mut self) {
        self.paused = false;
        if let Some(sink) = self.sink() {
            sink.play();
        } else if let Some(bytes) = self.current_bytes.clone() {
            if self.loaded {
                if let Err(e) = self.ensure_audio().and_then(|_| self.attach(bytes, self.position_base)) {
                    self.paused = true;
                    self.audio = None;
                    self.output_error(e);
                } else {
                    self.sh().audio_output_error = None;
                }
            } else {
                self.play_loaded(bytes);
            }
        } else if self.current.is_none() && self.list.is_some() {
            self.play_from_queue();
        } else if self.current.is_none() && self.sh().status == Status::Ready && self.source.station().is_some() {
            self.sh().loading = true;
            self.fetch_batch();
        }
    }

    fn play_loaded(&mut self, bytes: Arc<[u8]>) {
        let Some(t) = self.current.clone() else { return };
        if cfg!(windows) && self.paused && self.audio.is_none() {
            self.current_bytes = Some(bytes);
            self.sh().loading = false;
            return;
        }
        if let Err(e) = self.ensure_audio() {
            self.current_bytes = Some(bytes);
            self.paused = true;
            self.sh().loading = false;
            self.output_error(e);
            return;
        }
        if let Err(e) = self.attach(bytes.clone(), self.position_base) {
            self.on_load_error(e);
            return;
        }
        self.current_bytes = Some(bytes);
        self.loaded = true;
        self.errors_in_row = 0;
        {
            let mut shared = self.sh();
            shared.loading = false;
            shared.audio_output_error = None;
        }
        self.feedback("trackStarted", Some(&t), None);
        self.remember_played(&t);
        self.prefetch_next();
    }

    // ---------- память волны ----------

    fn remember_played(&mut self, t: &Track) {
        let Some(station) = self.source.station() else { return };
        if station != self.played_station {
            return;
        }
        let id = t.full_id();
        self.played.retain(|p| *p != id);
        self.played.push(id);
        if self.played.len() > 100 {
            let n = self.played.len() - 100;
            self.played.drain(..n);
        }
        self.save_wave();
    }

    /// «Моя волна» после перезапуска продолжается с той же сессии и тем же списком сыгранного.
    /// Иначе сервер начинает с чистого листа и выдаёт почти то же, что в прошлый раз.
    fn restore_wave(&mut self) {
        let first = !std::mem::replace(&mut self.wave_restored, true);
        let Some(w) = load_wave_state() else { return };
        if w.uid != self.uid {
            return;
        }
        self.played = w.played;
        // Сессию берём только при запуске; если сервер её уже закрыл, wave_session начнёт новую.
        let fresh = crate::config::now_secs().saturating_sub(w.at) < WAVE_SESSION_TTL;
        if first && fresh && self.session.is_empty() {
            self.session = w.session;
        }
    }

    fn save_wave(&self) {
        if self.played_station != WAVE || self.source.station() != Some(WAVE) || self.uid.is_empty() {
            return;
        }
        let w = serde_json::json!({
            "uid": self.uid,
            "session": self.session,
            "played": self.played,
            "at": crate::config::now_secs(),
        });
        let _ = self.bg.send(Box::new(move || {
            let path = crate::config::dir().join("wave.json");
            if let Err(e) = std::fs::write(&path, w.to_string()) {
                log(format!("wave.json: {e}"));
            }
        }));
    }

    fn on_load_error(&mut self, e: String) {
        self.errors_in_row += 1;
        let name = self.current.as_ref().map(|t| t.title.clone()).unwrap_or_default();
        self.error(format!("«{name}» не загрузился: {e}"));
        if self.errors_in_row < 5 {
            self.current = None;
            self.stop_audio();
            self.play_from_queue();
        } else {
            self.sh().loading = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave_engine() -> Engine {
        let (tx, _) = mpsc::channel();
        Engine::new(tx, Arc::default(), Arc::default(), Arc::new(|| {}), Arc::default(), None)
    }

    fn wave_batch(ids: &[&str]) -> Batch {
        let tracks = ids.iter().map(|id| {
            crate::api::parse_track(&serde_json::json!({"id": id, "title": id})).unwrap()
        }).collect();
        Batch { tracks, liked: vec![], batch_id: "batch".into(), session: "station".into() }
    }

    #[test]
    fn wave_never_requeues_recent_tracks_after_repeated_batches() {
        let mut engine = wave_engine();
        engine.played = vec!["old:album".into()];
        engine.current = wave_batch(&["current"]).tracks.pop();
        for _ in 0..3 {
            engine.on_batch(wave_batch(&["old", "current"]), true);
        }
        assert!(engine.queue.is_empty());
        assert!(engine.retry_at.is_some());
        engine.on_batch(wave_batch(&["old", "new", "new", "another"]), true);
        let queued: Vec<_> = engine.queue.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(queued, ["new", "another"]);
    }

    #[test]
    fn starting_my_wave_waits_for_a_different_song() {
        let mut engine = wave_engine();
        engine.current = wave_batch(&["current"]).tracks.pop();
        engine.start_wave(Source::Wave);
        for _ in 0..3 {
            engine.on_batch(wave_batch(&["current"]), true);
        }
        assert!(engine.retry_at.unwrap() > Instant::now());
        engine.retry_at = Some(Instant::now());
        engine.tick();
        assert!(engine.retry_at.is_none());
        engine.handle(Msg::Batch { res: Ok(wave_batch(&["next"])), gen: engine.batch_gen });
        assert_eq!(engine.current.as_ref().unwrap().id, "next");
    }

    #[test]
    fn wave_keeps_a_manual_queue_entry_when_the_batch_duplicates_it() {
        let mut engine = wave_engine();
        engine.current = wave_batch(&["current"]).tracks.pop();
        engine.queue.extend(wave_batch(&["manual"]).tracks);
        engine.on_batch(wave_batch(&["manual", "manual"]), true);
        assert_eq!(engine.queue.len(), 1);
        assert_eq!(engine.queue[0].id, "manual");
        assert!(engine.retry_at.is_none());
    }

    #[test]
    fn restarting_wave_preserves_play_next_during_delayed_retry() {
        let mut engine = wave_engine();
        engine.current = wave_batch(&["current"]).tracks.pop();
        engine.start_wave(Source::Wave);
        engine.handle(Msg::Batch { res: Err("offline".into()), gen: engine.batch_gen });
        engine.retry_at = Some(Instant::now());
        engine.tick();
        let track = wave_batch(&["manual"]).tracks.pop().unwrap();
        engine.cmd(Cmd::Enqueue { track, next: true });
        engine.on_batch(wave_batch(&["next"]), true);
        assert_eq!(engine.current.as_ref().unwrap().id, "manual");
        assert_eq!(engine.queue.front().unwrap().id, "next");
    }

    #[test]
    fn wave_requests_a_new_session_if_the_empty_queue_keeps_getting_repeats() {
        let mut engine = wave_engine();
        engine.played.push("old:album".into());
        for _ in 0..3 {
            engine.on_batch(wave_batch(&["old"]), true);
        }
        assert!(engine.session.is_empty());
        assert!(engine.radio_started_pending);
        assert!(engine.retry_at.is_some());
        assert_eq!(engine.played, ["old:album"]);
        engine.on_batch(wave_batch(&["fresh"]), true);
        assert_eq!(engine.current.unwrap().id, "fresh");
    }

    #[test]
    fn blocked_manual_track_cannot_interrupt_a_pending_wave_start() {
        let mut engine = wave_engine();
        engine.current = wave_batch(&["current"]).tracks.pop();
        engine.start_wave(Source::Wave);
        let mut blocked = wave_batch(&["blocked"]).tracks.pop().unwrap();
        blocked.explicit = true;
        engine.cmd(Cmd::Enqueue { track: blocked, next: true });
        engine.on_batch(wave_batch(&["current"]), false);
        assert_eq!(engine.current.as_ref().unwrap().id, "current");
        assert!(engine.starting_wave);
    }

    #[test]
    fn wave_suspends_single_track_repeat_from_a_list() {
        let mut engine = wave_engine();
        let album = Source::List { kind: ListKind::Album, id: "1".into(), title: "Album".into() };
        engine.set_source(album.clone());
        engine.cmd(Cmd::Repeat(Repeat::One));
        engine.start_wave(Source::Wave);
        engine.sync();
        assert_eq!(engine.sh().repeat, Repeat::Off);
        engine.set_source(album);
        engine.sync();
        assert_eq!(engine.sh().repeat, Repeat::One);
    }


    #[test]
    fn late_account_reply_after_logout_keeps_media_closed() {
        let (tx, _) = mpsc::channel();
        let mut engine = Engine::new(tx, Arc::default(), Arc::default(), Arc::new(|| {}), Arc::default(), None);
        let gen = engine.account_generation;
        engine.cmd(Cmd::Logout);
        engine.handle(Msg::Account(gen, Ok(Account { uid: "old".into(), login: "old".into(), has_plus: false })));
        assert_eq!(engine.sh().status, Status::LoggedOut);
    }

    #[test]
    fn first_play_keeps_pending_wave_and_reports_loading() {
        let (tx, _) = mpsc::channel();
        let mut engine = Engine::new(tx, Arc::default(), Arc::default(), Arc::new(|| {}), Arc::default(), None);
        engine.sh().status = Status::Ready;
        engine.fetching = true;
        let generation = engine.batch_gen;
        engine.cmd(Cmd::Resume);
        engine.sync();
        assert_eq!(engine.batch_gen, generation);
        assert!(engine.sh().loading);
        assert!(!engine.sh().paused);
    }

    #[test]
    fn explicit_pause_and_play_are_idempotent_during_download() {
        let (tx, _) = mpsc::channel();
        let mut engine = Engine::new(tx, Arc::default(), Arc::default(), Arc::new(|| {}), Arc::default(), None);
        engine.sh().loading = true;
        for (cmd, paused) in [(Cmd::Resume, false), (Cmd::Resume, false), (Cmd::Pause, true), (Cmd::Pause, true)] {
            engine.cmd(cmd);
            engine.sync();
            let state = engine.sh();
            assert_eq!(state.paused, paused);
            assert_eq!(state.can_pause(), !paused);
            assert!(!state.playing);
            assert!(state.loading);
        }
    }

    #[cfg(windows)]
    #[test]
    fn pause_during_first_download_keeps_buffered_audio_silent() {
        let (tx, _) = mpsc::channel();
        let mut engine = Engine::new(tx, Arc::default(), Arc::default(), Arc::new(|| {}), Arc::default(), None);
        engine.current = crate::api::parse_track(&serde_json::json!({"id": "song", "title": "Song"}));
        engine.cmd(Cmd::Resume);
        engine.cmd(Cmd::Pause);
        engine.handle(Msg::Audio { gen: engine.gen, res: Ok(vec![0; 8]) });
        engine.sync();
        assert!(engine.current_bytes.is_some());
        assert!(engine.audio.is_none());
        let state = engine.sh();
        assert!(state.paused && !state.playing && !state.loading);
    }

    #[test]
    fn next_track_notifies_with_text_before_audio_is_ready() {
        let (tx, _) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let observed = shared.clone();
        let (events, rx) = mpsc::channel();
        let notify = Arc::new(move || {
            let state = observed.lock().unwrap();
            if let Some(track) = &state.track {
                events.send((track.title.clone(), track.artists.clone(), state.loading)).unwrap();
            }
        });
        let mut engine = Engine::new(tx, shared, Arc::default(), notify, Arc::default(), None);
        engine.current = crate::api::parse_track(&serde_json::json!({
            "id": "2", "title": "Новый трек", "artists": [{ "id": "3", "name": "Исполнитель" }]
        }));
        engine.start_current();
        assert_eq!(rx.try_recv(), Ok(("Новый трек".into(), "Исполнитель".into(), true)));
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn audio_wait_dispatches_windows_messages() {
        use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging::{KillTimer, SetTimer}};
        thread_local! { static DISPATCHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
        unsafe extern "system" fn on_timer(window: HWND, _: u32, id: usize, _: u32) {
            unsafe { KillTimer(window, id); }
            DISPATCHED.set(true);
        }
        let (tx, rx) = mpsc::channel();
        let tx = AudioSender::from(tx);
        let timer = unsafe { SetTimer(std::ptr::null_mut(), 0, 10, Some(on_timer)) };
        assert_ne!(timer, 0);
        assert!(matches!(tx.recv(&rx, Duration::from_millis(200)), Err(RecvTimeoutError::Timeout)));
        assert!(DISPATCHED.get());
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn missing_asio_driver_returns_an_error() {
        let name = "yamusic-rs test ASIO driver that cannot be registered under this name";
        let Err(error) = open_output(Some(name)) else {
            panic!("an unavailable ASIO driver opened an audio stream");
        };
        assert!(error.contains(name));
        assert!(error.contains("драйвер не найден"));
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn asio_waits_for_play_and_retries_the_buffered_track() {
        let (tx, _) = mpsc::channel();
        let mut engine = Engine::new(
            tx,
            Arc::default(),
            Arc::default(),
            Arc::new(|| {}),
            Arc::default(),
            Some("yamusic-rs missing ASIO driver".into()),
        );
        engine.current = crate::api::parse_track(&serde_json::json!({ "id": "1", "title": "Тест" }));
        assert!(engine.current.is_some());
        engine.play_loaded(Arc::from([0; 4]));
        assert!(engine.sh().audio_output_error.is_none(), "driver opened before Play");
        assert!(engine.current_bytes.is_some());

        engine.cmd(Cmd::Resume);
        assert!(engine.sh().audio_output_error.is_some());
        engine.sh().audio_output_error = None;
        engine.cmd(Cmd::PlayPause);
        assert!(engine.sh().audio_output_error.is_some(), "Play did not retry opening the driver");
    }

    #[cfg(all(windows, feature = "asio"))]
    fn test_engine(driver: Option<String>) -> Engine {
        let (tx, _) = mpsc::channel();
        let mut engine = Engine::new(tx, Arc::default(), Arc::default(), Arc::new(|| {}), Arc::default(), driver);
        engine.current = crate::api::parse_track(&serde_json::json!({ "id": "1", "title": "Тест" }));
        engine.volume = 0.0;
        engine
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn system_prefetch_does_not_claim_an_output_before_play() {
        let mut engine = test_engine(None);
        // До Play сохраняем данные, оставляя выход свободным.
        engine.play_loaded(Arc::from([0]));
        assert!(engine.audio.is_none());
        assert!(engine.current_bytes.is_some());
        assert!(!engine.loaded);
        assert!(engine.sh().audio_output_error.is_none());
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn paused_prefetch_remembers_a_seek_before_first_play() {
        let mut engine = test_engine(None);
        engine.play_loaded(Arc::from([0]));
        engine.cmd(Cmd::Seek(7.0));
        engine.sync();
        assert_eq!(engine.sh().position, 7.0);
        assert!(engine.audio.is_none());
        assert!(!engine.loaded);
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn failed_manual_selection_replies_while_download_is_still_pending() {
        let mut engine = test_engine(Some("missing previous driver".into()));
        engine.sh().loading = true;
        let (tx, rx) = mpsc::channel();
        engine.cmd(Cmd::Output(Some("missing requested driver".into()), Box::new(move |active, error| {
            tx.send((active, error)).unwrap();
        })));
        let (active, error) = rx.recv().unwrap();
        assert_eq!(active.as_deref(), Some("missing previous driver"));
        assert!(error.unwrap().contains("missing requested driver"));
        assert!(engine.sh().loading);
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    fn lost_outputs_hold_position_until_play() {
        let mut engine = test_engine(Some("yamusic missing old".into()));
        engine.loaded = true;
        engine.paused = false;
        engine.position_base = Duration::from_secs(7);
        engine.current_bytes = Some(Arc::from([1]));
        engine.change_output(Some("yamusic missing new".into()), true);
        engine.tick();
        assert!(engine.paused);
        assert_eq!(engine.sh().position, 7.0);
        assert_eq!(engine.asio_driver.as_deref(), Some("yamusic missing old"));
        assert!(engine.sh().audio_output_error.is_some());
        engine.cmd(Cmd::Resume);
        assert!(engine.paused);
        assert_eq!(engine.pos(), 7.0);
    }

    #[cfg(all(windows, feature = "asio"))]
    #[test]
    #[ignore = "requires a free ASIO device and an MP3 fixture"]
    fn switch_live_outputs_on_hardware() {
        let name = std::env::var("YAMUSIC_TEST_ASIO").unwrap();
        let bytes: Arc<[u8]> = std::fs::read(std::env::var_os("YAMUSIC_TEST_AUDIO").unwrap()).unwrap().into();
        let mut engine = test_engine(None);
        engine.paused = false;
        engine.play_loaded(bytes.clone());
        let wait_for_audio = |e: &Engine, start: f32| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while e.pos() < start + 0.1 && Instant::now() < deadline { thread::sleep(Duration::from_millis(10)); }
            assert!(e.pos() >= start + 0.1, "position {}, expected progress from {start}", e.pos());
        };
        wait_for_audio(&engine, 0.0);
        let next = crate::api::parse_track(&serde_json::json!({ "id": "2", "title": "Следующий" })).unwrap();
        engine.queue.push_back(next);
        engine.played_station = WAVE.into();
        engine.played = vec!["history marker".into()];
        engine.eq.set(true, 0.8, &[1.0; 10]);
        let eq = engine.eq.clone();
        engine.seek(Duration::from_secs(3));
        wait_for_audio(&engine, 3.0);
        let before = engine.pos();
        let start = Instant::now();
        engine.change_output(Some(name.clone()), true);
        assert!(engine.sh().audio_output_error.is_none(), "{:?}", engine.sh().audio_output_error);
        assert_eq!(engine.asio_driver.as_deref(), Some(name.as_str()));
        eprintln!("WASAPI -> ASIO {:?}, position {before} -> {}", start.elapsed(), engine.pos());
        assert!((engine.pos() - before).abs() < 0.2);
        wait_for_audio(&engine, before);
        let Some(Output::Asio(stream)) = engine.audio.as_mut() else { panic!("expected ASIO"); };
        let start = Instant::now();
        let initial = stream.callback_count();
        while start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(250));
            assert!(!stream.needs_reopen(), "ASIO reset or callback timeout");
        }
        let blocks = stream.callback_count().wrapping_sub(initial);
        eprintln!("ASIO processed {blocks} buffers in {:?}", start.elapsed());
        assert!(blocks > 0);
        engine.cmd(Cmd::PlayPause);
        thread::sleep(Duration::from_millis(80));
        let paused = engine.pos();
        engine.change_output(Some("yamusic absent device".into()), true);
        assert!(engine.sh().audio_output_error.is_some());
        assert_eq!(engine.asio_driver.as_deref(), Some(name.as_str()));
        assert!(engine.sink().unwrap().is_paused());
        let start = Instant::now();
        engine.change_output(None, true);
        eprintln!("ASIO -> WASAPI {:?}, paused {paused} -> {}", start.elapsed(), engine.pos());
        assert!(engine.sh().audio_output_error.is_none());
        assert!(engine.asio_driver.is_none());
        engine.asio_driver = Some("yamusic unplugged previous driver".into());
        let (reply, result) = mpsc::channel();
        engine.cmd(Cmd::Output(Some("yamusic unavailable new driver".into()), Box::new(move |active, error| {
            reply.send((active, error)).unwrap();
        })));
        let (active, error) = result.recv().unwrap();
        assert!(active.is_none() && error.is_some());
        assert!(matches!(engine.audio, Some(Output::System { .. })));
        thread::sleep(Duration::from_millis(100));
        assert!((engine.pos() - paused).abs() < 0.05);
        assert!(Arc::ptr_eq(engine.current_bytes.as_ref().unwrap(), &bytes));
        assert_eq!(engine.queue.front().unwrap().id, "2");
        assert_eq!(engine.played, ["history marker"]);
        assert!(Arc::ptr_eq(&engine.eq, &eq));
        assert_eq!(engine.sink().unwrap().volume(), 0.0);
        // Проверяем обнаружение смены устройства без изменения системных настроек.
        let Some(Output::System { observed_default, .. }) = &mut engine.audio else { panic!("expected WASAPI"); };
        *observed_default = None;
        engine.next_output_check = Instant::now();
        engine.tick();
        assert!(engine.sh().audio_output_error.is_none());
        assert!(matches!(engine.audio.as_ref(), Some(Output::System { observed_default: Some(_), .. })));
        assert!(engine.sink().unwrap().is_paused());
        engine.cmd(Cmd::Resume);
        wait_for_audio(&engine, paused);

        // Имитируем остановку WASAPI на том же устройстве.
        let (stalled, _undrained_source) = Sink::new_idle();
        stalled.append(rodio::source::Zero::<f32>::new(2, 44100));
        engine.sink = Some(stalled);
        engine.next_output_check = Instant::now();
        engine.tick();
        thread::sleep(Duration::from_millis(3200));
        engine.tick();
        wait_for_audio(&engine, engine.position_base.as_secs_f32());
    }

    /// Сетевой тест: ссылка на поток → скачивание → декодирование (без токена — 30-сек превью).
    #[test]
    #[ignore]
    fn stream_and_decode() {
        let api = Api::new("");
        let url = api.stream_url("57703").expect("stream url");
        let bytes = api.fetch_bytes(&url).expect("download");
        assert!(bytes.len() > 50_000);
        let dec = Decoder::new(Cursor::new(bytes)).expect("decode");
        let samples = dec.take(44100).count();
        assert!(samples > 1000);
    }
}


#[cfg(test)]
#[test]
fn explicit_filter_survives_repeat_and_previous() {
    let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default(), None);
    let blocked = crate::api::parse_track(&serde_json::json!({"id":1,"title":"E","explicit":true})).unwrap();
    let allowed = crate::api::parse_track(&serde_json::json!({"id":2,"title":"A"})).unwrap();
    let previous = crate::api::ALLOW_EXPLICIT.swap(false, std::sync::atomic::Ordering::Relaxed);
    engine.list = Some(ListPlay { deferred: VecDeque::new(), all: vec![blocked.clone(), allowed.clone()], after: After::Loop });
    engine.repeat = Repeat::All;
    engine.play_from_queue();
    let repeated = engine.current.take().map(|t| t.id);
    engine.history = vec![allowed, blocked];
    engine.prev();
    let previous_track = engine.current.take().unwrap().id;
    let explicit = engine.list.as_ref().unwrap().all[0].clone();
    engine.list.as_mut().unwrap().all = vec![explicit.clone()];
    engine.list.as_mut().unwrap().after = After::Wave(Source::Wave);
    engine.repeat = Repeat::Off;
    engine.queue = VecDeque::from([explicit]);
    engine.play_from_queue();
    engine.set_allow_explicit(true);
    engine.cmd(Cmd::PlayPause);
    let resumed = engine.current.take().map(|t| t.id);
    let mut first = engine.list.as_ref().unwrap().all[0].clone();
    first.id = "3".into();
    let second = engine.list.as_ref().unwrap().all[0].clone();
    engine.set_allow_explicit(false);
    engine.cmd(Cmd::PlayList { source: Source::Wave, tracks: vec![first, second], start: 1, after: After::Loop });
    engine.set_allow_explicit(true);
    engine.cmd(Cmd::Resume);
    let selected = engine.current.as_ref().map(|t| t.id.clone());
    crate::api::ALLOW_EXPLICIT.store(previous, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(selected.as_deref(), Some("1"));
    assert_eq!(resumed.as_deref(), Some("1"));
    assert_eq!(repeated.as_deref(), Some("2"));
    assert_eq!(previous_track, "2");
}

#[cfg(test)]
#[test]
fn later_source_command_expires_artist_request() {
    let (tx, rx) = mpsc::channel();
    let player = Player { tx: tx.into(), shared: Default::default(), likes: Default::default(), eq: Default::default(), source_request: Default::default() };
    let first = player.begin_source_request();
    let second = player.begin_source_request();
    assert!(!player.send_for_request(first, Cmd::Wave(Source::Wave)));
    player.send(Cmd::Logout);
    assert!(!player.send_for_request(second, Cmd::Wave(Source::Wave)));
    assert!(matches!(rx.try_recv(), Ok(Msg::Cmd(Cmd::Logout))));
    assert!(rx.try_recv().is_err());
}

#[cfg(test)]
#[test]
fn last_failed_like_restores_previous_confirmation() {
    let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default(), None);
    engine.pending_likes.insert("1".into(), PendingLike { first: 2, latest: 3, confirmed: false });
    engine.handle(Msg::LikeSaved { id: "1".into(), seq: 1, liked: false, result: Ok(()) });
    engine.handle(Msg::LikeSaved { id: "1".into(), seq: 2, liked: true, result: Ok(()) });
    assert!(!engine.likes.lock().unwrap().contains("1"));
    engine.handle(Msg::LikeSaved { id: "1".into(), seq: 3, liked: false, result: Err("offline".into()) });
    assert!(engine.likes.lock().unwrap().contains("1"));
    engine.pending_likes.insert("2".into(), PendingLike { first: 4, latest: 4, confirmed: false });
    engine.merge_likes(vec!["2".into()]);
    engine.handle(Msg::LikeSaved { id: "2".into(), seq: 4, liked: true, result: Err("offline".into()) });
    assert!(engine.likes.lock().unwrap().contains("2"));
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn mpris_relative_seek_starts_at_the_preceding_absolute_seek() {
    let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default(), None);
    engine.current = crate::api::parse_track(&serde_json::json!({"id":1,"title":"A","durationMs":120000}));
    engine.current_bytes = Some(Arc::from([1]));
    engine.position_base = Duration::from_secs(10);
    engine.cmd(Cmd::SeekTrack(crate::mpris::track_id("1"), 30_000_000));
    engine.cmd(Cmd::SeekBy(5.0));
    assert_eq!(engine.position_base, Duration::from_secs(35));
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn ordered_pause_resume_commands_are_idempotent() {
    let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default(), None);
    for cmd in [Cmd::Pause, Cmd::Pause] { engine.cmd(cmd); }
    assert!(engine.paused);
    for cmd in [Cmd::Resume, Cmd::Resume] { engine.cmd(cmd); }
    assert!(!engine.paused);
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn ordered_seeks_follow_the_loading_track() {
    let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default(), None);
    let track = crate::api::parse_track(&serde_json::json!({"id": 7, "title": "A"})).unwrap();
    engine.history.push(track);
    engine.cmd(Cmd::Prev);
    let id = crate::mpris::track_id("7");
    engine.cmd(Cmd::SeekTrack(id, 30_000_000));
    engine.cmd(Cmd::SeekBy(2.0));
    assert_eq!(engine.pos(), 32.0);
    engine.start_current();
    assert_eq!(engine.pos(), 0.0);
}

#[cfg(test)]
#[test]
fn unshuffle_updates_a_deferred_list() {
    let mut engine = Engine::new(mpsc::channel().0, Default::default(), Default::default(), Arc::new(|| {}), Default::default(), None);
    let tracks: Vec<_> = (0..4).map(|i| {
        let mut t = crate::api::parse_track(&serde_json::json!({"id":i,"title":"T","explicit":true})).unwrap();
        t.list_position = Some(i);
        t
    }).collect();
    let deferred = [1, 3, 0, 2].map(|i| tracks[i].clone()).into();
    engine.list = Some(ListPlay { all: tracks, deferred, after: After::Loop });
    engine.cmd(Cmd::Shuffle(false));
    let order: Vec<_> = engine.list.unwrap().deferred.iter().filter_map(|t| t.list_position).collect();
    assert_eq!(order, [1, 2, 3, 0]);
}
