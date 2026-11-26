//! Окно на Slint: связывает интерфейс с плеером и данными.

slint::include_modules!();

mod history_ui;
mod keys;
mod landing_ui;
mod track_about;

use crate::save::{audio_ext, file_name_max};
use crate::api::{album_kind_word, Album, Api, Card, Collection, Pin, Playlist, Track};
use crate::assets::Sprite;
use crate::config::{log, Config};
use crate::images::Images;
use crate::net::{tracks_word, Data, ListData, Net};
use crate::player::{After, Cmd, ListKind, Loudness, Player, Repeat, Source, Status};
use crate::sys;
use slint::winit_030::{winit, WinitWindowAccessor};
use slint::{Color, ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct EntityLike<T = bool> {
    confirmed: T,
    desired: T,
}

impl<T: Copy + PartialEq> EntityLike<T> {
    fn complete(&mut self, sent: T, success: bool) -> Option<T> {
        if success { self.confirmed = sent; }
        if self.desired == sent && !success { self.desired = self.confirmed; }
        (self.desired != self.confirmed).then_some(self.desired)
    }
}

#[cfg(test)]
#[test]
fn repeated_entity_clicks_settle_on_the_last_choice() {
    let mut pending = EntityLike { confirmed: false, desired: false };
    assert_eq!(pending.complete(true, true), Some(false));
    assert_eq!(pending.complete(false, false), None);
    assert!(pending.desired && pending.confirmed);
}

fn merge_entity_likes(kind: &'static str, fresh: &mut std::collections::HashSet<String>,
    saved: &HashMap<(&'static str, String), bool>, pending_likes: &mut HashMap<(&'static str, String), EntityLike>) {
    for ((k, key), &confirmed) in saved {
        if *k == kind {
            if confirmed { fresh.insert(key.clone()); } else { fresh.remove(key); }
        }
    }
    for ((k, id), pending) in pending_likes {
        if *k == kind {
            let key = if kind == "playlists" { format!("playlist:{id}") } else { id.clone() };
            pending.confirmed = fresh.contains(&key);
            if pending.desired { fresh.insert(key); } else { fresh.remove(&key); }
        }
    }
}

#[cfg(test)]
#[test]
fn stale_snapshot_cannot_erase_confirmed_like_during_unlike() {
    let id = ("albums", String::from("1"));
    let saved = HashMap::from([(id.clone(), true)]);
    let mut pending = HashMap::from([(id.clone(), EntityLike { confirmed: true, desired: false })]);
    let mut snapshot = Default::default();
    merge_entity_likes("albums", &mut snapshot, &saved, &mut pending);
    assert!(snapshot.is_empty());
    let like = pending.get_mut(&id).unwrap();
    assert_eq!(like.complete(false, false), None);
    assert!(like.desired);
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ArtistMark { Like, Dislike, Neutral }

impl ArtistMark {
    // При снятии реакции API выбирается по последнему подтверждённому состоянию.
    fn request(self, previous: Self) -> (bool, bool) {
        let remove = self == Self::Neutral;
        let dislike = (if remove { previous } else { self }) == Self::Dislike;
        (dislike, remove)
    }
}

#[cfg(test)]
#[test]
fn artist_dislike_waits_for_the_like_and_restores_it_on_error() {
    use ArtistMark::*;
    let mut reaction = EntityLike { confirmed: Neutral, desired: Dislike };
    assert_eq!(reaction.complete(Like, true), Some(Dislike));
    assert_eq!(reaction.desired, Dislike);
    assert_eq!(reaction.complete(Dislike, false), None);
    assert_eq!(reaction.desired, Like);
    assert_eq!(Neutral.request(Dislike), (true, true));
    assert_eq!(Neutral.request(Like), (false, true));
}

const SEARCH_TRACK_KIND: &str = "track";

struct ListDownload {
    uid: String,
    list_key: String,
    stop: Arc<AtomicBool>,
}

impl ListDownload {
    fn stop_for(&self, uid: &str, list_key: Option<&str>) -> Option<&AtomicBool> {
        let key = list_key?;
        (self.uid == uid && self.list_key == Self::key(uid, key)).then_some(&self.stop)
    }

    fn key(uid: &str, key: &str) -> String {
        if key == "likes" { format!("playlist:{uid}:3") } else { key.to_string() }
    }
}

struct State {
    entity_likes: HashMap<(&'static str, String), EntityLike>,
    saved_entities: HashMap<(&'static str, String), bool>,
    cfg: Config,
    dark: bool,
    uid: String,
    /// Пока пользователь ничего не включил, плеер показывается пустым — как в официальном приложении.
    started: bool,
    wheel: Vec<crate::api::WheelItem>,
    /// Для какой волны загружено колесо; None — ещё не грузили.
    wheel_seeds: Option<Vec<String>>,
    /// Настройки «Моей волны», выбранные здесь (seeds `setting…`).
    tune_seeds: Vec<String>,
    liked_albums: std::collections::HashSet<String>,
    liked_playlists: std::collections::HashSet<String>,
    /// «Смотреть всё»: ключ страницы, её вкладки и выбранная вкладка.
    grid_key: Option<String>,
    grid_titles: HashMap<String, String>,
    grid: Vec<(String, Vec<Card>)>,
    grid_tab: usize,
    /// Лендинг по скелету («Жанры», «Эпохи»): id скелета.
    landing_key: Option<String>,
    net: Option<Net>,
    pins: Vec<Pin>,
    /// Когда закрепы последний раз сверялись с сервером: их могли поменять в другом клиенте.
    pins_checked: Option<Instant>,
    playlists: Vec<Playlist>,
    albums: Vec<Album>,
    coll_shown: Option<Arc<Data>>,
    pl_tab: usize,
    album_tab: usize,
    list_key: Option<String>,
    list_shown: Option<Arc<Data>>,
    view: Vec<usize>,
    search_index: Vec<(String, String)>,
    history: Vec<(Page, Option<String>)>,
    forward: Vec<(Page, Option<String>)>,
    track_id: Option<String>,
    toast_timer: Option<Timer>,
    last_audio_output_error: Option<Instant>,
    cards: HashMap<String, Vec<Card>>,
    search_shown: Option<Arc<Data>>,
    results_key: Option<String>,
    results_shown: Option<Arc<Data>>,
    results: Vec<Track>,
    search_timer: Option<Timer>,
    chart: Vec<Track>,
    artist_key: Option<String>,
    artist_shown: Option<Arc<Data>>,
    artist_tracks: Vec<Track>,
    liked_artists: std::collections::HashSet<String>,
    saved_artist_marks: HashMap<String, ArtistMark>,
    pending_artist_marks: HashMap<String, EntityLike<ArtistMark>>,
    /// None, пока список не пришёл с сервера.
    disliked_artists: Option<std::collections::HashSet<String>>,
    menu_track: Option<Track>,
    /// Только что созданный плейлист: когда откроется, сразу править название.
    rename_key: Option<String>,
    menu_playlists: Vec<Playlist>,
    lyrics_for: Option<String>,
    lyrics_track: Option<String>,
    lyrics: Vec<crate::api::SyncLine>,
    /// Текст без меток времени: все строки видны, прокрутка свободная.
    lyrics_static: bool,
    lyric_target: i32,
    lyrics_wheel_timer: Option<Timer>,
    lyrics_mouse_timer: Option<Timer>,
    queue_ids: Vec<String>,
    blob_src: Option<SharedPixelBuffer<Rgba8Pixel>>,
    blob_requested: bool,
    vibe_key: Option<(bool, Option<[u8; 3]>)>,
    /// Анимация пятна: таймер есть, только пока волна играет на экране.
    trailer: Option<Trailer>,
    /// Настройки «Моей волны» с сервера, выбранный вариант в каждой группе и картинки вариантов.
    wave_tune: Vec<(crate::api::WaveSettingGroup, usize, Vec<Image>)>,
    /// Аккаунт и ключ списка, для которого можно отменить загрузку.
    download: Option<ListDownload>,
}

/// Трейлер: треки звучат по 12,5 с с середины, потом следующий.
struct Trailer {
    ids: Vec<String>,
    seeked: Option<String>,
    start: f32,
    seek_at: Option<Instant>,
    done: Option<String>,
    /// Плеер уже переключился на трейлер; до этого чужой трек — не повод его сбрасывать.
    began: bool,
}

const TRAILER_PART: f32 = 12.5;

impl State {
    fn remote_artist_mark(&self, id: &str) -> ArtistMark {
        if self.liked_artists.contains(id) { ArtistMark::Like }
        else if self.disliked_artists.as_ref().is_some_and(|ids| ids.contains(id)) { ArtistMark::Dislike }
        else { ArtistMark::Neutral }
    }

    fn artist_mark(&self, id: &str) -> ArtistMark {
        self.pending_artist_marks.get(id).map(|p| p.desired)
            .or_else(|| self.saved_artist_marks.get(id).copied())
            .unwrap_or_else(|| self.remote_artist_mark(id))
    }

    fn download_stop(&self) -> Option<&AtomicBool> {
        self.list_shown.as_ref()?;
        self.download.as_ref()?.stop_for(&self.uid, self.list_key.as_deref())
    }
}

struct Ctx {
    ui: slint::Weak<AppWindow>,
    /// Лёгкий режим работы: без анимаций, кэши меньше.
    lite: Cell<bool>,
    player: Player,
    volume_save_at: Cell<Option<Instant>>,
    discord: Arc<crate::discord::Discord>,
    images: Images,
    list_window: Cell<(usize, usize)>,
    grid_window: Cell<(usize, usize)>,
    st: RefCell<State>,
}

thread_local! {
    static CTX: RefCell<Option<Rc<Ctx>>> = const { RefCell::new(None) };
}

fn with_ctx(f: impl FnOnce(&Rc<Ctx>)) {
    let c = CTX.with(|c| c.borrow().clone());
    if let Some(c) = c {
        f(&c);
    }
}

fn rgb(c: [u8; 3]) -> Color {
    Color::from_rgb_u8(c[0], c[1], c[2])
}

fn volume_save_due(deadline: &Cell<Option<Instant>>, now: Instant) -> bool {
    if deadline.get().is_some_and(|at| now >= at) {
        deadline.set(None);
        true
    } else {
        false
    }
}

#[cfg(test)]
#[test]
fn slider_writes_once_after_the_last_change() {
    let start = Instant::now();
    let end = start + Duration::from_millis(300);
    let deadline = Cell::new(Some(end));
    deadline.set(Some(end + Duration::from_millis(30)));
    assert!(!volume_save_due(&deadline, end));
    assert!(volume_save_due(&deadline, end + Duration::from_millis(30)));
    assert!(!volume_save_due(&deadline, end + Duration::from_secs(1)));
}

fn mmss(secs: f32) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn mm_ss(ms: u64) -> String {
    let s = (ms + 500) / 1000;
    format!("{:02}:{:02}", s / 60, s % 60)
}

use crate::net::group_digits;

/// В группе настроек волны ничего не выбрано (у «Под занятие» нет варианта «Любое»).
const NOTHING: usize = usize::MAX;

fn is_activity(seed: &str) -> bool {
    seed.starts_with("activity:")
}

fn plural<'a>(n: u64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let (m10, m100) = (n % 10, n % 100);
    if m10 == 1 && m100 != 11 {
        one
    } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
        few
    } else {
        many
    }
}

/// «4,5 часа», «4 часа», «45 минут» — как в «Любимых исполнителях».
fn listen_time(secs: u64) -> String {
    if secs < 3600 {
        let m = (secs + 30) / 60;
        return format!("{m} {}", plural(m, "минута", "минуты", "минут"));
    }
    let halves = (secs + 900) / 1800;
    let (h, half) = (halves / 2, halves % 2 == 1);
    if half {
        format!("{h},5 часа")
    } else {
        format!("{h} {}", plural(h, "час", "часа", "часов"))
    }
}

fn date_ru(iso: &str) -> String {
    const MONTHS: [&str; 12] = [
        "января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря",
    ];
    let mut it = iso.get(..10).unwrap_or("").split('-').map(|p| p.parse::<u32>().unwrap_or(0));
    let (y, m, d) = (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0));
    if m == 0 || m > 12 {
        return String::new();
    }
    let this_year = 1970 + (crate::config::now_secs() / 31_556_952) as u32;
    if y == this_year {
        format!("{d} {}", MONTHS[m as usize - 1])
    } else {
        format!("{d} {} {y}", MONTHS[m as usize - 1])
    }
}

fn previous_row<T: Clone + 'static>(model: &ModelRc<T>, first: i32, index: usize) -> Option<T> {
    model.row_data(index.checked_sub(first as usize)?)
}

fn card_item(c: &Card) -> CardItem {
    CardItem {
        title: c.title.clone().into(),
        subtitle: c.subtitle.clone().into(),
        round: c.round,
        year: c.year.clone().into(),
        likes: c.likes.map(group_digits).unwrap_or_default().into(),
        explicit: c.explicit,
        ..Default::default()
    }
}

impl Ctx {
    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("окно живо")
    }

    /// Ближайший размер обложки для `css` логических пикселей.
    fn size_for(&self, css: f32) -> u32 {
        // Ровно под экран: картинка 1:1 рисуется без сглаживания, а это вдвое быстрее.
        // Адрес на сервере округляет `api::pic`.
        ((css * self.ui().window().scale_factor()).round() as u32).clamp(1, 1000)
    }

    fn toast(self: &Rc<Self>, msg: impl Into<String>) {
        let ui = self.ui();
        ui.global::<App>().set_toast_has_cover(false);
        ui.global::<App>().set_toast(crate::i18n::tr(&msg.into()).into());
        let t = Timer::default();
        let weak = self.ui.clone();
        t.start(TimerMode::SingleShot, Duration::from_secs(4), move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<App>().set_toast(SharedString::new());
            }
        });
        self.st.borrow_mut().toast_timer = Some(t);
    }

    fn sync_player(self: &Rc<Self>) {
        if volume_save_due(&self.volume_save_at, Instant::now()) {
            self.st.borrow().cfg.save();
        }
        let s = self.player.state();
        let ui = self.ui();
        let app = ui.global::<App>();

        if let Some(message) = self.player.take_like_error() { self.toast(message); }

        if let Status::Failed(e) = &s.status {
            // Токен отозван или истёк — обратно на экран входа.
            if self.st.borrow().net.is_none() && e.contains("токен") && !app.get_logged_out() {
                self.st.borrow_mut().cfg.forget_token();
                self.st.borrow().cfg.save();
                self.login_start();
            } else if app.get_toast().is_empty() && !app.get_logged_out() {
                self.toast(e.clone());
            }
        }
        if let Some((ref message, time)) = s.audio_output_error {
            let is_new = self.st.borrow().last_audio_output_error != Some(time) && !app.get_mini();
            #[cfg(windows)]
            let is_new = is_new && sys::is_shown(ui.window()) && !sys::is_minimized(ui.window());
            if is_new {
                self.st.borrow_mut().last_audio_output_error = Some(time);
                self.toast(message.as_str());
            }
        }
        if app.get_page() == Page::Settings {
            app.set_discord_status(self.discord.status().into());
        }
        if s.status == Status::Ready && self.st.borrow().cfg.token.is_some() && self.st.borrow().net.is_none() {
            self.on_ready(&s.uid);
        }

        if s.resumed && !self.st.borrow().started {
            self.st.borrow_mut().started = true;
        }
        let started = self.st.borrow().started;
        app.set_has_track(started && s.track.is_some());
        app.set_playing(s.playing);
        app.set_can_pause(started && s.can_pause());
        app.set_playback_paused(s.paused && s.audio_output_error.is_none());
        #[cfg(windows)]
        crate::tray::update_media(&s);
        app.set_likes_revision(s.likes_revision);
        app.set_loading(s.loading);
        app.set_position(s.position);
        app.set_track_format(s.format.as_str().into());
        app.set_duration(s.duration);
        app.set_volume(s.volume);
        app.set_shuffle(s.shuffle);
        app.set_repeat(match s.repeat {
            Repeat::Off => 0,
            Repeat::All => 1,
            Repeat::One => 2,
        });
        app.set_is_wave(s.source.station().is_some());
        // Обычная «Моя волна» подписана просто; всё остальное — «челка» со сбросом.
        let (source_title, context) = match &s.source {
            Source::Wave => ("Моя волна".to_string(), false),
            Source::Seed { header, title, .. } if title.is_empty() => (header.clone(), true),
            Source::Seed { header, title, .. } if header == "Моя волна" => (title.clone(), true),
            Source::Seed { header, title, .. } => (format!("{header} «{title}»"), true),
            Source::List { title, .. } => (title.clone(), true),
        };
        app.set_source_title(source_title.into());
        app.set_wave_context(context);
        self.sync_wheel(&s.source);

        app.set_can_prev(s.track.is_some());
        let playing_now = started && s.playing;
        self.sync_vibe(playing_now, s.track.as_ref().and_then(|t| t.color));

        self.sync_trailer(&s);
        self.sync_queue(&s);
        let tid = s.track.as_ref().map(|t| t.id.clone());
        if self.st.borrow().lyrics_track != tid {
            self.st.borrow_mut().lyrics_track = tid;
            self.reset_lyrics();
        }
        self.update_lyric_current(s.position);
        if app.get_fullscreen() && app.get_fs_mode() == 2 {
            self.sync_lyrics();
        }
        let track = s.track.filter(|_| started);
        let changed = track_label_changed(&mut self.st.borrow_mut().track_id, track.as_ref().map(|t| t.id.as_str()));
        let Some(t) = track else {
            app.set_title(SharedString::new());
            app.set_artists(SharedString::new());
            return;
        };
        app.set_liked(self.player.is_liked(&t.id));
        app.set_disliked(s.disliked);
        if changed {
            app.set_title(t.title.clone().into());
            app.set_artists(t.artists.clone().into());
            let names: Vec<SharedString> = t.artist_list.iter().map(|a| SharedString::from(a.name.as_str())).collect();
            app.set_artist_list(ModelRc::from(Rc::new(VecModel::from(names))));
            let longest = t.artists.split_whitespace().max_by_key(|w| w.chars().count()).unwrap_or("");
            app.set_artists_longest(longest.into());
            app.set_track_version(t.version.clone().into());
            app.set_track_explicit(t.explicit);
            app.set_has_lyrics(t.lyrics);
            if let Some(c) = t.color {
                app.set_tint(player_tint(c));
                app.set_accent(rgb(c));
            }
            if app.get_fullscreen() {
                self.load_cover_big();
            }
            self.update_artist_current();
            app.set_cover(Image::default());
            if let Some(url) = t.cover_url(self.size_for(96.0)) {
                let weak = self.ui.clone();
                let id = t.id.clone();
                self.images.get(&url, self.size_for(96.0), move |img| {
                    with_ctx(|c| {
                        if c.st.borrow().track_id.as_deref() == Some(id.as_str()) {
                            if let Some(ui) = weak.upgrade() {
                                ui.global::<App>().set_cover(img.clone());
                            }
                        }
                    })
                });
            }
            self.update_list_current();
        }
    }

    fn same_session(&self, session: &Net) -> bool {
        self.st.borrow().net.as_ref().is_some_and(|net| net.same_session(session))
    }

    fn on_ready(self: &Rc<Self>, uid: &str) {
        let token = self.st.borrow().cfg.token.clone().unwrap_or_default();
        let notify = notifier();
        let net = Net::new(Api::new(&token), uid.to_string(), notify);
        self.st.borrow_mut().net = Some(net.clone());
        self.st.borrow_mut().uid = uid.to_string();
        net.get("collection");

        let weak = self.ui.clone();
        let api = net.api.clone();
        let uid = uid.to_string();
        let session = net.clone();
        std::thread::spawn(move || {
            let about = api.about();
            let pins = api.pins();
            let liked_albums = api.liked_album_ids(&uid);
            let artists = api.liked_artists(&uid);
            let liked_playlists = api.liked_playlist_keys(&uid);
            let less_ai = api.account_settings().ok().and_then(|v| v.get("aiContentReductionEnabled").and_then(|b| b.as_bool()));
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|c| {
                    let Some(ui) = weak.upgrade() else { return };
                    let app = ui.global::<App>();
                    if !c.same_session(&session) { return; }
                    match about {
                        Ok((name, avatar)) => {
                            app.set_login(name.into());
                            if let Some(url) = avatar {
                                let w = weak.clone();
                                c.images.get(&url, 100, move |img| {
                                    with_ctx(|c| {
                                        if c.same_session(&session) {
                                            if let Some(ui) = w.upgrade() { ui.global::<App>().set_avatar(img.clone()); }
                                        }
                                    });
                                });
                            }
                        }
                        Err(e) => log(format!("профиль: {e}")),
                    }
                    match pins {
                        Ok(p) => c.set_pins(p),
                        Err(e) => log(format!("pins: {e}")),
                    }
                    if let Ok(artists) = artists {
                        c.st.borrow_mut().liked_artists = artists.into_iter().map(|a| a.id).collect();
                        c.show_artist_mark();
                    }
                    match liked_albums {
                        Ok(ids) => c.merge_entity_snapshot("albums", ids),
                        Err(e) => log(format!("понравившиеся альбомы: {e}")),
                    }
                    match liked_playlists {
                        Ok(keys) => c.merge_entity_snapshot("playlists", keys),
                        Err(e) => log(format!("понравившиеся плейлисты: {e}")),
                    }
                    if let Some(on) = less_ai {
                        crate::api::LESS_AI.store(on, std::sync::atomic::Ordering::Relaxed);
                        app.set_less_ai(on);
                    }
                })
            });
        });
        let s = self.player.state();
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_plus(s.has_plus);
        if app.get_login().is_empty() {
            app.set_login(s.login.into());
        }
    }

    fn set_pins(self: &Rc<Self>, pins: Vec<Pin>) {
        // Без изменений не пересобираем: обложки мигнули бы.
        if self.st.borrow().pins == pins {
            return;
        }
        let items: Vec<PinItem> = pins
            .iter()
            .map(|p| PinItem { title: p.title.clone().into(), subtitle: p.subtitle.clone().into(), cover: Image::default(), round: p.round })
            .collect();
        let model = Rc::new(VecModel::from(items));
        self.ui().global::<App>().set_pins(ModelRc::from(model.clone()));
        let size = self.size_for(40.0);
        for (i, p) in pins.iter().enumerate() {
            if let Some(uri) = &p.cover {
                let m = model.clone();
                self.images.get(&crate::api::pic(uri, size), size, move |img| {
                    if let Some(mut row) = m.row_data(i) {
                        row.cover = img.clone();
                        m.set_row_data(i, row);
                    }
                });
            }
        }
        self.st.borrow_mut().pins = pins;
    }

    fn navigate(self: &Rc<Self>, page: Page, key: Option<String>, remember: bool) {
        let key = match key {
            Some(k) if page == Page::List && k.strip_prefix("playlist:").and_then(|s| s.strip_suffix(":3")) == Some(self.st.borrow().uid.as_str()) => Some("likes".into()),
            key => key,
        };
        let ui = self.ui();
        let app = ui.global::<App>();
        self.check_pins();
        // Поле ввода старой страницы исчезает вместе с ней и о потере фокуса не сообщает.
        app.set_text_focused(false);
        let from = app.get_page();
        if remember {
            let cur = self.here();
            let mut st = self.st.borrow_mut();
            if st.history.last() != Some(&cur) && (cur.0 != page || cur.1 != key) {
                st.history.push(cur);
                if st.history.len() > 100 {
                    st.history.remove(0);
                }
            }
            st.forward.clear();
        }
        if page != Page::List {
            self.images.cancel_pending();
        }
        app.set_menu_open(false);
        app.set_page(page);
        app.set_hint_open(false);
        app.set_cover_open(false);
        app.set_overview_open(false);
        if from != page {
            self.release_page(from);
        }
        app.set_can_back(!self.st.borrow().history.is_empty());
        app.set_can_forward(!self.st.borrow().forward.is_empty());
        ui.global::<Theme>().set_dark(page == Page::Wave || self.st.borrow().dark);
        match page {
            Page::List => {
                if let Some(k) = key {
                    let same = self.st.borrow().list_key.as_deref() == Some(k.as_str());
                    if !same {
                        self.images.cancel_pending();
                        let mut st = self.st.borrow_mut();
                        st.list_key = Some(k);
                        st.list_shown = None;
                        st.view.clear();
                        st.search_index.clear();
                        drop(st);
                        app.set_list_count(0);
                        app.set_tracks(ModelRc::default());
                        app.set_list_filter(SharedString::new());
                        app.set_list_edit(0);
                        app.set_list_desc_raw(SharedString::new());
                        app.set_list_title(SharedString::new());
                        app.set_list_desc(SharedString::new());
                        app.set_list_meta(SharedString::new());
                        app.set_list_cover(Image::default());
                        app.set_list_label(SharedString::new());
                        app.set_list_label_id(SharedString::new());
                    }
                }
                self.refresh_list();
            }
            Page::Collection => self.refresh_collection(),
            Page::Search => {
                let text = app.get_search_text();
                let tracks_only = key.as_deref() == Some(SEARCH_TRACK_KIND) && !text.trim().is_empty();
                self.refresh_search();
                if app.get_search_tracks_only() != tracks_only {
                    app.set_search_tracks_only(tracks_only);
                    self.search_now(text.to_string());
                } else {
                    self.refresh_results();
                }
            }
            Page::Artist => {
                if let Some(k) = key {
                    if self.st.borrow().artist_key.as_deref() != Some(k.as_str()) {
                        self.images.cancel_pending();
                        let mut st = self.st.borrow_mut();
                        st.artist_key = Some(k);
                        st.artist_shown = None;
                        drop(st);
                        app.set_ar_name(SharedString::new());
                        app.set_ar_listeners(SharedString::new());
                        app.set_ar_cover(Image::default());
                        app.set_ar_familiar(SharedString::new());
                        app.set_ar_popular(ModelRc::default());
                        for f in [App::set_ar_release, App::set_ar_studio, App::set_ar_albums, App::set_ar_playlists, App::set_ar_compilations, App::set_ar_similar] {
                            f(&app, ModelRc::default());
                        }
                        app.set_ar_tint(Color::from_argb_u8(0, 0, 0, 0));
                    }
                }
                self.refresh_artist();
            }
            Page::Artists => self.refresh_my_artists(),
            Page::Trends | Page::Concerts | Page::Nonmusic => self.refresh_landing(),
            Page::Landing => {
                // Другой лендинг: прежние блоки и заголовок не висят, пока грузится новый скелет.
                let other = key.filter(|k| self.st.borrow().landing_key.as_ref() != Some(k));
                if let Some(k) = &other {
                    self.release_landing();
                    self.st.borrow_mut().landing_key = Some(k.clone());
                }
                if other.is_some() || from != page {
                    app.set_landing_title(SharedString::new());
                }
                self.refresh_landing();
            }
            Page::Chart => self.refresh_chart(),
            #[cfg(all(windows, feature = "asio"))]
            Page::Settings => {
                let configured = self.st.borrow().cfg.asio_driver.clone();
                setup_audio_output(&app, configured.as_deref());
            }
            Page::History => self.enter_history(),
            Page::Grid => {
                if let Some(k) = key {
                    let previous = self.st.borrow_mut().grid_key.replace(k.clone());
                    if previous.as_ref() != Some(&k) {
                        app.set_grid_label_popular(false);
                    }
                }
                self.refresh_grid();
            }
            _ => {}
        }
    }

    /// «Смотреть всё»: всё содержимое блока, у коллекции — с вкладками.
    fn refresh_grid(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let Some(key) = self.st.borrow().grid_key.clone() else { return };
        let title = self.st.borrow().grid_titles.get(&key).cloned().unwrap_or_default();
        let label_overview = key.starts_with("label:");
        let label_releases = key.starts_with("label-releases:");
        let by_popularity = app.get_grid_label_popular();
        app.set_grid_title(title.into());
        app.set_grid_tabs(ModelRc::default());
        app.set_grid_count(0);
        app.set_grid_cards(ModelRc::default());
        app.set_grid_create(false);
        app.set_grid_label(label_overview);
        app.set_grid_label_sortable(label_releases);
        app.set_grid_label_releases(ModelRc::default());
        app.set_grid_label_artists(ModelRc::default());
        {
            let mut st = self.st.borrow_mut();
            st.grid.clear();
            st.grid_tab = 0;
        }
        let Some(net) = self.st.borrow().net.clone() else { return };
        let cached = self.st.borrow().cards.get(key.split(':').next().unwrap_or("")).cloned().unwrap_or_default();
        std::thread::spawn(move || {
            let (block, arg) = key.split_once(':').unwrap_or((key.as_str(), ""));
            let one = |r: Result<Vec<Card>, String>| r.map(|c| vec![(String::new(), c)]);
            // Заголовок подборки — название тега из ответа, как у TagPage оригинала.
            let mut title = None;
            let res = match block {
                "tag" => one(net.api.tag_playlists(arg).map(|(t, c)| {
                    title = Some(t).filter(|t| !t.is_empty());
                    c
                })),
                "playlists" => net.api.collection_tabs("playlists-liked-and-playlists-created"),
                "albums" => net.api.collection_tabs("albums-liked-and-albums-presaved"),
                "label" => (|| {
                    Ok(vec![
                        (String::new(), net.api.label_albums(arg, 12, false)?),
                        (String::new(), net.api.label_artists(arg, 12)?),
                    ])
                })(),
                "label-releases" => one(net.api.label_albums(arg, 100, by_popularity)),
                "label-artists" => one(net.api.label_artists(arg, 100)),
                // Как у оригинала: «Студийные альбомы» ведут на discography-albums, а не на все альбомы.
                "ar-studio" => one(net.api.artist_albums(arg, "discography-albums")),
                "ar-albums" => one(net.api.artist_albums(arg, "direct-albums")),
                "ar-compilations" => one(net.api.artist_albums(arg, "also-albums")),
                "ar-similar" => one(net.api.similar_artists(arg)),
                "entities" => one(net.api.block_entities(arg)),
                "disliked-artists" => one(net.api.disliked_artists(&net.uid).map(|list| list.iter().map(crate::api::artist_card).collect())),
                _ => Ok(vec![(String::new(), cached)]),
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|c| {
                    let ui = c.ui();
                    let app = ui.global::<App>();
                    if !c.same_session(&net) || app.get_page() != Page::Grid || c.st.borrow().grid_key.as_deref() != Some(key.as_str())
                        || (label_releases && app.get_grid_label_popular() != by_popularity) {
                        return;
                    }
                    match res {
                        Ok(tabs) if label_overview => {
                            let mut cards = tabs.into_iter().map(|(_, cards)| cards);
                            app.set_grid_label_releases(c.card_model("label-releases", &cards.next().unwrap_or_default(), 191.0));
                            app.set_grid_label_artists(c.card_model("label-artists", &cards.next().unwrap_or_default(), 191.0));
                        }
                        Ok(tabs) => {
                            if let Some(t) = title {
                                c.ui().global::<App>().set_grid_title(t.as_str().into());
                                c.st.borrow_mut().grid_titles.insert(key.clone(), t);
                            }
                            c.st.borrow_mut().grid = tabs;
                            c.show_grid_tab();
                        }
                        Err(e) => c.toast(format!("Не удалось загрузить: {e}")),
                    }
                })
            });
        });
    }

    fn show_grid_tab(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let (names, count, tab, playlists) = {
            let mut st = self.st.borrow_mut();
            let tab = st.grid_tab.min(st.grid.len().saturating_sub(1));
            st.grid_tab = tab;
            // Счётчик — только у выбранной вкладки: «Альбомы • 54».
            let names: Vec<SharedString> = st
                .grid
                .iter()
                .enumerate()
                .map(|(i, (t, items))| if i == tab { format!("{t} • {}", items.len()) } else { t.clone() }.into())
                .collect();
            let count = st.grid.get(tab).map_or(0, |t| t.1.len());
            (names, count, tab, st.grid_key.as_deref() == Some("playlists"))
        };
        app.set_grid_tabs(ModelRc::from(Rc::new(VecModel::from(names))));
        app.set_grid_tab(tab as i32);
        app.set_grid_create(playlists && tab == 0);
        app.set_grid_count(count as i32);
        self.show_grid_window(false);
    }

    fn show_grid_window(&self, scrolling: bool) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let (first, capacity) = self.grid_window.get();
        let cards = {
            let st = self.st.borrow();
            let cards = st.grid.get(st.grid_tab).map(|(_, cards)| cards.as_slice()).unwrap_or_default();
            cards.iter().skip(first).take(capacity).cloned().collect::<Vec<_>>()
        };
        self.images.cancel_pending();
        let previous = if scrolling { app.get_grid_cards() } else { ModelRc::default() };
        let previous_first = app.get_grid_first();
        let rows = cards.iter().enumerate().map(|(i, item)| {
            previous_row(&previous, previous_first, first + i).unwrap_or_else(|| card_item(item))
        }).collect::<Vec<_>>();
        let model = Rc::new(VecModel::from(rows));
        app.set_grid_first(first as i32);
        app.set_grid_cards(ModelRc::from(model.clone()));
        let size = self.size_for(204.0);
        for (i, card) in cards.iter().enumerate().rev() {
            if model.row_data(i).is_some_and(|row| row.cover.size().width > 0) { continue; }
            if let Some(uri) = card.covers.first() {
                self.viewport_cover(&model, i, &crate::api::pic(uri, size), size, |row, img| row.cover = img);
            }
        }
    }

    /// «Исполнители»: топ месяца из коллекции и понравившиеся — отдельным запросом.
    fn refresh_my_artists(self: &Rc<Self>) {
        self.show_my_top();
        let Some(net) = self.st.borrow().net.clone() else { return };
        let uid = self.st.borrow().uid.clone();
        std::thread::spawn(move || match net.api.liked_artists(&uid) {
            Ok(list) => {
                let _ = slint::invoke_from_event_loop(move || {
                    with_ctx(|c| {
                        if !c.same_session(&net) || c.ui().global::<App>().get_page() != Page::Artists {
                            return;
                        }
                        let cards: Vec<Card> = list
                            .iter()
                            .map(|a| Card {
                                title: a.name.clone(),
                                subtitle: String::new(),
                                year: String::new(),
                                likes: None,
                                covers: a.cover.iter().cloned().collect(),
                                round: true,
                                explicit: false,
                                target: format!("artist:{}", a.id),
                            })
                            .collect();
                        let model = c.card_model("my-artists", &cards, 178.0);
                        c.ui().global::<App>().set_my_artists(model);
                    })
                });
            }
            Err(e) => net.say(format!("Не удалось загрузить исполнителей: {e}")),
        });
    }

    fn show_my_top(self: &Rc<Self>) {
        let Some(data) = self.collection() else { return };
        if let Data::Collection(c) = &*data {
            self.set_top_artists(c);
        }
    }

    fn refresh(self: &Rc<Self>) {
        let said = self.st.borrow().net.as_ref().and_then(|n| n.toast.lock().unwrap().take());
        if let Some((msg, _)) = said {
            self.toast(msg);
        }
        match self.ui().global::<App>().get_page() {
            Page::List => self.refresh_list(),
            Page::Collection => self.refresh_collection(),
            Page::Search => {
                self.refresh_search();
                self.refresh_results();
            }
            Page::Artist => self.refresh_artist(),
            Page::Artists => self.show_my_top(),
            Page::Trends | Page::Concerts | Page::Nonmusic | Page::Landing => self.refresh_landing(),
            Page::Chart => self.refresh_chart(),
            Page::History => self.refresh_history(),
            _ => {}
        }
        if self.ui().global::<App>().get_menu_open() {
            self.load_menu_playlists();
        }
    }

    fn refresh_collection(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let Some(net) = self.st.borrow().net.clone() else { return };
        match net.get("collection") {
            None => app.set_coll_loading(true),
            Some(Err(e)) => {
                app.set_coll_loading(false);
                self.toast(format!("Не удалось загрузить коллекцию: {e}"));
            }
            Some(Ok(data)) => {
                app.set_coll_loading(false);
                if self.st.borrow().coll_shown.as_ref().is_some_and(|d| Arc::ptr_eq(d, &data)) {
                    return;
                }
                let previous = self.st.borrow_mut().coll_shown.replace(data.clone());
                if let Data::Collection(c) = &*data {
                    self.show_likes_preview(c);
                    if let Some(Data::Collection(old)) = previous.as_deref() {
                        if old.likes_cover == c.likes_cover && Arc::ptr_eq(&old.top_artists, &c.top_artists)
                            && Arc::ptr_eq(&old.playlist_tabs, &c.playlist_tabs) && Arc::ptr_eq(&old.album_tabs, &c.album_tabs) {
                            return;
                        }
                    }
                    self.show_collection(c);
                }
            }
        }
    }

    fn collection(&self) -> Option<Arc<Data>> {
        if let Some(d) = self.st.borrow().coll_shown.clone() {
            return Some(d);
        }
        let net = self.st.borrow().net.clone()?;
        net.get("collection")?.ok()
    }

    fn load_cover<T: Clone + 'static>(&self, model: &Rc<VecModel<T>>, i: usize, uri: Option<&str>, css: f32, set: fn(&mut T, Image)) {
        let Some(uri) = uri else { return };
        let size = self.size_for(css);
        let m = model.clone();
        self.images.get(&crate::api::pic(uri, size), size, move |img| {
            if let Some(mut row) = m.row_data(i) {
                set(&mut row, img.clone());
                m.set_row_data(i, row);
            }
        });
    }

    fn show_likes_preview(&self, c: &Collection) {
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_likes_subtitle(tracks_word(c.likes_count).into());
        let preview = Rc::new(VecModel::from(c.likes_preview.iter().map(|t| self.track_item(t)).collect::<Vec<_>>()));
        app.set_likes_preview(ModelRc::from(preview.clone()));
        for (i, t) in c.likes_preview.iter().enumerate() {
            self.load_cover(&preview, i, t.cover.as_deref(), 40.0, |r, img| r.cover = img);
        }
    }

    fn show_collection(self: &Rc<Self>, c: &Collection) {
        let ui = self.ui();
        let app = ui.global::<App>();
        if let Some(uri) = &c.likes_cover {
            let size = self.size_for(58.0);
            let w = self.ui.clone();
            self.images.get(&crate::api::pic(uri, size), size, move |img| {
                if let Some(ui) = w.upgrade() {
                    ui.global::<App>().set_likes_cover(img.clone());
                }
            });
        }

        self.set_top_artists(c);

        let names = |tabs: Vec<SharedString>| ModelRc::from(Rc::new(VecModel::from(tabs)));
        app.set_playlist_tabs(names(c.playlist_tabs.iter().map(|t| SharedString::from(t.0.as_str())).collect()));
        app.set_album_tabs(names(c.album_tabs.iter().map(|t| SharedString::from(t.0.as_str())).collect()));
        self.show_playlist_tab();
    }

    fn set_top_artists(self: &Rc<Self>, c: &crate::api::Collection) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let top = Rc::new(VecModel::from(
            c.top_artists
                .iter()
                .map(|a| TopArtistItem {
                    name: a.artist.name.clone().into(),
                    cover: Image::default(),
                    rank: a.position.to_string().into(),
                    trend: match (a.position, a.progress.as_str()) {
                        (1, _) => 0,
                        (_, "up") => 2,
                        (_, "down") => 3,
                        (_, "new") => 4,
                        _ => 1,
                    },
                    time: listen_time(a.seconds).into(),
                })
                .collect::<Vec<_>>(),
        ));
        app.set_top_artists(ModelRc::from(top.clone()));
        for (i, a) in c.top_artists.iter().enumerate() {
            self.load_cover(&top, i, a.artist.cover.as_deref(), 191.0, |r, img| r.cover = img);
        }
        self.show_album_tab();
    }

    fn show_playlist_tab(self: &Rc<Self>) {
        let Some(data) = self.collection() else { return };
        let Data::Collection(c) = &*data else { return };
        let tab = self.st.borrow().pl_tab.min(c.playlist_tabs.len().saturating_sub(1));
        let pls = c.playlist_tabs.get(tab).map(|t| t.1.clone()).unwrap_or_default();
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_playlist_tab(tab as i32);
        let model = Rc::new(VecModel::from(
            pls.iter()
                .map(|p| CardItem { title: p.title.clone().into(), ..Default::default() })
                .collect::<Vec<_>>(),
        ));
        app.set_playlists(ModelRc::from(model.clone()));
        for (i, p) in pls.iter().enumerate() {
            self.load_cover(&model, i, p.covers.first().map(|s| s.as_str()), 186.0, |r, img| r.cover = img);
        }
        self.st.borrow_mut().playlists = pls;
    }

    fn show_album_tab(self: &Rc<Self>) {
        let Some(data) = self.collection() else { return };
        let Data::Collection(c) = &*data else { return };
        let tab = self.st.borrow().album_tab.min(c.album_tabs.len().saturating_sub(1));
        let albums = c.album_tabs.get(tab).map(|t| t.1.clone()).unwrap_or_default();
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_album_tab(tab as i32);
        let model = Rc::new(VecModel::from(
            albums
                .iter()
                .map(|a| CardItem { title: a.title.clone().into(), subtitle: a.artists.clone().into(), ..Default::default() })
                .collect::<Vec<_>>(),
        ));
        app.set_albums(ModelRc::from(model.clone()));
        for (i, a) in albums.iter().enumerate() {
            self.load_cover(&model, i, a.cover.as_deref(), 186.0, |r, img| r.cover = img);
        }
        self.st.borrow_mut().albums = albums;
    }

    fn track_item(&self, t: &Track) -> TrackItem {
        TrackItem {
            id: t.id.as_str().into(),
            title: t.title.clone().into(),
            version: t.version.clone().into(),
            artists: t.artists.clone().into(),
            duration: mm_ss(t.duration_ms).into(),
            cover: Image::default(),
            explicit: t.explicit,
            available: true,
            artist_names: ModelRc::from(Rc::new(VecModel::from(
                t.artist_list.iter().map(|a| SharedString::from(a.name.as_str())).collect::<Vec<_>>(),
            ))),
        }
    }

    fn refresh_list(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let (net, key) = {
            let st = self.st.borrow();
            (st.net.clone(), st.list_key.clone())
        };
        let (Some(net), Some(key)) = (net, key) else {
            app.set_list_loading(true);
            return;
        };
        match net.get(&key) {
            None => app.set_list_loading(true),
            Some(Err(e)) => {
                app.set_list_loading(false);
                self.toast(format!("Не удалось загрузить: {e}"));
            }
            Some(Ok(data)) => {
                app.set_list_loading(false);
                if self.st.borrow().list_shown.as_ref().is_some_and(|d| Arc::ptr_eq(d, &data)) {
                    return;
                }
                self.st.borrow_mut().list_shown = Some(data.clone());
                if let Data::List(l) = &*data {
                    self.st.borrow_mut().search_index = l.tracks.iter().map(|t| (t.title.to_lowercase(), t.artists.to_lowercase())).collect();
                    self.show_list_header(l);
                    self.apply_filter();
                }
            }
        }
    }

    fn show_list_header(self: &Rc<Self>, l: &ListData) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let soft = |c: [u8; 3]| Color::from_argb_u8(38, c[0], c[1], c[2]);
        app.set_list_desc(SharedString::new());
        app.set_list_year(SharedString::new());
        app.set_list_likes(SharedString::new());
        app.set_list_label(SharedString::new());
        app.set_list_label_id(SharedString::new());
        app.set_list_own(false);
        app.set_list_artist_cover(Image::default());
        app.set_list_tint(Color::from_argb_u8(0, 0, 0, 0));
        match l.kind {
            ListKind::Likes => {
                app.set_list_kind("likes".into());
                app.set_list_kind_label(crate::i18n::tr("Плейлист").into());
                app.set_list_meta(app.get_login());
                app.set_list_tint(Color::from_rgb_u8(217, 217, 217));
            }
            ListKind::Artist => {
                app.set_list_kind("artist".into());
                app.set_list_kind_label(crate::i18n::tr("Популярные треки").into());
                app.set_list_meta(tracks_word(l.tracks.len() as u64).into());
                if let Some(c) = l.artist.as_ref().and_then(|a| a.color) {
                    app.set_list_tint(soft(c));
                }
            }
            ListKind::Album => {
                let a = l.album.as_ref();
                app.set_list_kind("album".into());
                app.set_list_kind_label(crate::i18n::tr(album_kind_word(a.map(|a| a.kind.as_str()).unwrap_or(""))).into());
                app.set_list_meta(a.map(|a| a.artists.clone()).unwrap_or_default().into());
                let names: Vec<SharedString> = a.map(|a| a.artist_list.iter().map(|x| SharedString::from(x.name.as_str())).collect()).unwrap_or_default();
                app.set_list_artists(ModelRc::from(Rc::new(VecModel::from(names))));
                app.set_list_year(a.and_then(|a| a.year).map(|y| y.to_string()).unwrap_or_default().into());
                app.set_list_likes(a.map(|a| group_digits(a.likes)).unwrap_or_default().into());
                app.set_list_label(a.map(|a| a.label.clone()).unwrap_or_default().into());
                app.set_list_label_id(a.and_then(|a| a.label_id).map(|id| id.to_string()).unwrap_or_default().into());
                app.set_list_liked(a.is_some_and(|a| self.st.borrow().liked_albums.contains(&a.id)));
                if let Some(c) = l.tracks.first().and_then(|t| t.color) {
                    app.set_list_tint(soft(c));
                }
                if let Some(uri) = a.and_then(|a| a.artist_cover.clone()) {
                    let size = self.size_for(20.0);
                    let w = self.ui.clone();
                    self.images.get(&crate::api::pic(&uri, size), size, move |img| {
                        if let Some(ui) = w.upgrade() {
                            ui.global::<App>().set_list_artist_cover(img.clone());
                        }
                    });
                }
            }
            _ => {
                let p = l.playlist.as_ref();
                let date = p.map(|p| date_ru(&p.modified)).unwrap_or_default();
                let mine = p.is_some_and(|p| p.uid == self.st.borrow().uid);
                let raw = p.map(|p| p.description.clone()).unwrap_or_default();
                app.set_list_desc_raw(raw.clone().into());
                let desc = match raw {
                    d if d.is_empty() && mine => format!("{} ...", crate::i18n::tr("Добавить описание")),
                    d => d,
                };
                app.set_list_kind("playlist".into());
                app.set_list_kind_label(crate::i18n::tr("Плейлист").into());
                app.set_list_own(mine);
                if let Some(p) = p {
                    let key = format!("playlist:{}:{}", p.uid, p.kind);
                    app.set_list_liked(self.st.borrow().liked_playlists.contains(&key));
                    if p.likes > 0 {
                        app.set_list_likes(group_digits(p.likes).into());
                    }
                }
                app.set_list_desc(desc.into());
                app.set_list_meta(
                    match p {
                        Some(p) if !date.is_empty() => format!("{} обновил плейлист {date}", p.owner),
                        Some(p) => p.owner.clone(),
                        None => String::new(),
                    }
                    .into(),
                );
                if let Some(c) = p.and_then(|p| p.color) {
                    app.set_list_tint(soft(c));
                }
            }
        }
        app.set_list_title(l.title.clone().into());
        if l.playlist.is_some() && self.st.borrow().rename_key.is_some() && self.st.borrow().rename_key == self.st.borrow().list_key {
            self.st.borrow_mut().rename_key = None;
            app.set_list_edit_request(1);
        }
        app.set_pinned(self.list_target().is_some_and(|t| self.is_pinned(&t)));
        if let Some(uri) = l.covers.first() {
            let size = self.size_for(204.0);
            let weak = self.ui.clone();
            let key = self.st.borrow().list_key.clone();
            self.images.get(&crate::api::pic(uri, size), size, move |img| {
                with_ctx(|c| {
                    if c.st.borrow().list_key == key {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<App>().set_list_cover(img.clone());
                        }
                    }
                })
            });
        }
    }

    fn list(&self) -> Option<Arc<Data>> {
        self.st.borrow().list_shown.clone()
    }

    fn apply_filter(self: &Rc<Self>) {
        let Some(data) = self.list() else { return };
        let Data::List(l) = &*data else { return };
        let ui = self.ui();
        let app = ui.global::<App>();
        let query = app.get_list_filter().to_lowercase();
        let view = if query.is_empty() {
            (0..l.tracks.len()).collect::<Vec<_>>()
        } else {
            self.st.borrow().search_index.iter().enumerate()
                .filter(|(_, (title, artists))| title.contains(&query) || artists.contains(&query))
                .map(|(i, _)| i).collect()
        };
        let count = view.len();
        self.st.borrow_mut().view = view;
        app.set_list_count(count as i32);
        self.show_list_window(false);
        self.update_list_current();
    }

    fn view_track(&self, i: usize) -> Option<Track> {
        let data = self.list()?;
        let Data::List(l) = &*data else { return None };
        let idx = *self.st.borrow().view.get(i)?;
        l.tracks.get(idx).cloned()
    }

    fn list_id(l: &ListData) -> String {
        match (&l.playlist, &l.album, &l.artist) {
            (Some(p), _, _) => format!("{}:{}", p.uid, p.kind),
            (_, Some(a), _) => a.id.clone(),
            (_, _, Some(a)) => a.id.clone(),
            _ => "likes".into(),
        }
    }

    fn update_list_current(&self) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let s = self.player.state();
        let cur = (|| {
            let data = self.list()?;
            let Data::List(l) = &*data else { return None };
            let Source::List { id, .. } = &s.source else { return None };
            if *id != Self::list_id(l) {
                return None;
            }
            let tid = &s.track.as_ref()?.id;
            let st = self.st.borrow();
            st.view.iter().position(|&i| l.tracks[i].id == *tid)
        })();
        app.set_list_current(cur.map(|i| i as i32).unwrap_or(-1));
    }

    fn show_list_window(&self, scrolling: bool) {
        let Some(data) = self.list() else { return };
        let Data::List(list) = &*data else { return };
        let ui = self.ui();
        let app = ui.global::<App>();
        let (first, capacity) = self.list_window.get();
        let st = self.st.borrow();
        let tracks: Vec<_> = st.view.iter().skip(first).take(capacity).map(|&i| &list.tracks[i]).collect();
        drop(st);
        let previous = if scrolling { app.get_tracks() } else { ModelRc::default() };
        let previous_first = app.get_list_first();
        let rows = tracks.iter().enumerate().map(|(i, item)| {
            previous_row(&previous, previous_first, first + i).unwrap_or_else(|| self.track_item(item))
        }).collect::<Vec<_>>();
        let model = Rc::new(VecModel::from(rows));
        self.images.cancel_pending();
        app.set_list_first(first as i32);
        app.set_tracks(ModelRc::from(model.clone()));
        let size = self.size_for(40.0);
        for (i, track) in tracks.iter().enumerate().rev() {
            if model.row_data(i).is_some_and(|row| row.cover.size().width > 0) { continue; }
            if let Some(url) = track.cover_url(size) {
                self.viewport_cover(&model, i, &url, size, |row, img| row.cover = img);
            }
        }
    }

    fn viewport_cover<T: Clone + 'static>(&self, model: &Rc<VecModel<T>>, i: usize, url: &str, size: u32, set: impl FnOnce(&mut T, Image) + 'static) {
        let weak = Rc::downgrade(model);
        self.images.get_cancellable(url, size, move |img| {
            let Some(model) = weak.upgrade() else { return };
            if let Some(mut row) = model.row_data(i) {
                set(&mut row, img.clone());
                model.set_row_data(i, row);
            }
        });
    }

    fn play_list(self: &Rc<Self>, i: usize) {
        let Some(data) = self.list() else { return };
        let Data::List(l) = &*data else { return };
        if l.tracks.is_empty() {
            return;
        }
        if self.ui().global::<App>().get_list_current() == i as i32 {
            self.player.send(Cmd::PlayPause);
            return;
        }
        let start = self.st.borrow().view.get(i).copied().unwrap_or(0);
        let wave = match (&l.playlist, &l.album, &l.artist) {
            (Some(p), _, _) => Source::Seed { station: p.seed(), header: "Моя волна по плейлисту".into(), title: p.title.clone() },
            (_, Some(a), _) => Source::Seed { station: format!("album:{}", a.id), header: "Моя волна по альбому".into(), title: a.title.clone() },
            (_, _, Some(a)) => Source::Seed { station: format!("artist:{}", a.id), header: "Моя волна по исполнителю".into(), title: a.name.clone() },
            _ => Source::Wave,
        };
        self.player.send(Cmd::PlayList {
            source: Source::List { kind: l.kind, id: Self::list_id(l), title: l.title.clone() },
            tracks: l.tracks.clone(),
            start,
            after: After::Wave(wave),
        });
        self.start();
    }

    /// Панель настроек «Моей волны»: варианты приходят с сервера, выбранные — из последней волны
    /// в любом клиенте.
    fn open_wave_settings(self: &Rc<Self>) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<App>().set_wave_settings_open(true);
        }
        let activity = self.player.state().source.station().filter(|s| s.starts_with("activity:")).map(String::from);
        std::thread::spawn(move || {
            let groups = net.api.wave_setting_groups();
            let mut current = net.api.wave_settings();
            current.extend(activity);
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|c| match groups {
                    Ok(groups) if c.same_session(&net) => c.set_wave_settings(groups, &current),
                    Ok(_) => {}
                    Err(e) => log(format!("rotor/wave/settings: {e}")),
                })
            });
        });
    }

    fn set_wave_settings(self: &Rc<Self>, groups: Vec<crate::api::WaveSettingGroup>, current: &[String]) {
        let tune: Vec<_> = groups
            .into_iter()
            .map(|g| {
                let activities = g.options.iter().any(|o| is_activity(&o.seed));
                let chosen = g
                    .options
                    .iter()
                    .position(|o| current.contains(&o.seed))
                    .or_else(|| g.options.iter().position(|o| o.any))
                    .unwrap_or(if activities { NOTHING } else { 0 });
                let images = vec![Image::default(); g.options.len()];
                (g, chosen, images)
            })
            .collect();
        let urls: Vec<(usize, usize, String)> = tune
            .iter()
            .enumerate()
            .flat_map(|(gi, (g, _, _))| g.options.iter().enumerate().filter_map(move |(oi, o)| Some((gi, oi, o.image.clone()?))))
            .collect();
        self.st.borrow_mut().wave_tune = tune;
        self.show_wave_settings();
        for (gi, oi, url) in urls {
            let expected = url.clone();
            self.images.get(&url, 48, move |img| {
                with_ctx(|c| {
                    let mut st = c.st.borrow_mut();
                    let Some((group, _, images)) = st.wave_tune.get_mut(gi) else { return };
                    if group.options.get(oi).and_then(|o| o.image.as_ref()) != Some(&expected) { return; }
                    images[oi] = img.clone();
                    drop(st);
                    c.show_wave_settings();
                })
            });
        }
    }

    fn show_wave_settings(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let groups: Vec<WaveSettingGroup> = self
            .st
            .borrow()
            .wave_tune
            .iter()
            .map(|(g, chosen, images)| WaveSettingGroup {
                name: g.name.as_str().into(),
                options: ModelRc::new(VecModel::from(
                    g.options
                        .iter()
                        .zip(images)
                        .enumerate()
                        .map(|(i, (o, image))| WaveOption { name: o.name.as_str().into(), image: image.clone(), selected: i == *chosen })
                        .collect::<Vec<_>>(),
                )),
            })
            .collect();
        ui.global::<App>().set_wave_settings(ModelRc::new(VecModel::from(groups)));
    }

    /// Пользователь что-то включил: показать плеер и снять с паузы.
    fn start(self: &Rc<Self>) {
        self.st.borrow_mut().started = true;
        self.player.send(Cmd::Resume);
        self.st.borrow_mut().track_id = None;
        self.sync_player();
    }

    fn set_wheel(self: &Rc<Self>, items: Vec<crate::api::WheelItem>) {
        let model = Rc::new(VecModel::from(
            items
                .iter()
                .map(|w| WheelEntry {
                    name: w.name.clone().into(),
                    description: w.description.clone().into(),
                    cover: Image::default(),
                    round: w.round,
                    control_accent: w.control_accent,
                })
                .collect::<Vec<_>>(),
        ));
        self.ui().global::<App>().set_wheel(ModelRc::from(model.clone()));
        for (i, w) in items.iter().enumerate() {
            self.load_cover(&model, i, w.cover.as_deref(), 80.0, |r, img| r.cover = img);
        }
        self.st.borrow_mut().wheel = items;
    }

    /// Колесо зависит от играющей волны: сменилась волна — спросить его заново.
    fn sync_wheel(self: &Rc<Self>, source: &Source) {
        let seeds = match source {
            Source::Wave => std::iter::once(crate::api::WAVE.to_string()).chain(self.st.borrow().tune_seeds.iter().cloned()).collect(),
            Source::Seed { station, .. } => vec![station.clone()],
            Source::List { .. } => Vec::new(),
        };
        let Some(net) = self.st.borrow().net.clone() else { return };
        if self.st.borrow().wheel_seeds.as_ref() == Some(&seeds) {
            return;
        }
        self.st.borrow_mut().wheel_seeds = Some(seeds.clone());
        std::thread::spawn(move || {
            let wheel = net.api.wheel(&seeds);
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|c| match wheel {
                    Ok(w) if c.same_session(&net) && c.st.borrow().wheel_seeds.as_ref() == Some(&seeds) => c.set_wheel(w),
                    Ok(_) => {}
                    Err(e) => log(format!("wheel: {e}")),
                })
            });
        });
    }

    fn play_wheel(self: &Rc<Self>, i: usize) {
        let Some(w) = self.st.borrow().wheel.get(i).cloned() else { return };
        self.st.borrow_mut().tune_seeds.clear();
        let source = match w.station() {
            Some(station) => Source::Seed { station, header: "Моя волна".into(), title: w.name.clone() },
            None => Source::Wave,
        };
        self.player.send(Cmd::Wave(source));
        self.start();
    }

    fn open_pin(self: &Rc<Self>, i: usize) {
        let Some(pin) = self.st.borrow().pins.get(i).cloned() else { return };
        let (kind, arg) = pin.target.split_once(':').unwrap_or(("", ""));
        match kind {
            "album" | "playlist" => self.navigate(Page::List, Some(pin.target.clone()), true),
            "station" => {
                self.player.send(Cmd::Wave(Source::Seed { station: arg.to_string(), header: pin.subtitle.clone(), title: pin.title.clone() }));
                self.start();
                self.navigate(Page::Wave, None, true);
            }
            "artist" => self.navigate(Page::Artist, Some(pin.target.clone()), true),
            _ => {}
        }
    }
}

/// Масштаб как у оригинала: начиная с 1920 CSS-пикселей ширины корневой шрифт
/// `0.75rem + 0.3vw`, и всё, что задано в rem, растёт вместе с ним.
fn apply_ui_scale(ui: &AppWindow) {
    rescale(ui, false);
}

/// `force` — после переноса на монитор с другим масштабом: размер и масштаб окна пересылаются
/// Slint заново, даже если масштаб совпал, и окно перерисовывается целиком. Иначе после
/// переноса могла остаться недорисованная часть окна (#16).
fn rescale(ui: &AppWindow, force: bool) {
    let w = ui.window();
    #[cfg(windows)]
    let phys = w.with_winit_window(|ww| {
        let size = ww.inner_size();
        slint::PhysicalSize::new(size.width, size.height)
    }).unwrap_or_else(|| w.size());
    #[cfg(not(windows))]
    let phys = w.size();
    if phys.width == 0 || phys.height == 0 {
        return;
    }
    let base = std::env::var("SLINT_SCALE_FACTOR").ok().and_then(|s| s.parse::<f32>().ok()).unwrap_or_else(|| sys::scale(ui.window()));
    let css_w = phys.width as f32 / base;
    let k = if css_w >= 1920.0 { (12.0 + 0.003 * css_w) / 16.0 } else { 1.0 };
    let target = base * k;
    ui.global::<App>().set_ui_scale(k);
    ui.global::<App>().set_dpr(target);
    if !force && (w.scale_factor() - target).abs() < 0.001 {
        return;
    }
    w.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: target });
    w.dispatch_event(slint::platform::WindowEvent::Resized { size: phys.to_logical(target) });
    if force {
        full_repaint(ui);
    }
}

/// Перерисовать окно целиком, а не только изменившиеся места. Кадр выводится в окно через GDI,
/// и часть окна за краем экрана при этом отсекается: если окно потом вернуть на экран, там
/// осталась бы пустота или старый кадр (#16).
fn full_repaint(ui: &AppWindow) {
    ui.global::<App>().set_repaint(true);
    let weak = ui.as_weak();
    Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<App>().set_repaint(false);
        }
    });
}

#[cfg(target_os = "linux")]
thread_local! {
    static LAST_CURSOR: std::cell::Cell<slint::LogicalPosition> = const { std::cell::Cell::new(slint::LogicalPosition::new(0.0, 0.0)) };
}

/// Окно двигает и тянет за край сама система, и отпускание кнопки до Slint не доходит.
/// Slint так и считает левую зажатой и отдаёт все клики полосе заголовка, а та снова начинает
/// перетаскивание (#17). То же происходит на Windows после Win32-перетаскивания и Snap.
/// Отпускаем кнопку за него. Не прямо здесь: мы внутри обработки нажатия.
fn release_after_system_drag(ui: &AppWindow) {
    let weak = ui.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        if let Some(ui) = weak.upgrade() {
            #[cfg(target_os = "linux")]
            let position = LAST_CURSOR.get();
            // За пределами заголовка: сброс захвата не должен считаться кликом по нему.
            #[cfg(not(target_os = "linux"))]
            let position = slint::LogicalPosition::new(-1.0, -1.0);
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        }
    });
}

/// После переключения режима ограничения Slint и события Windows применяются не сразу.
/// Повторно согласуем стиль, геометрию и захват мыши после применения этих изменений.
fn after_window_mode_change(ui: &AppWindow) {
    #[cfg(windows)]
    for ms in [0, 150] {
        let weak = ui.as_weak();
        Timer::single_shot(Duration::from_millis(ms), move || {
            let Some(ui) = weak.upgrade() else { return };
            sys::set_mini_mode(ui.window(), ui.global::<App>().get_mini());
            rescale(&ui, true);
            ui.global::<App>().set_maximized(sys::is_maximized(ui.window()));
            release_after_system_drag(&ui);
        });
    }
    #[cfg(not(windows))]
    let _ = ui;
}

fn window_maximized(w: &slint::Window) -> bool {
    #[cfg(windows)]
    { sys::is_maximized(w) }
    #[cfg(not(windows))]
    { w.is_maximized() }
}

thread_local! {
    static MOVED: Timer = Timer::default();
}

/// Окно сдвинули: перерисовать целиком, когда его перестанут двигать.
fn after_move() {
    MOVED.with(|t| {
        t.start(TimerMode::SingleShot, Duration::from_millis(150), || with_ctx(|c| full_repaint(&c.ui())));
    });
}

/// Окно перенесли на монитор с другим масштабом. Пока его тащат, Windows может прислать
/// смену масштаба несколько раз, поэтому пересчёт ещё раз — когда всё уляжется.
fn after_dpi_change() {
    for ms in [0, 300] {
        Timer::single_shot(Duration::from_millis(ms), || with_ctx(|c| rescale(&c.ui(), true)));
    }
}

const DONATE_URL: &str = "https://pay.cloudtips.ru/p/befb620d";

/// Чат поддержки, как `createSupportChatUrl` оригинала.
const SUPPORT_CHAT: &str =
    "https://yandex.ru/chat?context=%7B%22entrypoint%22%3A%22web_desktop%22%7D#/user/ee219503-b69b-9484-405b-6877a510e13c";

fn quit() {
    #[cfg(windows)]
    crate::tray::remove();
    let _ = slint::quit_event_loop();
}

/// Обычное окно перед мини-плеером, место плашки и последний размер неразвёрнутого окна.
#[derive(Default)]
struct Places {
    normal: Cell<Option<(Option<slint::PhysicalPosition>, slint::PhysicalSize, bool)>>,
    mini: Cell<Option<slint::PhysicalPosition>>,
    restored: Cell<Option<slint::PhysicalSize>>,
}

/// Место окна на экране; Wayland его приложению не сообщает.
fn window_pos(w: &slint::Window) -> Option<slint::PhysicalPosition> {
    w.with_winit_window(|ww| ww.outer_position().ok()).flatten().map(|p| slint::PhysicalPosition::new(p.x, p.y))
}

fn save_places(ui: &AppWindow, places: &Places) {
    let w = ui.window();
    if !sys::created(w) || w.is_minimized() {
        return;
    }
    let mini = ui.global::<App>().get_mini();
    if mini {
        places.mini.set(window_pos(w).or(places.mini.get()));
    }
    let normal = if mini { places.normal.get() } else { Some((window_pos(w), w.size(), window_maximized(w))) };
    with_ctx(|c| {
        let mut st = c.st.borrow_mut();
        let cfg = &mut st.cfg;
        if let Some((pos, size, max)) = normal {
            if let Some(p) = pos {
                cfg.window_pos = Some([p.x, p.y]);
            }
            if let Some(s) = if max { places.restored.get() } else { Some(size) } {
                cfg.window_size = Some([s.width, s.height]);
            }
            cfg.window_maximized = max;
        }
        cfg.mini_pos = places.mini.get().map(|p| [p.x, p.y]);
        cfg.save();
    });
}

/// Вернуть окно на экран, если его монитор с прошлого раза отключили.
fn keep_on_screen(w: &slint::Window) {
    let fix = w
        .with_winit_window(|ww| {
            let p = ww.outer_position().ok()?;
            let seen = ww.available_monitors().any(|m| {
                let (mp, ms) = (m.position(), m.size());
                (mp.x..mp.x + ms.width as i32).contains(&(p.x + 100)) && (mp.y..mp.y + ms.height as i32).contains(&(p.y + 20))
            });
            if seen {
                return None;
            }
            let m = ww.primary_monitor().or_else(|| ww.available_monitors().next())?;
            let (mp, ms, s) = (m.position(), m.size(), ww.outer_size());
            let x = mp.x + (ms.width as i32 - s.width as i32).max(0) / 2;
            let y = mp.y + (ms.height as i32 - s.height as i32).max(0) / 2;
            Some(slint::PhysicalPosition::new(x, y))
        })
        .flatten();
    if let Some(p) = fix {
        w.set_position(p);
    }
}

/// Показать окно: при запуске в трей оно ещё не создано, после закрытия в трей — спрятано.
fn reveal(ui: &AppWindow) {
    let first = !sys::created(ui.window());
    if let Err(e) = ui.show() {
        log(format!("окно не показалось: {e}"));
        return;
    }
    finish_reveal(ui.as_weak(), first, 0);
}

/// Режим работы: анимации в интерфейсе и размер кэша глифов Skia.
fn apply_mode(ui: &AppWindow, lite: bool) {
    ui.global::<App>().set_motion(!lite);
    // Кэш глифов по умолчанию 2 МБ. В «Плавном» крупный текст песни и анимация масштаба
    // строки его вытесняют, и тогда весь текст растеризуется заново в каждом кадре.
    skia_safe::graphics::set_font_cache_limit(if lite { 2 << 20 } else { 16 << 20 });
}

/// Окно winit создаётся не сразу после `show()` (до запуска цикла событий и с OpenGL),
/// поэтому масштаб и прочее применяются, когда окно появится.
fn finish_reveal(weak: slint::Weak<AppWindow>, first: bool, tries: u32) {
    let Some(ui) = weak.upgrade() else { return };
    if !sys::created(ui.window()) {
        if tries < 100 {
            Timer::single_shot(Duration::from_millis(20), move || finish_reveal(weak, first, tries + 1));
        }
        return;
    }
    if first {
        apply_ui_scale(&ui);
        sys::prepare(ui.window());
        keep_on_screen(ui.window());
    }
    sys::raise(ui.window());
}

fn track_label_changed(previous: &mut Option<String>, current: Option<&str>) -> bool {
    if previous.as_deref() == current {
        return false;
    }
    *previous = current.map(String::from);
    true
}

#[cfg(test)]
#[test]
fn same_track_after_empty_queue_restores_the_label() {
    let mut previous = None;
    for (id, changed) in [(Some("a"), true), (Some("a"), false), (None, true), (Some("a"), true)] {
        assert_eq!(track_label_changed(&mut previous, id), changed);
    }
}

fn notifier() -> Arc<dyn Fn() + Send + Sync> {
    let pending = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = pending.clone();
        let _ = slint::invoke_from_event_loop(move || {
            pending.store(false, Ordering::Release);
            with_ctx(|c| {
                c.sync_player();
                c.refresh();
            });
        });
    })
}

fn load_assets() -> (Vec<Vec<u8>>, Option<Sprite>) {
    let mut src = crate::assets::Source::open();
    if matches!(src, crate::assets::Source::Web(_)) {
        log("официальное приложение не найдено — шрифты и иконки из веб-версии");
    }
    (crate::assets::load_fonts(&mut src), Sprite::load(&mut src))
}

thread_local! {
    /// Найденный новый релиз — для кнопки «Обновить».
    static UPDATE: RefCell<Option<crate::update::Release>> = const { RefCell::new(None) };
}

/// Спросить GitHub (долго, звать из фонового потока) и показать итог: новый релиз — тост
/// «Доступна версия …», в настройках под «Проверить обновления» — что вышло и когда (#128).
fn check_updates(weak: &slint::Weak<AppWindow>) {
    let res = crate::update::check();
    if let Err(e) = &res {
        log(format!("обновления: {e}"));
    }
    let _ = weak.upgrade_in_event_loop(move |ui| {
        let app = ui.global::<App>();
        let time = chrono::Local::now().format("%H:%M");
        app.set_update_failed(res.is_err());
        let status = match res {
            Ok(Some(r)) => {
                app.set_update_version(r.version.clone().into());
                let status = format!("Проверено в {time}: доступна версия {}", r.version);
                UPDATE.with(|u| *u.borrow_mut() = Some(r));
                status
            }
            Ok(None) => format!("Проверено в {time}: новых версий нет"),
            Err(e) => format!("Ошибка в {time}: {e}"),
        };
        app.set_update_status(status.into());
    });
}

/// «Проверить обновления» в настройках и в трее: спросить сейчас, даже если фоновая проверка
/// выключена. У оригинала пункт трея (`desktop.check-for-updates`) так же зовёт `updater.check()`.
fn check_updates_now(ui: &AppWindow) {
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::Relaxed) {
        return;
    }
    ui.global::<App>().set_update_status("Проверяем…".into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        check_updates(&weak);
        BUSY.store(false, Ordering::Relaxed);
    });
}

/// Проверка обновлений в фоне (первая — через 10 с после запуска) и кнопка «Обновить» в тосте.
fn start_updates(ui: &AppWindow) {
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        loop {
            if !crate::update::ENABLED.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(60));
                continue;
            }
            check_updates(&weak);
            // После отказа по лимиту запросов — когда разрешит GitHub, а не через полчаса.
            std::thread::sleep(crate::update::next_check());
        }
    });
    ui.global::<App>().on_update_changes(|| {
        if let Some(r) = UPDATE.with(|u| u.borrow().clone()) {
            sys::open_url(&crate::update::changes_url(&r.version));
        }
    });
    ui.global::<App>().on_update_install(|| {
        let Some(r) = UPDATE.with(|u| u.borrow().clone()) else { return };
        // В релизе нет exe — обновить можно только руками, ведём на страницу.
        if r.exe.is_none() {
            sys::open_url(&r.page);
            return;
        }
        with_ctx(|c| c.ui().global::<App>().set_update_busy(true));
        std::thread::spawn(move || {
            let res = crate::update::install(&r).and_then(|exe| crate::update::relaunch(&exe));
            let _ = slint::invoke_from_event_loop(move || match res {
                // Новая версия ждёт, пока мы закроемся.
                Ok(()) => quit(),
                Err(e) => {
                    log(format!("обновление: {e}"));
                    with_ctx(|c| {
                        c.ui().global::<App>().set_update_busy(false);
                        c.toast(format!("Не удалось обновить: {e}"));
                    });
                    sys::open_url(&r.page);
                }
            });
        });
    });
}

/// Иконки меню профиля: из кэша сразу, иначе скачиваются в фоне и подставляются, когда готовы.
fn load_id_icons(ui: &AppWindow) {
    fn apply(ui: &AppWindow, svgs: &crate::yandex_id::Svgs) {
        let icons = ui.global::<Icons>();
        let img = |name: &str| {
            svgs.get(name).and_then(|s| Image::load_from_svg_data(s.as_bytes()).ok()).unwrap_or_default()
        };
        icons.set_id_logo(img("logo"));
        icons.set_id_logo_dark(img("logo-dark"));
        icons.set_id_check(img("check"));
        icons.set_id_close(img("close"));
        icons.set_id_account(img("id"));
        icons.set_id_settings(img("settings"));
        icons.set_id_support(img("support"));
        icons.set_id_language(img("language"));
        icons.set_id_appearance(img("appearance"));
        icons.set_id_dark(img("dark"));
        icons.set_id_system(img("system"));
    }
    if let Some(svgs) = crate::yandex_id::cached() {
        apply(ui, &svgs);
        return;
    }
    let weak = ui.as_weak();
    std::thread::spawn(move || match crate::yandex_id::download() {
        Ok(svgs) => {
            crate::yandex_id::save(&svgs);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    apply(&ui, &svgs);
                }
            });
        }
        Err(e) => log(format!("Яндекс ID: {e}")),
    });
}

fn set_icons(ui: &AppWindow, sprite: &Sprite) {
    let icons = ui.global::<Icons>();
    let none = HashMap::new();
    let get = |id: &str| sprite.image(id, &none);
    let light_vars = HashMap::from([("--ym-logo-color-primary-back", "#000"), ("--ym-logo-text-color", "#000"), ("currentColor", "#fe0")]);
    let dark_vars = HashMap::from([("--ym-logo-color-primary-back", "transparent"), ("--ym-logo-text-color", "#ff0"), ("currentColor", "#ff0")]);
    icons.set_logo(sprite.image("musicLogoLeftRu", &light_vars));
    icons.set_logo_dark(sprite.image("musicLogoLeftRu", &dark_vars));
    icons.set_logo_mark(sprite.image("musicLogo", &light_vars));
    icons.set_logo_mark_dark(sprite.image("musicLogo", &dark_vars));
    icons.set_nav_search(get("navigationSearch_xs"));
    icons.set_nav_wave(get("navigationMyVibe_xs"));
    icons.set_nav_trends(get("navigationForYouAndTrends_xs"));
    icons.set_nav_trends_selected(get("navigationForYouAndTrends_selected_xs"));
    icons.set_nav_concerts(get("navigationConcerts_xs"));
    icons.set_nav_concerts_selected(get("navigationConcerts_selected_xs"));
    icons.set_nav_nonmusic(get("navigationNonMusic_xs"));
    icons.set_nav_nonmusic_selected(get("navigationNonMusic_selected_xs"));
    icons.set_nav_collection(get("navigationCollection_xs"));
    icons.set_nav_collection_selected(get("navigationCollection_selected_xs"));
    icons.set_play(get("play_xs"));
    icons.set_pause(get("pause_xs"));
    icons.set_next(get("next_xs"));
    icons.set_prev(get("previous_xs"));
    icons.set_like(get("like_xs"));
    icons.set_liked(get("liked_xs"));
    icons.set_dislike(get("dislike_xs"));
    icons.set_disliked(get("disliked_xs"));
    icons.set_shuffle(get("shuffle_xs"));
    icons.set_repeat(get("repeat_xs"));
    icons.set_repeat_one(get("repeat_one_xs"));
    icons.set_lyrics(get("syncLyrics_xs"));
    icons.set_queue(get("playQueue_xs"));
    icons.set_settings(get("settings_xs"));
    icons.set_volume(get("volume_xs"));
    icons.set_volume_off(get("volumeOff_xs"));
    icons.set_more(get("more_xs"));
    icons.set_search(get("search_xs"));
    icons.set_arrow_left(get("arrowLeft_xs"));
    icons.set_arrow_right(get("arrowRight_xs"));
    icons.set_pin(get("pin_xs"));
    icons.set_trailer(get("trailer_xs"));
    icons.set_explicit(get("explicit_xs"));
    icons.set_plus_badge(get("plus"));
    icons.set_add(get("add_xxs"));
    icons.set_note(get("note_xs"));
    icons.set_more_small(get("more_xxs"));
    icons.set_prev_small(get("previous_xxs"));
    icons.set_next_small(get("next_xxs"));
    icons.set_play_filled(get("play_filled_l"));
    icons.set_pause_filled(get("pause_filled_l"));
    icons.set_play_small(get("play_xxs"));
    icons.set_trailer_small(get("trailer_xxs"));
    icons.set_like_small(get("like_xxs"));
    icons.set_liked_small(get("liked_xxs"));
    icons.set_pin_small(get("pin_xxs"));
    icons.set_pin_filled_small(get("pin_filled_xxs"));
    icons.set_arrow_left_small(get("arrowLeft_xxs"));
    icons.set_arrow_right_small(get("arrowRight_xxs"));
    icons.set_exclamation(get("exclamation_xxxs"));
    icons.set_close_small(get("close_xxs"));
    icons.set_bucket_small(get("bucket_xxs"));
    icons.set_more_xs(get("more_xs"));
    icons.set_play_big(get("play"));
    icons.set_pause_big(get("pause"));
    icons.set_pause_filled_xs(get("pause_filled_xs"));
    icons.set_play_filled_xs(get("play_filled_xs"));
    icons.set_crown(get("crown_xxs"));
    icons.set_chart_up(get("chartUp_xxs"));
    icons.set_chart_down(get("chartDown_xxs"));
    icons.set_chart_new(get("chartNew_xxs"));
    icons.set_chart_same(get("chartSame_xxs"));
    icons.set_add_large(get("add_l"));
    icons.set_pencil(get("pencil_xxs"));
    icons.set_download_small(get("download_xxs"));
    icons.set_fullscreen(get("fullscreen_xs"));
    icons.set_arrow_down(get("arrowDown_xs"));
    icons.set_queue_m(get("playQueue_m"));
    icons.set_lyrics_m(get("syncLyrics_m"));
    icons.set_more_m(get("more_m"));
    icons.set_like_m(get("like_m"));
    icons.set_liked_m(get("liked_m"));
    icons.set_play_m(get("play_m"));
    icons.set_pause_m(get("pause_m"));
    icons.set_repeat_small(get("repeat_xxs"));
    icons.set_repeat_one_small(get("repeat_one_xxs"));
    icons.set_users(get("users_xxxs"));
    icons.set_m_like(get("like_xxs"));
    icons.set_m_liked(get("liked_xxs"));
    icons.set_m_trailer(get("trailer_xxs"));
    icons.set_m_vibe(get("vibe_xxs"));
    icons.set_m_download(get("download_xxs"));
    icons.set_m_play_next(get("playNext_xxs"));
    icons.set_m_play_last(get("playLast_xxs"));
    icons.set_m_dislike(get("dislike_xxs"));
    icons.set_m_disliked(get("disliked_xxs"));
    icons.set_m_add_playlist(get("addToPlaylist_xxs"));
    icons.set_m_lyrics(get("lyrics_xxs"));
    icons.set_m_queue(get("playQueue_xxs"));
    icons.set_m_share(get("share_xxs"));
    icons.set_m_pin(get("pin_xxs"));
    icons.set_m_unpin(get("unpin_xxs"));
    icons.set_m_link(get("link_xxs"));
    icons.set_m_code(get("code_xxs"));
    icons.set_m_album(get("album_xxs"));
    icons.set_m_artist(get("artist_xxs"));
    icons.set_m_info(get("info_xxs"));
    icons.set_m_complain(get("complain_xxs"));
    icons.set_link(get("link_rounded_xxs"));
    icons.set_check(get("check_xxs"));
    icons.set_history_m(get("history_m"));
    icons.set_ruble(get("ruble_xxxs"));
    icons.set_arrow_down_tiny(get("arrowDown_xxxs"));
    icons.set_play_xxs(get("play_xxs"));
    icons.set_play_filled_m(get("play_filled_m"));
    icons.set_like_variant(get("likeVariant_xxs"));
    icons.set_play_filled_xl(get("play_filled_xl"));
    icons.set_edit(get("edit_xxs"));
    icons.set_close_xs(get("close_xs"));
    icons.set_upload(get("upload_xxs"));
}

#[cfg(all(windows, feature = "asio"))]
fn setup_audio_output(app: &App<'_>, configured: Option<&str>) {
    let mut drivers = crate::asio::names(configured);
    let mut names: Vec<SharedString> = std::iter::once(crate::i18n::tr("Системный выход").into())
        .chain(drivers.iter().map(|name| format!("ASIO · {name}").into()))
        .collect();
    let current = configured.map_or(0, |name| {
        let index = drivers.iter().position(|driver| driver == name).unwrap_or_else(|| {
            drivers.push(name.to_string());
            names.push(format!("ASIO · {name} (недоступен)").into());
            drivers.len() - 1
        });
        index as i32 + 1
    });
    app.set_asio_available(true);
    app.set_audio_outputs(ModelRc::new(VecModel::from(names)));
    app.set_audio_output_index(current);
    app.on_select_audio_output(move |index| {
        with_ctx(|ctx| {
            let ui = ctx.ui();
            let app = ui.global::<App>();
            let driver = match index {
                0 => None,
                i if i > 0 && (i as usize) <= drivers.len() => drivers.get(i as usize - 1).cloned(),
                _ => return,
            };
            if app.get_audio_output_pending() { return; }
            app.set_audio_output_pending(true);
            ctx.player.send(Cmd::Output(driver, Box::new(|active, error| {
                let _ = slint::invoke_from_event_loop(move || with_ctx(|ctx| {
                    let result = {
                        let mut state = ctx.st.borrow_mut();
                        if error.is_none() {
                            state.cfg.asio_driver = active.clone();
                            state.cfg.save_to_disk()
                        } else {
                            Ok(())
                        }
                    };
                    let ui = ctx.ui();
                    let app = ui.global::<App>();
                    app.set_audio_output_pending(false);
                    setup_audio_output(&app, active.as_deref());
                    if let Some(message) = error {
                        ctx.toast(message);
                    } else if let Err(e) = result {
                        ctx.toast(format!("Аудиовыход переключён, но выбор не сохранён: {e}"));
                    }
                }));
            })));
        });
    });
}

pub fn run(start_hidden: bool) -> Result<(), slint::PlatformError> {
    let cfg = Config::load();
    crate::api::set_lang(&cfg.lang);
    crate::i18n::set_lang(&cfg.lang);
    let dark = match cfg.theme.as_str() {
        "dark" => true,
        "light" => false,
        _ => !sys::system_light_theme(),
    };
    let ui = AppWindow::new()?;
    ui.global::<Theme>().set_dark(true);
    let lite = cfg.mode == "lite";
    apply_mode(&ui, lite);
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let (mut have_fonts, mut have_sprite) = (false, false);
        for attempt in 0..=30 {
            if attempt > 0 { std::thread::sleep(Duration::from_secs(20)); }
            let (fonts, sprite) = load_assets();
            let update_fonts = !have_fonts && !fonts.is_empty();
            let update_sprite = !have_sprite && sprite.is_some();
            have_fonts |= update_fonts;
            have_sprite |= update_sprite;
            if weak.upgrade_in_event_loop(move |ui| {
                if update_fonts {
                    crate::assets::register_fonts(fonts);
                    ui.global::<App>().set_fonts_loaded(true);
                }
                if update_sprite {
                    if let Some(sprite) = sprite {
                        set_icons(&ui, &sprite);
                        with_ctx(|c| c.refresh_menu_icons());
                    }
                }
            }).is_err() || (have_fonts && have_sprite) { break; }
        }
    });
    load_id_icons(&ui);
    crate::update::ENABLED.store(cfg.check_updates, Ordering::Relaxed);
    start_updates(&ui);

    crate::api::ALLOW_EXPLICIT.store(cfg.allow_explicit, Ordering::Relaxed);
    let player = Player::spawn(notifier(), cfg.asio_driver.clone());
    player.send(Cmd::Volume(cfg.volume));
    if cfg.log_volume {
        player.send(Cmd::Loudness(Loudness::Decibels));
    }
    player.send(Cmd::Crossfade(cfg.crossfade()));
    if let Some(t) = cfg.token.clone() {
        player.send(Cmd::Login(t));
    }

    let app = ui.global::<App>();
    app.set_version(env!("CARGO_PKG_VERSION").into());
    app.set_app_name(env!("APP_NAME").into());
    app.set_theme_mode(cfg.theme.clone().into());
    app.set_lang(cfg.lang.clone().into());
    ui.global::<T>().set_lang(cfg.lang.clone().into());
    ui.global::<T>().on_tr(|s: SharedString| SharedString::from(crate::i18n::tr(s.as_str())));
    app.set_nav_collapsed(cfg.navbar_collapsed);

    let discord = crate::discord::Discord::spawn(
        player.clone(),
        crate::discord::DiscordCfg { enabled: cfg.discord_enabled, app_id: cfg.discord_app_id.clone() },
    );
    app.set_discord_on(cfg.discord_enabled);
    app.set_autostart(sys::autostart_enabled());
    app.set_check_updates(cfg.check_updates);
    app.set_allow_explicit(cfg.allow_explicit);
    app.set_linux(cfg!(target_os = "linux"));
    app.set_translations(crate::i18n::available());
    app.set_download_dir(music_dir(&cfg).display().to_string().into());
    app.set_download_flac(cfg.download_flac);
    keys::init(&app, &cfg.hotkeys);
    app.set_log_volume(cfg.log_volume);
    app.set_wave_motion(cfg.wave_motion);
    app.set_crossfade(cfg.crossfade_secs > 0);
    app.set_hidden_blocks(landing_ui::hidden_titles(&cfg).into());
    let places = Rc::new(Places::default());
    places.mini.set(cfg.mini_pos.map(|[x, y]| slint::PhysicalPosition::new(x, y)));
    if let Some([w, h]) = cfg.window_size {
        let size = slint::PhysicalSize::new(w, h);
        places.restored.set(Some(size));
        ui.window().set_size(size);
    }
    if let Some([x, y]) = cfg.window_pos {
        ui.window().set_position(slint::PhysicalPosition::new(x, y));
    }
    if cfg.window_maximized {
        ui.window().set_maximized(true);
        app.set_maximized(true);
    }
    let ctx = Rc::new(Ctx {
        ui: ui.as_weak(),
        lite: Cell::new(lite),
        player: player.clone(),
        volume_save_at: Cell::new(None),
        discord,
        images: Images::new(lite),
        list_window: Cell::default(),
        grid_window: Cell::default(),
        st: RefCell::new(State {
            entity_likes: HashMap::new(),
            saved_entities: Default::default(),
            cfg,
            dark,
            uid: String::new(),
            started: false,
            wheel: Vec::new(),
            wheel_seeds: None,
            tune_seeds: Vec::new(),
            liked_albums: Default::default(),
            liked_playlists: Default::default(),
            grid_key: None,
            landing_key: None,
            grid_titles: HashMap::new(),
            grid: Vec::new(),
            grid_tab: 0,
            net: None,
            pins: Vec::new(),
            pins_checked: None,
            playlists: Vec::new(),
            albums: Vec::new(),
            coll_shown: None,
            pl_tab: 0,
            album_tab: 0,
            list_key: None,
            list_shown: None,
            view: Vec::new(),
            search_index: Vec::new(),
            history: Vec::new(),
            forward: Vec::new(),
            track_id: None,
            toast_timer: None,
            last_audio_output_error: None,
            cards: HashMap::new(),
            search_shown: None,
            results_key: None,
            results_shown: None,
            results: Vec::new(),
            search_timer: None,
            chart: Vec::new(),
            artist_key: None,
            artist_shown: None,
            artist_tracks: Vec::new(),
            liked_artists: Default::default(),
            saved_artist_marks: Default::default(),
            pending_artist_marks: Default::default(),
            disliked_artists: None,
            menu_track: None,
            rename_key: None,
            menu_playlists: Vec::new(),
            lyrics_for: None,
            lyrics_track: None,
            lyrics: Vec::new(),
            lyrics_static: false,
            lyric_target: -1,
            lyrics_wheel_timer: None,
            lyrics_mouse_timer: None,
            queue_ids: Vec::new(),
            blob_src: None,
            blob_requested: false,
            vibe_key: None,
            trailer: None,
            wave_tune: Vec::new(),
            download: None,
        }),
    });
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    if ctx.st.borrow().cfg.token.is_none() {
        ctx.login_start();
    } else {
        ctx.refresh_token_if_needed();
    }

    let p = player.clone();
    app.on_play_pause(move || {
        let started = CTX.with(|c| c.borrow().as_ref().map(|c| c.st.borrow().started)).unwrap_or(true);
        if started {
            p.send(Cmd::PlayPause);
        } else {
            with_ctx(|c| c.start());
        }
    });
    let p = player.clone();
    app.on_next(move || p.send(Cmd::Next));
    let p = player.clone();
    app.on_prev(move || p.send(Cmd::Prev));
    app.on_seek(|seconds| with_ctx(|c| {
        if let Some(position) = c.player.seek(seconds) {
            c.ui().global::<App>().set_position(position);
        }
    }));
    let p = player.clone();
    app.on_track_liked(move |id, _revision| p.is_liked(id.as_str()));
    let p = player.clone();
    app.on_like(move || {
        if let Some(t) = p.state().track {
            with_ctx(|c| c.like(t));
        }
    });
    app.on_dislike(|| with_ctx(|c| c.dislike()));
    let p = player.clone();
    app.on_cycle_repeat(move || {
        let next = p.state().next_repeat();
        p.send(Cmd::Repeat(next));
    });
    let p = player.clone();
    app.on_toggle_shuffle(move || p.send(Cmd::Shuffle(!p.state().shuffle)));
    let p = player.clone();
    app.on_set_volume(move |v| {
        p.send(Cmd::Volume(v));
        with_ctx(|c| {
            c.st.borrow_mut().cfg.volume = v;
            c.volume_save_at.set(Some(Instant::now() + Duration::from_millis(300)));
        });
    });
    app.on_format_time(|s| mmss(s).into());

    app.on_navigate(|page| with_ctx(|c| c.navigate(page, None, true)));
    app.on_open_pin(|i| with_ctx(|c| c.open_pin(i as usize)));
    app.on_open_pin_menu(|i, x, y| with_ctx(|c| {
        if let Some(pin) = c.st.borrow().pins.get(i as usize) {
            let item = c.menu_entry(&format!("unpin:{}", pin.target), "Открепить", c.ui().global::<Icons>().get_m_unpin(), false);
            c.show_menu(vec![item], x, y, true);
        }
    }));
    app.on_open_likes(|| with_ctx(|c| c.navigate(Page::List, Some("likes".into()), true)));
    app.on_open_playlist(|i| {
        with_ctx(|c| {
            let p = c.st.borrow().playlists.get(i as usize).cloned();
            if let Some(p) = p {
                c.navigate(Page::List, Some(format!("playlist:{}:{}", p.uid, p.kind)), true);
            }
        })
    });
    app.on_open_album(|i| {
        with_ctx(|c| {
            let a = c.st.borrow().albums.get(i as usize).cloned();
            if let Some(a) = a {
                c.navigate(Page::List, Some(format!("album:{}", a.id)), true);
            }
        })
    });
    app.on_open_top_artist(|i| {
        with_ctx(|c| {
            let Some(data) = c.collection() else { return };
            let Data::Collection(col) = &*data else { return };
            let Some(a) = col.top_artists.get(i as usize) else { return };
            c.navigate(Page::Artist, Some(format!("artist:{}", a.artist.id)), true);
        })
    });
    app.on_select_playlist_tab(|i| {
        with_ctx(|c| {
            c.st.borrow_mut().pl_tab = i.max(0) as usize;
            c.show_playlist_tab();
        })
    });
    app.on_select_album_tab(|i| {
        with_ctx(|c| {
            c.st.borrow_mut().album_tab = i.max(0) as usize;
            c.show_album_tab();
        })
    });
    app.on_create_playlist(|| {
        with_ctx(|c| {
            let Some(net) = c.st.borrow().net.clone() else { return };
            let title = crate::i18n::tr("Новый плейлист");
            std::thread::spawn(move || match net.api.create_playlist(&net.uid, &title) {
                Ok(pl) => {
                    net.refresh("playlists");
                    net.refresh("collection");
                    let key = format!("playlist:{}:{}", pl.uid, pl.kind);
                    let _ = slint::invoke_from_event_loop(move || {
                        with_ctx(|c| {
                            if !c.same_session(&net) { return; }
                            c.st.borrow_mut().rename_key = Some(key.clone());
                            c.navigate(Page::List, Some(key), true);
                        })
                    });
                }
                Err(e) => net.say(format!("{}: {e}", crate::i18n::tr("Не удалось создать плейлист"))),
            });
        })
    });
    app.on_play_likes_preview(|i| {
        with_ctx(|c| {
            let Some(data) = c.collection() else { return };
            let Data::Collection(col) = &*data else { return };
            let Some(t) = col.likes_preview.get(i as usize).cloned() else { return };
            let full = c.st.borrow().net.as_ref().and_then(|n| n.get("likes")).and_then(|r| r.ok());
            let tracks = match full.as_deref() {
                Some(Data::List(l)) => l.tracks.clone(),
                _ => col.likes_preview.clone(),
            };
            let start = tracks.iter().position(|x| x.id == t.id).unwrap_or(0);
            c.player.send(Cmd::PlayList {
                source: Source::List { kind: ListKind::Likes, id: "likes".into(), title: "Мне нравится".into() },
                tracks,
                start,
                after: After::Wave(Source::Wave),
            });
            c.start();
        })
    });
    app.on_like_preview(|i| {
        with_ctx(|c| {
            let Some(data) = c.collection() else { return };
            let Data::Collection(col) = &*data else { return };
            if let Some(t) = col.likes_preview.get(i as usize).cloned() {
                c.like(t);
            }
        })
    });
    app.on_play_wheel(|i| with_ctx(|c| c.play_wheel(i as usize)));
    app.on_go_back(|| with_ctx(|c| c.go_back()));
    app.on_go_forward(|| with_ctx(|c| c.go_forward()));
    app.on_play_list(|i| with_ctx(|c| c.play_list(i as usize)));
    app.on_like_track(|i| {
        with_ctx(|c| {
            let Some(t) = c.view_track(i as usize) else { return };
            c.like(t);
        })
    });
    app.on_list_viewport(|first, count| with_ctx(|c| {
        let range = (first.max(0) as usize, count.max(0) as usize);
        if c.list_window.replace(range) != range && c.ui().global::<App>().get_page() == Page::List {
            c.show_list_window(true);
        }
    }));
    app.on_grid_viewport(|first, count| with_ctx(|c| {
        let range = (first.max(0) as usize, count.max(0) as usize);
        if c.grid_window.replace(range) != range && c.ui().global::<App>().get_page() == Page::Grid {
            c.show_grid_window(true);
        }
    }));
    app.on_filter_changed(|_| with_ctx(|c| c.apply_filter()));

    register_extra(&app);
    landing_ui::register(&app);
    history_ui::register(&app);
    track_about::register(&app);
    landing_ui::load_concert_assets(&app);

    let weak = ui.as_weak();
    app.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            #[cfg(windows)]
            sys::start_move(ui.window());
            #[cfg(not(windows))]
            {
                ui.window().with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
            release_after_system_drag(&ui);
        }
    });
    let weak = ui.as_weak();
    let pl = places.clone();
    app.on_toggle_mini(move || {
        use winit::dpi::LogicalSize;
        let Some(ui) = weak.upgrade() else { return };
        let app = ui.global::<App>();
        let w = ui.window();
        if !app.get_mini() {
            let was_max = window_maximized(w);
            pl.normal.set(Some((window_pos(w), w.size(), was_max)));
            app.set_mini(true);
            app.set_maximized(false);
            // KWin в ответ на set_maximized(false) присылает текущий размер и перебивает размер плашки.
            w.set_maximized(false);
            #[cfg(windows)]
            sys::set_mini_mode(w, true);
            // Slint обновит ограничения размера только на следующем кадре, а Windows без этого не даст ужать окно.
            w.with_winit_window(|ww| {
                if was_max {
                    ww.set_maximized(false);
                }
                ww.set_min_inner_size(Some(LogicalSize::new(320.0, 88.0)));
            });
            w.set_size(slint::LogicalSize::new(400.0, 88.0));
            // Композитор на Linux возвращает размер развёрнутого окна уже после нашего запроса.
            #[cfg(target_os = "linux")]
            {
                let weak = weak.clone();
                Timer::single_shot(Duration::from_millis(150), move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let w = ui.window();
                    if ui.global::<App>().get_mini() && w.size().height as f32 > 120.0 * w.scale_factor() {
                        w.set_size(slint::LogicalSize::new(400.0, 88.0));
                    }
                });
            }
            let k = w.scale_factor();
            let (mw, mh, gap) = ((400.0 * k) as i32, (88.0 * k) as i32, (16.0 * k) as i32);
            let (l, t, r, b) = sys::work_area(w);
            let p = pl.mini.get().unwrap_or(slint::PhysicalPosition::new(r - mw - gap, b - mh - gap));
            w.set_position(slint::PhysicalPosition::new(p.x.min(r - mw).max(l), p.y.min(b - mh).max(t)));
        } else {
            pl.mini.set(window_pos(w).or(pl.mini.get()));
            app.set_mini(false);
            w.set_maximized(false);
            #[cfg(windows)]
            sys::set_mini_mode(w, false);
            w.with_winit_window(|ww| ww.set_min_inner_size(Some(LogicalSize::new(960.0, 640.0))));
            match pl.normal.take() {
                Some((pos, size, max)) => {
                    w.set_size(if max { pl.restored.get().unwrap_or(size) } else { size });
                    if let Some(pos) = pos {
                        w.set_position(pos);
                        // Оконный менеджер X11 может сдвинуть окно, пока применяет новый размер.
                        #[cfg(target_os = "linux")]
                        {
                            let weak = weak.clone();
                            Timer::single_shot(Duration::from_millis(150), move || {
                                let Some(ui) = weak.upgrade() else { return };
                                if !ui.global::<App>().get_mini() && window_pos(ui.window()) != Some(pos) {
                                    ui.window().set_position(pos);
                                }
                            });
                        }
                    }
                    if max {
                        w.set_maximized(true);
                        app.set_maximized(true);
                    }
                }
                None => w.set_size(slint::LogicalSize::new(1280.0, 800.0)),
            }
        }
        after_window_mode_change(&ui);
    });
    let weak = ui.as_weak();
    app.on_resize_window(move |dir| {
        if let Some(ui) = weak.upgrade() {
            #[cfg(windows)]
            sys::start_resize(ui.window(), dir);
            #[cfg(not(windows))]
            {
                use winit::window::ResizeDirection as D;
                let d = [D::North, D::NorthEast, D::East, D::SouthEast, D::South, D::SouthWest, D::West, D::NorthWest][dir.clamp(0, 7) as usize];
                ui.window().with_winit_window(|w| {
                    let _ = w.drag_resize_window(d);
                });
            }
            release_after_system_drag(&ui);
        }
    });
    let weak = ui.as_weak();
    app.on_toggle_maximize(move || {
        if let Some(ui) = weak.upgrade() {
            #[cfg(windows)]
            let m = !sys::is_maximized(ui.window());
            #[cfg(not(windows))]
            let m = !ui.window().is_maximized();
            ui.window().set_maximized(m);
            ui.global::<App>().set_maximized(m);
        }
    });
    let weak = ui.as_weak();
    app.on_minimize(move || {
        if let Some(ui) = weak.upgrade() {
            sys::minimize(ui.window());
        }
    });
    // Как у оригинала на Windows: пока играет трек, окно прячется в трей, иначе приложение закрывается.
    // На Linux трея пока нет, там закрытие всегда выход.
    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        let p = player.clone();
        app.on_close(move || {
            if p.state().playing {
                if let Some(ui) = weak.upgrade() {
                    if sys::created(ui.window()) {
                        sys::hide(ui.window());
                        sys::trim_memory();
                        return;
                    }
                }
            }
            quit();
        });
        install_tray(&ui, &player);
    }
    #[cfg(not(windows))]
    {
        app.on_close(quit);
        let weak = ui.as_weak();
        sys::on_wake(move || {
            let _ = weak.upgrade_in_event_loop(|ui| reveal(&ui));
        });
        install_mpris(&ui, &player);
    }
    // Alt+F4 и закрытие из панели задач: Slint сам только прячет окно, а процесс остаётся жить.
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<App>().invoke_close();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });

    ui.window().on_winit_window_event(|_w, ev| {
        use slint::winit_030::EventResult;
        use winit::event::{ElementState, MouseButton, WindowEvent};
        match ev {
            WindowEvent::CursorMoved { position, .. } => {
                let p = position.to_logical::<f32>(_w.scale_factor() as f64);
                let p = slint::LogicalPosition::new(p.x, p.y);
                keys::set_cursor(p);
                #[cfg(target_os = "linux")]
                LAST_CURSOR.set(p);
                EventResult::Propagate
            }
            WindowEvent::ModifiersChanged(m) => {
                keys::set_modifiers(m.state());
                EventResult::Propagate
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key {
                    if keys::on_key(code, event.state == ElementState::Pressed, event.repeat) {
                        return EventResult::PreventDefault;
                    }
                }
                EventResult::Propagate
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                keys::on_mouse_down();
                EventResult::Propagate
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Back, .. } => {
                with_ctx(|c| c.go_back());
                EventResult::PreventDefault
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Forward, .. } => {
                with_ctx(|c| c.go_forward());
                EventResult::PreventDefault
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                after_dpi_change();
                EventResult::Propagate
            }
            WindowEvent::Moved(_) => {
                after_move();
                EventResult::Propagate
            }
            // В i3 кадры, нарисованные до того, как окно показано и растянуто, теряются, а Slint
            // дорисовывает только изменившееся: окно оставалось пустым кусками (#87).
            // Ждём, пока окно сменит размер или станет видно.
            #[cfg(target_os = "linux")]
            WindowEvent::Occluded(false) | WindowEvent::Focused(true) | WindowEvent::Resized(_) => {
                Timer::single_shot(Duration::from_millis(150), || with_ctx(|c| full_repaint(&c.ui())));
                EventResult::Propagate
            }
            _ => EventResult::Propagate,
        }
    });

    let tick = Timer::default();
    tick.start(TimerMode::Repeated, Duration::from_millis(250), || with_ctx(|c| c.sync_player()));
    // Строки текста переключаются точнее, чем раз в 250 мс.
    let lyric_tick = Timer::default();
    lyric_tick.start(TimerMode::Repeated, Duration::from_millis(50), || {
        with_ctx(|c| {
            let ui = c.ui();
            let app = ui.global::<App>();
            if app.get_fullscreen() && app.get_fs_mode() == 2 && app.get_playing() {
                c.update_lyric_current(c.player.state().position);
            }
        })
    });

    // «Как в системе» следит за темой Windows и на ходу. На Linux её отдаёт портал через
    // gdbus, запускать его каждые пару секунд не стоит.
    #[cfg(windows)]
    let theme_tick = Timer::default();
    #[cfg(windows)]
    theme_tick.start(TimerMode::Repeated, Duration::from_secs(2), || {
        with_ctx(|c| {
            if c.st.borrow().cfg.theme != "system" {
                return;
            }
            let dark = !sys::system_light_theme();
            if c.st.borrow().dark == dark {
                return;
            }
            c.st.borrow_mut().dark = dark;
            let ui = c.ui();
            ui.global::<Theme>().set_dark(ui.global::<App>().get_page() == Page::Wave || dark);
        })
    });

    let weak = ui.as_weak();
    let pl = places.clone();
    app.on_window_resized(move || {
        let weak = weak.clone();
        let pl = pl.clone();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                apply_ui_scale(&ui);
                let w = ui.window();
                // Snap и восстановление через Win32 обходят состояние кнопки в Slint.
                #[cfg(windows)]
                ui.global::<App>().set_maximized(sys::is_maximized(w));
                if !window_maximized(w) && !w.is_minimized() && !ui.global::<App>().get_mini() {
                    pl.restored.set(Some(w.size()));
                }
            }
        });
    });

    if !start_hidden {
        reveal(&ui);
    }
    let result = slint::run_event_loop_until_quit();
    if ctx.volume_save_at.take().is_some() {
        ctx.st.borrow().cfg.save();
    }
    player.leave();
    save_places(&ui, &places);
    result?;
    sys::cleanup();
    // Фоновые потоки (аудио, сеть, обложки) не ждём: иначе процесс может остаться
    // без окна и держать мьютекс, и следующий запуск молча закроется.
    std::process::exit(0)
}

/// Медиаклавиши и плеер в панели Linux. Команды выполняются так же, как кнопки в окне.
#[cfg(target_os = "linux")]
fn install_mpris(ui: &AppWindow, player: &Player) {
    let weak = ui.as_weak();
    let p = player.clone();
    crate::mpris::spawn(player.clone(), move |e| {
        let p = p.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            use crate::mpris::Event;
            let app = ui.global::<App>();
            match e {
                Event::PlayPause => app.invoke_play_pause(),
                Event::Play => with_ctx(|c| {
                    if c.st.borrow().started { p.send(Cmd::Resume); } else { c.start(); }
                }),
                Event::Pause => p.send(Cmd::Pause),
                Event::Next => p.send(Cmd::Next),
                Event::Prev => p.send(Cmd::Prev),
                Event::SeekTrack(id, micros) => p.send(Cmd::SeekTrack(id, micros)),
                Event::SeekBy(sec) => p.send(Cmd::SeekBy(sec)),
                Event::Volume(v) => app.invoke_set_volume(v),
                Event::Raise => reveal(&ui),
                Event::Quit => quit(),
            }
        });
    });
}

#[cfg(windows)]
fn install_tray(ui: &AppWindow, player: &Player) {
    let weak = ui.as_weak();
    let p = player.clone();
    crate::tray::install(
        move |e| {
            use crate::tray::Event;
            let Some(ui) = weak.upgrade() else { return };
            match e {
                Event::Toggle => {
                    if !sys::is_shown(ui.window()) {
                        reveal(&ui);
                    } else if sys::is_minimized(ui.window()) {
                        sys::raise(ui.window());
                    } else {
                        ui.window().set_minimized(true);
                    }
                }
                Event::Show => reveal(&ui),
                Event::PlayPause | Event::Play | Event::Pause | Event::Next | Event::Prev if p.state().status != Status::Ready => {}
                Event::PlayPause => ui.global::<App>().invoke_play_pause(),
                Event::Play => with_ctx(|ctx| {
                    if ctx.st.borrow().started {
                        p.send(Cmd::Resume);
                    } else {
                        ctx.start();
                    }
                }),
                Event::Seek(ms) => { p.seek(ms as f32 / 1000.0); }
                Event::Pause => p.send(Cmd::Pause),
                Event::Prev => p.send(Cmd::Prev),
                Event::Next => p.send(Cmd::Next),
                Event::Updates => check_updates_now(&ui),
                Event::Terms => sys::open_url("https://yandex.ru/legal/music_mobile_agreement/"),
                Event::Recommendations => sys::open_url("https://music.yandex.ru/legal/recommendations/ru/#music"),
                Event::Support => sys::open_url(SUPPORT_CHAT),
                Event::Quit => quit(),
            }
        },
        {
            let weak = ui.as_weak();
            let p = player.clone();
            move || {
                let minimized = weak.upgrade().is_none_or(|ui| !sys::is_shown(ui.window()) || sys::is_minimized(ui.window()));
                let s = p.state();
                crate::tray::MenuState {
                    minimized,
                    playing: s.can_pause(),
                    can_prev: s.track.is_some(),
                    can_next: s.track.is_some(),
                }
            }
        },
    );
}

// ---------- цвета волны и плеера ----------

fn hsl(c: [u8; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l * 100.0);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    (h.round(), (s * 100.0).round(), (l * 100.0).round())
}

fn from_hsl(h: f32, s: f32, l: f32) -> Color {
    let (s, l) = (s / 100.0, l / 100.0);
    let k = |n: f32| (n + h / 30.0) % 12.0;
    let a = s * l.min(1.0 - l);
    let f = |n: f32| l - a * (-1.0f32).max((k(n) - 3.0).min(9.0 - k(n)).min(1.0));
    Color::from_rgb_u8((f(0.0) * 255.0).round() as u8, (f(8.0) * 255.0).round() as u8, (f(4.0) * 255.0).round() as u8)
}

/// Фон плеера оригинала: тот же оттенок и насыщенность, светлота 20%.
fn player_tint(c: [u8; 3]) -> Color {
    let (h, s, _) = hsl(c);
    from_hsl(h, s, 20.0)
}

/// Корзина палитры «Моей волны» для цвета трека — как getVibePaletteColors.
fn vibe_bucket(c: Option<[u8; 3]>) -> &'static (&'static str, u16, u16, [u8; 3], [[u8; 3]; 16]) {
    use crate::palette::{BUCKETS, LIGHTNESS};
    let by_name = |n: &str| BUCKETS.iter().find(|b| b.0 == n);
    let fallback = by_name("slate").expect("slate");
    let Some(c) = c else { return fallback };
    let (h, s, l) = hsl(c);
    let found = if s > 0.0 {
        let h = h as u16;
        BUCKETS.iter().find(|b| if b.1 > b.2 { h >= b.1 || h <= b.2 } else { h >= b.1 && h <= b.2 })
    } else {
        let l = l as u8;
        let last = LIGHTNESS.len() - 1;
        LIGHTNESS
            .iter()
            .enumerate()
            .find(|(i, b)| if *i == last { l >= b.1 && l <= b.2 } else { l >= b.1 && l < b.2 })
            .and_then(|(_, b)| by_name(b.0))
    };
    found.unwrap_or(fallback)
}

/// Режим наложения luminosity: оттенок фона, яркость картинки.
fn recolor(src: &SharedPixelBuffer<Rgba8Pixel>, c: [u8; 3]) -> Image {
    let lum = |r: f32, g: f32, b: f32| 0.3 * r + 0.59 * g + 0.11 * b;
    let (cr, cg, cb) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let cl = lum(cr, cg, cb);
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(src.width(), src.height());
    for (o, p) in out.make_mut_slice().iter_mut().zip(src.as_slice()) {
        let l = lum(p.r as f32 / 255.0, p.g as f32 / 255.0, p.b as f32 / 255.0);
        let d = l - cl;
        let (mut r, mut g, mut b) = (cr + d, cg + d, cb + d);
        let ll = lum(r, g, b);
        let n = r.min(g).min(b);
        let x = r.max(g).max(b);
        if n < 0.0 {
            r = ll + (r - ll) * ll / (ll - n);
            g = ll + (g - ll) * ll / (ll - n);
            b = ll + (b - ll) * ll / (ll - n);
        }
        if x > 1.0 {
            r = ll + (r - ll) * (1.0 - ll) / (x - ll);
            g = ll + (g - ll) * (1.0 - ll) / (x - ll);
            b = ll + (b - ll) * (1.0 - ll) / (x - ll);
        }
        *o = Rgba8Pixel { r: (r * 255.0) as u8, g: (g * 255.0) as u8, b: (b * 255.0) as u8, a: p.a };
    }
    Image::from_rgba8(out)
}

const BLOB_URL: &str = "https://avatars.mds.yandex.net/get-music-misc/2419084/img.69c4e2a11982013a65e2121b/400x400";

fn card_from_artist(a: &crate::api::Artist) -> Card {
    Card {
        title: a.name.clone(),
        subtitle: String::new(),
        year: String::new(),
        likes: None,
        covers: a.cover.iter().cloned().collect(),
        round: true,
        explicit: false,
        target: format!("artist:{}", a.id),
    }
}

fn card_from_album(a: &Album) -> Card {
    Card {
        title: a.title.clone(),
        subtitle: a.artists.clone(),
        year: a.year.map(|y| y.to_string()).unwrap_or_default(),
        likes: None,
        covers: a.cover.iter().cloned().collect(),
        round: false,
        explicit: false,
        target: format!("album:{}", a.id),
    }
}

fn card_from_playlist(p: &Playlist) -> Card {
    Card {
        title: p.title.clone(),
        subtitle: p.owner.clone(),
        year: String::new(),
        likes: None,
        covers: p.covers.clone(),
        round: false,
        explicit: false,
        target: format!("playlist:{}:{}", p.uid, p.kind),
    }
}

fn track_link(t: &Track) -> String {
    match &t.album_id {
        Some(a) => format!("https://music.yandex.ru/album/{a}/track/{}", t.id),
        None => format!("https://music.yandex.ru/track/{}", t.id),
    }
}

/// Код для вставки плеера на сайт, как в окне «HTML-код» оригинала.
fn iframe_code(path: &str, w: u32, h: u32, link: &str) -> String {
    format!(
        "<iframe frameborder=\"0\" allow=\"clipboard-write\" style=\"border:none;width:{w}px;height:{h}px;\" width=\"{w}\" height=\"{h}\" src=\"https://music.yandex.ru/iframe/{path}\">Слушайте {link} на Яндекс Музыке</iframe>"
    )
}


fn file_name(s: &str) -> String {
    file_name_max(s, 100)
}

impl Ctx {
    // ---------- вход ----------

    /// Показать экран входа и запросить код устройства; дальше поток ждёт подтверждения.
    fn login_start(self: &Rc<Self>) {
        static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_logged_out(true);
        app.set_login_code(SharedString::new());
        app.set_login_error(SharedString::new());
        let weak = self.ui.clone();
        std::thread::spawn(move || {
            let alive = || GEN.load(Ordering::SeqCst) == gen;
            let set = move |code: String, err: String| {
                let w = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        let app = ui.global::<App>();
                        let chars: Vec<SharedString> = code.chars().map(|c| SharedString::from(c.to_string())).collect();
                        app.set_login_code_chars(ModelRc::from(Rc::new(VecModel::from(chars))));
                        app.set_login_code(code.into());
                        app.set_login_error(err.into());
                    }
                });
            };
            let dc = match crate::auth::device_code() {
                Ok(dc) => dc,
                Err(e) => return set(String::new(), format!("Не удалось получить код: {e}")),
            };
            set(dc.user_code.clone(), String::new());
            let deadline = Instant::now() + Duration::from_secs(dc.expires_in);
            while alive() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(dc.interval));
                if !alive() {
                    return;
                }
                match crate::auth::poll(&dc.device_code) {
                    crate::auth::Poll::Pending => continue,
                    crate::auth::Poll::Failed(e) => return set(String::new(), format!("Вход не удался: {e}")),
                    crate::auth::Poll::Done(t) => {
                        let _ = slint::invoke_from_event_loop(move || with_ctx(|c| c.login_done(t)));
                        return;
                    }
                }
            }
            if alive() {
                set(String::new(), "Код устарел".into());
            }
        });
    }

    fn login_done(self: &Rc<Self>, t: crate::auth::Token) {
        {
            let mut st = self.st.borrow_mut();
            st.cfg.token = Some(t.access.clone());
            if t.refresh.is_some() {
                st.cfg.refresh_token = t.refresh;
            }
            st.cfg.token_expires = t.expires_in.map(|s| crate::config::now_secs() + s);
            st.cfg.save();
        }
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_logged_out(false);
        app.set_login_code(SharedString::new());
        self.player.send(Cmd::Login(t.access));
    }

    /// Токен живёт около года: за месяц до конца продлеваем его в фоне.
    fn refresh_token_if_needed(self: &Rc<Self>) {
        let (refresh, expires) = {
            let st = self.st.borrow();
            (st.cfg.refresh_token.clone(), st.cfg.token_expires)
        };
        let (Some(refresh), Some(expires)) = (refresh, expires) else { return };
        if expires > crate::config::now_secs() + 30 * 86400 {
            return;
        }
        std::thread::spawn(move || match crate::auth::refresh(&refresh) {
            Ok(t) => {
                let _ = slint::invoke_from_event_loop(move || {
                    with_ctx(|c| {
                        let mut st = c.st.borrow_mut();
                        st.cfg.token = Some(t.access);
                        if t.refresh.is_some() {
                            st.cfg.refresh_token = t.refresh;
                        }
                        st.cfg.token_expires = t.expires_in.map(|s| crate::config::now_secs() + s);
                        st.cfg.save();
                    })
                });
            }
            Err(e) => log(format!("продление токена: {e}")),
        });
    }

    fn logout(self: &Rc<Self>) {
        self.player.send(Cmd::Logout);
        {
            let mut st = self.st.borrow_mut();
            if let Some(token) = st.cfg.token.clone() {
                std::thread::spawn(move || match crate::auth::revoke(&token) {
                    Ok(()) => log("выход: токен отозван"),
                    Err(e) => log(format!("выход: токен не отозван: {e}")),
                });
            }
            st.cfg.forget_token();
            st.cfg.forget_refresh();
            st.cfg.token_expires = None;
            st.cfg.save();
            st.net = None;
            st.entity_likes.clear();
            st.saved_entities.clear();
            st.started = false;
            st.uid.clear();
            st.pins.clear();
            st.wheel.clear();
            st.wheel_seeds = None;
            st.tune_seeds.clear();
            st.wave_tune.clear();
            st.pending_artist_marks.clear();
            st.saved_artist_marks.clear();
            st.liked_artists.clear();
            st.disliked_artists = None;
            st.pins_checked = None;
            st.liked_albums.clear();
            st.liked_playlists.clear();
            st.cards.clear();
            st.menu_playlists.clear();
            st.playlists.clear();
            st.albums.clear();
            st.coll_shown = None;
            st.track_id = None;
            st.history.clear();
            st.forward.clear();
        }
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_login(SharedString::new());
        app.set_avatar(Image::default());
        app.set_pins(ModelRc::default());
        app.set_wheel(ModelRc::default());
        app.set_wave_settings(ModelRc::default());
        app.set_wave_settings_open(false);
        app.set_my_artists(ModelRc::default());
        app.set_fullscreen(false);
        self.navigate(Page::Wave, None, false);
        self.login_start();
    }

    fn menu_entry(&self, id: &str, text: &str, icon: Image, submenu: bool) -> MenuEntry {
        MenuEntry { id: id.into(), text: crate::i18n::tr(text).into(), icon, submenu, checked: false }
    }

    // ---------- карточки ----------

    fn card_model(self: &Rc<Self>, block: &str, cards: &[Card], css: f32) -> ModelRc<CardItem> {
        let items: Vec<_> = cards.iter().map(card_item).collect();
        let model = Rc::new(VecModel::from(items));
        for (i, c) in cards.iter().enumerate() {
            self.load_cover(&model, i, c.covers.first().map(|s| s.as_str()), css, |r, img| r.cover = img);
            self.load_cover(&model, i, c.covers.get(1).map(|s| s.as_str()), css * 0.6, |r, img| r.cover2 = img);
            self.load_cover(&model, i, c.covers.get(2).map(|s| s.as_str()), css * 0.6, |r, img| r.cover3 = img);
        }
        self.st.borrow_mut().cards.insert(block.to_string(), cards.to_vec());
        ModelRc::from(model)
    }

    fn track_model(self: &Rc<Self>, tracks: &[Track]) -> ModelRc<TrackItem> {
        let model = Rc::new(VecModel::from(tracks.iter().map(|t| self.track_item(t)).collect::<Vec<_>>()));
        for (i, t) in tracks.iter().enumerate() {
            self.load_cover(&model, i, t.cover.as_deref(), 40.0, |r, img| r.cover = img);
        }
        ModelRc::from(model)
    }

    fn open_target(self: &Rc<Self>, target: &str, title: &str) {
        let (kind, arg) = target.split_once(':').unwrap_or(("", ""));
        match kind {
            "album" | "playlist" => self.navigate(Page::List, Some(target.to_string()), true),
            "artist" => self.navigate(Page::Artist, Some(target.to_string()), true),
            // Подборка — сетка её плейлистов; заголовок, пока грузится, — с карточки.
            "tag" => {
                self.st.borrow_mut().grid_titles.insert(target.to_string(), title.to_string());
                self.navigate(Page::Grid, Some(target.to_string()), true);
            }
            "landing" => self.navigate(Page::Landing, Some(arg.to_string()), true),
            _ => {}
        }
    }

    fn open_card(self: &Rc<Self>, block: &str, i: usize) {
        match block {
            "chart" => return self.navigate(Page::Chart, None, true),
            "dislikes" => return self.navigate(Page::List, Some("dislikes".into()), true),
            "artist-tracks" => {
                if let Some((id, _)) = self.current_artist() {
                    self.navigate(Page::List, Some(format!("artist-tracks:{id}")), true);
                }
                return;
            }
            "artist-more" => {
                let (x, y) = (self.ui().window().size().width as f32 / self.ui().window().scale_factor() / 2.0, 300.0);
                return self.open_artist_menu(x, y);
            }
            _ => {}
        }
        let card = {
            let st = self.st.borrow();
            let cards = if block == "grid" { st.grid.get(st.grid_tab).map(|t| &t.1) } else { st.cards.get(block) };
            cards.and_then(|items| items.get(i).cloned())
        };
        if let Some(c) = card {
            self.open_target(&c.target, &c.title);
        }
    }

    // ---------- поиск ----------

    fn refresh_search(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let Some(net) = self.st.borrow().net.clone() else { return };
        match net.get("search-landing") {
            None => app.set_search_loading(true),
            Some(Err(e)) => {
                app.set_search_loading(false);
                self.toast(format!("Не удалось загрузить поиск: {e}"));
            }
            Some(Ok(data)) => {
                app.set_search_loading(false);
                if self.st.borrow().search_shown.as_ref().is_some_and(|d| Arc::ptr_eq(d, &data)) {
                    return;
                }
                self.st.borrow_mut().search_shown = Some(data.clone());
                let Data::SearchLanding(l) = &*data else { return };
                app.set_search_suggest(ModelRc::from(Rc::new(VecModel::from(l.suggestions.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>()))));
                app.set_search_mixes(self.card_model("search-mixes", &l.mixes, 112.0));
                app.set_search_missed(self.card_model("search-missed", &l.missed, 186.0));
                app.set_search_newpl(self.card_model("search-newpl", &l.new_playlists, 186.0));
                app.set_search_history(self.card_model("search-history", &l.history, 186.0));
                let tracks: Vec<Track> = l.chart.iter().map(|c| c.track.clone()).collect();
                let rows = Rc::new(VecModel::from(
                    l.chart
                        .iter()
                        .map(|c| ChartRow {
                            item: self.track_item(&c.track),
                            pos: c.position.to_string().into(),
                            trend: match c.progress.as_str() {
                                "up" => 2,
                                "down" => 3,
                                "new" => 4,
                                _ => 1,
                            },
                        })
                        .collect::<Vec<_>>(),
                ));
                for (i, t) in tracks.iter().enumerate() {
                    let Some(uri) = t.cover.as_deref() else { continue };
                    let size = self.size_for(40.0);
                    let m = rows.clone();
                    self.images.get(&crate::api::pic(uri, size), size, move |img| {
                        if let Some(mut r) = m.row_data(i) {
                            r.item.cover = img.clone();
                            m.set_row_data(i, r);
                        }
                    });
                }
                app.set_search_chart(ModelRc::from(rows));
                self.st.borrow_mut().chart = tracks;
            }
        }
    }

    fn search_now(self: &Rc<Self>, text: String) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let text = text.trim().to_string();
        app.set_searching(!text.is_empty());
        if text.is_empty() {
            app.set_search_tracks_only(false);
        }
        let kind = if app.get_search_tracks_only() { SEARCH_TRACK_KIND } else { "all" };
        let key = (!text.is_empty()).then(|| format!("search:{kind}:{text}"));
        {
            let mut st = self.st.borrow_mut();
            if st.results_key == key {
                return;
            }
            st.results_key = key;
            st.results_shown = None;
            st.results.clear();
            for block in ["sr-artists", "sr-albums", "sr-playlists"] {
                st.cards.remove(block);
            }
        }
        app.set_sr_tracks(ModelRc::default());
        for set_cards in [App::set_sr_artists, App::set_sr_albums, App::set_sr_playlists] {
            set_cards(&app, ModelRc::default());
        }
        app.set_results_loading(false);
        if !text.is_empty() {
            self.refresh_results();
        }
    }

    fn refresh_results(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let (net, key) = {
            let st = self.st.borrow();
            (st.net.clone(), st.results_key.clone())
        };
        let (Some(net), Some(key)) = (net, key) else { return };
        match net.get(&key) {
            None => app.set_results_loading(true),
            Some(Err(e)) => {
                app.set_results_loading(false);
                self.toast(format!("Поиск не удался: {e}"));
            }
            Some(Ok(data)) => {
                app.set_results_loading(false);
                if self.st.borrow().results_shown.as_ref().is_some_and(|d| Arc::ptr_eq(d, &data)) {
                    return;
                }
                self.st.borrow_mut().results_shown = Some(data.clone());
                let Data::Search(s) = &*data else { return };
                app.set_sr_tracks(self.track_model(&s.tracks));
                let artists: Vec<Card> = s.artists.iter().map(card_from_artist).collect();
                let albums: Vec<Card> = s.albums.iter().map(card_from_album).collect();
                let pls: Vec<Card> = s.playlists.iter().map(card_from_playlist).collect();
                app.set_sr_artists(self.card_model("sr-artists", &artists, 186.0));
                app.set_sr_albums(self.card_model("sr-albums", &albums, 186.0));
                app.set_sr_playlists(self.card_model("sr-playlists", &pls, 186.0));
                self.st.borrow_mut().results = s.tracks.clone();
            }
        }
    }

    // ---------- исполнитель ----------

    fn refresh_artist(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let (net, key) = {
            let st = self.st.borrow();
            (st.net.clone(), st.artist_key.clone())
        };
        let (Some(net), Some(key)) = (net, key) else { return };
        match net.get(&key) {
            None => app.set_ar_loading(true),
            Some(Err(e)) => {
                app.set_ar_loading(false);
                self.toast(format!("Не удалось загрузить исполнителя: {e}"));
            }
            Some(Ok(data)) => {
                app.set_ar_loading(false);
                if self.st.borrow().artist_shown.as_ref().is_some_and(|d| Arc::ptr_eq(d, &data)) {
                    return;
                }
                self.st.borrow_mut().artist_shown = Some(data.clone());
                let Data::Artist(a) = &*data else { return };
                app.set_ar_name(a.name.clone().into());
                app.set_ar_listeners(a.listeners.map(crate::net::listeners_word).unwrap_or_default().into());
                app.set_ar_tint(a.color.map(|c| Color::from_argb_u8(178, c[0], c[1], c[2])).unwrap_or(Color::from_argb_u8(0, 0, 0, 0)));
                app.set_ar_liked(self.st.borrow().artist_mark(&a.id) == ArtistMark::Like);
                self.load_disliked_artists();
                app.set_ar_pinned(self.st.borrow().pins.iter().any(|p| p.target == key));
                app.set_ar_popular(self.track_model(&a.popular));
                let fam = if a.familiar_tracks > 0 {
                    let mut s = tracks_word(a.familiar_tracks);
                    if a.familiar_albums > 0 {
                        s.push_str(&format!(" • {} {}", a.familiar_albums, plural(a.familiar_albums, "альбом", "альбома", "альбомов")));
                    }
                    s
                } else {
                    String::new()
                };
                app.set_ar_familiar(fam.into());
                app.set_ar_release(self.card_model("ar-release", a.release.as_slice(), 204.0));
                app.set_ar_studio(self.card_model("ar-studio", &a.studio, 191.0));
                app.set_ar_albums(self.card_model("ar-albums", &a.albums, 191.0));
                app.set_ar_playlists(self.card_model("ar-playlists", &a.playlists, 191.0));
                app.set_ar_compilations(self.card_model("ar-compilations", &a.compilations, 191.0));
                app.set_ar_similar(self.card_model("ar-similar", &a.similar, 191.0));
                if let Some(uri) = &a.cover {
                    let size = self.size_for(204.0);
                    let w = self.ui.clone();
                    let k = key.clone();
                    self.images.get(&crate::api::pic(uri, size), size, move |img| {
                        with_ctx(|c| {
                            if c.st.borrow().artist_key.as_deref() == Some(k.as_str()) {
                                if let Some(ui) = w.upgrade() {
                                    ui.global::<App>().set_ar_cover(img.clone());
                                }
                            }
                        })
                    });
                }
                self.st.borrow_mut().artist_tracks = a.popular.clone();
                self.update_artist_current();
            }
        }
    }

    fn current_artist(&self) -> Option<(String, String)> {
        let data = self.st.borrow().artist_shown.clone()?;
        let Data::Artist(a) = &*data else { return None };
        Some((a.id.clone(), a.name.clone()))
    }

    fn update_artist_current(&self) {
        let s = self.player.state();
        let cur = s.track.as_ref().and_then(|t| self.st.borrow().artist_tracks.iter().position(|x| x.id == t.id));
        self.ui().global::<App>().set_ar_current(cur.map(|i| i as i32).unwrap_or(-1));
    }

    fn play_tracks(self: &Rc<Self>, tracks: Vec<Track>, start: usize, kind: ListKind, id: &str, title: &str) {
        if tracks.is_empty() {
            return;
        }
        if self.player.state().track.as_ref().map(|t| t.id.as_str()) == tracks.get(start).map(|t| t.id.as_str()) {
            self.player.send(Cmd::PlayPause);
            return;
        }
        self.player.send(Cmd::PlayList {
            source: Source::List { kind, id: id.to_string(), title: title.to_string() },
            tracks,
            start,
            after: After::Wave(Source::Wave),
        });
        self.start();
    }

    // ---------- меню трека ----------

    fn open_track_menu(self: &Rc<Self>, x: f32, y: f32) {
        let Some(t) = self.player.state().track else { return };
        self.open_menu_for(t, x, y, false);
    }

    fn open_menu_for(self: &Rc<Self>, t: Track, x: f32, y: f32, below: bool) {
        let ui = self.ui();
        let icons = ui.global::<Icons>();
        let liked = self.player.is_liked(&t.id);
        let current = self.player.state().track.is_some_and(|c| c.id == t.id);
        let mut items = vec![
            self.menu_entry("like", if liked { "Удалить из «Мне нравится»" } else { "Нравится" }, if liked { icons.get_m_liked() } else { icons.get_m_like() }, false),
            self.menu_entry("trailer", "Трейлер", icons.get_m_trailer(), false),
            self.menu_entry("vibe", "Моя волна по треку", icons.get_m_vibe(), false),
            self.menu_entry("download", "Скачать", icons.get_m_download(), false),
            self.menu_entry("next", "Играть следующим", icons.get_m_play_next(), false),
            self.menu_entry("last", "Добавить в конец очереди", icons.get_m_play_last(), false),
        ];
        if current {
            let entry = if self.player.state().disliked {
                self.menu_entry("dislike", "Убрать из «Не нравится»", icons.get_m_disliked(), false)
            } else {
                self.menu_entry("dislike", "Не нравится", icons.get_m_dislike(), false)
            };
            items.push(entry);
            items.push(self.menu_entry("queue", "Открыть очередь", icons.get_m_queue(), false));
        }
        items.push(self.menu_entry("playlist", "Добавить в плейлист", icons.get_m_add_playlist(), true));
        if t.lyrics && current {
            items.push(self.menu_entry("lyrics", "Показать текст песни", icons.get_m_lyrics(), false));
        }
        items.push(self.menu_entry("share", "Поделиться", icons.get_m_share(), true));
        if t.album_id.is_some() {
            items.push(self.menu_entry("album", "Перейти к альбому", icons.get_m_album(), false));
        }
        if !t.artist_list.is_empty() {
            items.push(self.menu_entry("artist", "Перейти к исполнителю", icons.get_m_artist(), false));
        }
        // Заголовок по типу (`track-modal.*-title`): глава, выпуск или трек.
        let about = if t.is_chapter() { "О главе" } else if t.is_episode() { "О выпуске" } else { "О треке" };
        items.push(self.menu_entry("info", about, icons.get_m_info(), false));
        items.push(self.menu_entry("complain", "Сообщить о проблеме", icons.get_m_complain(), false));
        self.st.borrow_mut().menu_track = Some(t);
        self.show_menu(items, x, y, below);
        self.load_menu_playlists();
    }

    fn account_action(self: &Rc<Self>, id: &str) {
        let ui = self.ui();
        let app = ui.global::<App>();
        if let Some(l) = id.strip_prefix("lang:") {
            crate::api::set_lang(l);
            crate::i18n::set_lang(l);
            app.set_lang(l.into());
            ui.global::<T>().set_lang(l.into());
            keys::show(&app);
            let mut st = self.st.borrow_mut();
            st.cfg.lang = l.to_string();
            st.cfg.save();
            return;
        }
        if let Some(mode) = id.strip_prefix("theme:") {
            let dark = match mode {
                "dark" => true,
                "light" => false,
                _ => !sys::system_light_theme(),
            };
            app.set_theme_mode(mode.into());
            {
                let mut st = self.st.borrow_mut();
                st.dark = dark;
                st.cfg.theme = mode.to_string();
                st.cfg.save();
            }
            ui.global::<Theme>().set_dark(app.get_page() == Page::Wave || dark);
            return;
        }
        if id == "explicit" {
            let allow = !app.get_allow_explicit();
            app.set_allow_explicit(allow);
            self.player.send(Cmd::AllowExplicit(allow));
            let mut st = self.st.borrow_mut();
            st.cfg.allow_explicit = allow;
            st.cfg.save();
            return;
        }
        app.set_account_open(false);
        match id {
            "account" => sys::open_url("https://id.yandex.ru"),
            "settings" => self.navigate(Page::Settings, None, true),
            "support" => sys::open_url(SUPPORT_CHAT),
            _ => {}
        }
    }

    fn setting(self: &Rc<Self>, id: &str) {
        let ui = self.ui();
        let app = ui.global::<App>();
        match id {
            "clear" => {
                self.images.clear();
                sys::release_memory();
                self.toast("Память очищена");
            }
            "mode:smooth" | "mode:lite" => {
                let lite = id == "mode:lite";
                apply_mode(&ui, lite);
                self.lite.set(lite);
                self.images.set_lite(lite);
                if lite {
                    sys::release_memory();
                }
                let mut st = self.st.borrow_mut();
                st.cfg.mode = if lite { "lite" } else { "smooth" }.to_string();
                st.cfg.save();
            }
            "less-ai" => {
                let on = !app.get_less_ai();
                app.set_less_ai(on);
                crate::api::LESS_AI.store(on, std::sync::atomic::Ordering::Relaxed);
                let Some(net) = self.st.borrow().net.clone() else { return };
                std::thread::spawn(move || {
                    if let Err(e) = net.api.set_account_setting("aiContentReductionEnabled", if on { "true" } else { "false" }) {
                        net.say(format!("Не удалось сохранить настройку: {e}"));
                    }
                });
            }
            "download-dir" => {
                let cur = music_dir(&self.st.borrow().cfg).display().to_string();
                let Some(dir) = sys::pick_folder(ui.window(), &crate::i18n::tr("Папка для скачивания"), &cur) else { return };
                app.set_download_dir(dir.clone().into());
                let mut st = self.st.borrow_mut();
                st.cfg.download_dir = dir;
                st.cfg.save();
            }
            "log-volume" => {
                let on = !app.get_log_volume();
                app.set_log_volume(on);
                self.player.send(Cmd::Loudness(if on { Loudness::Decibels } else { Loudness::Square }));
                let mut st = self.st.borrow_mut();
                st.cfg.log_volume = on;
                st.cfg.save();
            }
            "crossfade" => {
                let on = !app.get_crossfade();
                app.set_crossfade(on);
                let mut st = self.st.borrow_mut();
                st.cfg.crossfade_secs = if on { crate::config::CROSSFADE_SECS } else { 0 };
                self.player.send(Cmd::Crossfade(st.cfg.crossfade()));
                st.cfg.save();
            }
            "wave-motion" => {
                let on = !app.get_wave_motion();
                app.set_wave_motion(on);
                let mut st = self.st.borrow_mut();
                st.cfg.wave_motion = on;
                st.cfg.save();
            }
            "download-flac" => {
                let on = !app.get_download_flac();
                app.set_download_flac(on);
                let mut st = self.st.borrow_mut();
                st.cfg.download_flac = on;
                st.cfg.save();
            }
            "hidden-blocks" => {
                {
                    let mut st = self.st.borrow_mut();
                    st.cfg.hidden_blocks.clear();
                    st.cfg.save();
                }
                app.set_hidden_blocks(SharedString::new());
                self.toast("Скрытые блоки снова на главной");
            }
            "about" => sys::open_url(env!("CARGO_PKG_REPOSITORY")),
            "update-check" => check_updates_now(&ui),
            "releases" => sys::open_url(concat!(env!("CARGO_PKG_REPOSITORY"), "/releases")),
            "donate" => sys::open_url(DONATE_URL),
            "discord" => {
                let on = !app.get_discord_on();
                app.set_discord_on(on);
                self.discord.cfg.lock().unwrap().enabled = on;
                let mut st = self.st.borrow_mut();
                st.cfg.discord_enabled = on;
                st.cfg.save();
            }
            "autostart" => {
                let on = !app.get_autostart();
                if sys::set_autostart(on) {
                    app.set_autostart(on);
                } else {
                    self.toast("Не удалось изменить автозапуск");
                }
            }
            "check-updates" => {
                let on = !app.get_check_updates();
                app.set_check_updates(on);
                crate::update::ENABLED.store(on, Ordering::Relaxed);
                if !on {
                    app.set_update_version(SharedString::new());
                    UPDATE.with(|u| *u.borrow_mut() = None);
                }
                let mut st = self.st.borrow_mut();
                st.cfg.check_updates = on;
                st.cfg.save();
            }
            "logout" => self.logout(),
            _ => {}
        }
    }

    /// Меню «…» в шапке альбома или плейлиста.
    fn open_list_menu(self: &Rc<Self>, x: f32, y: f32) {
        let Some(data) = self.list() else { return };
        let Data::List(l) = &*data else { return };
        let ui = self.ui();
        let icons = ui.global::<Icons>();
        let app = ui.global::<App>();
        let pinned = self.list_target().is_some_and(|t| self.is_pinned(&t));
        let liked = app.get_list_liked();
        let likes = l.kind == ListKind::Likes;
        let mut items = vec![self.menu_entry(
            "list-pin",
            if pinned { "Открепить" } else { "Закрепить" },
            if pinned { icons.get_m_unpin() } else { icons.get_m_pin() },
            false,
        )];
        if !likes && !app.get_list_own() {
            items.push(self.menu_entry(
                "list-like",
                if liked { "Удалить из коллекции" } else { "Нравится" },
                if liked { icons.get_m_liked() } else { icons.get_m_like() },
                false,
            ));
        }
        if !likes {
            items.push(self.menu_entry("list-trailer", "Трейлер", icons.get_m_trailer(), false));
            let vibe = if l.album.is_some() { "Моя волна по альбому" } else { "Моя волна по плейлисту" };
            items.push(self.menu_entry("list-vibe", vibe, icons.get_m_vibe(), false));
        }
        if !l.tracks.is_empty() {
            items.push(self.menu_entry("list-playlist", "Добавить в плейлист", icons.get_m_add_playlist(), true));
        }
        if self.st.borrow().download_stop().is_some() {
            items.push(self.menu_entry("list-download-stop", "Отменить скачивание", icons.get_m_download(), false));
        } else if !l.tracks.is_empty() && !likes {
            // У «Мне нравится» скачивание вынесено кнопкой в шапку.
            items.push(self.menu_entry("list-download", "Скачать", icons.get_m_download(), false));
        }
        items.push(self.menu_entry("list-share", "Поделиться", icons.get_m_share(), true));
        if app.get_list_own() {
            items.push(self.menu_entry("list-rename", "Изменить название", icons.get_pencil(), false));
            let desc = if app.get_list_desc_raw().is_empty() { "Добавить описание" } else { "Изменить описание" };
            items.push(self.menu_entry("list-describe", desc, icons.get_edit(), false));
            let poster = if app.get_list_cover().size().width > 0 { "Изменить обложку" } else { "Добавить обложку" };
            items.push(self.menu_entry("list-cover", poster, icons.get_upload(), false));
            items.push(self.menu_entry("list-delete", "Удалить плейлист", icons.get_bucket_small(), false));
        }
        // Без трека menu_playlist добавит весь открытый список.
        self.st.borrow_mut().menu_track = None;
        self.show_menu(items, x, y, true);
        self.load_menu_playlists();
    }

    /// Цель закрепа открытого списка; «Мне нравится» — плейлист 3 владельца.
    fn list_target(&self) -> Option<String> {
        let st = self.st.borrow();
        match st.list_key.as_deref()? {
            "likes" => Some(format!("playlist:{}:3", st.uid)),
            k => Some(
                k.strip_prefix("artist-tracks:")
                    .or_else(|| k.strip_prefix("familiar:"))
                    .map_or_else(|| k.to_string(), |id| format!("artist:{id}")),
            ),
        }
    }

    fn is_pinned(&self, target: &str) -> bool {
        self.st.borrow().pins.iter().any(|p| p.target == target)
    }

    fn toggle_pin(self: &Rc<Self>, target: String) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        let pin = !self.is_pinned(&target);
        let ui = self.ui();
        let app = ui.global::<App>();
        if self.current_artist().is_some_and(|(id, _)| target.strip_prefix("artist:") == Some(id.as_str())) {
            app.set_ar_pinned(pin);
        }
        if self.list_target().as_deref() == Some(target.as_str()) {
            app.set_pinned(pin);
        }
        std::thread::spawn(move || {
            if let Err(e) = net.api.pin(&target, pin) {
                net.say(format!("Не удалось закрепить: {e}"));
            }
            Self::reload_pins(&net);
        });
    }

    /// Свой открытый плейлист: (uid, kind, название).
    fn own_playlist(&self) -> Option<(String, String, String)> {
        let data = self.list()?;
        let Data::List(l) = &*data else { return None };
        let p = l.playlist.as_ref()?;
        (p.uid == self.st.borrow().uid).then(|| (p.uid.clone(), p.kind.clone(), p.title.clone()))
    }

    /// После правки на сервере: перечитать плейлист, список плейлистов и коллекцию.
    fn refresh_playlist(net: &Net, uid: &str, kind: &str) {
        net.refresh(&format!("playlist:{uid}:{kind}"));
        net.refresh("playlists");
        net.refresh("collection");
    }

    /// `field` — `name` или `description`.
    fn list_save(self: &Rc<Self>, field: &'static str, text: String) {
        let Some((uid, kind, _)) = self.own_playlist() else { return };
        let Some(net) = self.st.borrow().net.clone() else { return };
        let ui = self.ui();
        let app = ui.global::<App>();
        let text = text.trim().to_string();
        if field == "name" {
            if text.is_empty() || text == app.get_list_title().as_str() {
                return;
            }
            app.set_list_title(text.clone().into());
            let target = format!("playlist:{uid}:{kind}");
            let mut pins = self.st.borrow().pins.clone();
            if let Some(p) = pins.iter_mut().find(|p| p.target == target) {
                p.title = text.clone();
                self.set_pins(pins);
            }
        } else {
            if text == app.get_list_desc_raw().as_str() {
                return;
            }
            app.set_list_desc_raw(text.clone().into());
            let shown = if text.is_empty() { format!("{} ...", crate::i18n::tr("Добавить описание")) } else { text.clone() };
            app.set_list_desc(shown.into());
        }
        std::thread::spawn(move || {
            match net.api.set_playlist_field(&uid, &kind, field, &text) {
                Ok(()) => {}
                Err(e) => {
                    let what = if field == "name" { "Не удалось изменить название плейлиста" } else { "Не удалось изменить описание плейлиста" };
                    net.say(format!("{}: {e}", crate::i18n::tr(what)));
                }
            }
            Self::refresh_playlist(&net, &uid, &kind);
            Self::reload_pins(&net);
        });
    }

    /// При переходах по разделам, но не чаще раза в минуту.
    fn check_pins(&self) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        let now = Instant::now();
        let due = self.st.borrow().pins_checked.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(60));
        if !due {
            return;
        }
        self.st.borrow_mut().pins_checked = Some(now);
        std::thread::spawn(move || Self::reload_pins(&net));
    }

    fn reload_pins(net: &Net) {
        if let Ok(p) = net.api.pins() {
            let session = net.clone();
            let _ = slint::invoke_from_event_loop(move || with_ctx(|c| {
                if c.same_session(&session) { c.set_pins(p); }
            }));
        }
    }

    fn list_change_cover(self: &Rc<Self>) {
        let Some((uid, kind, _)) = self.own_playlist() else { return };
        let Some(net) = self.st.borrow().net.clone() else { return };
        let ui = self.ui();
        let Some(path) = sys::pick_image(ui.window(), &crate::i18n::tr("Изменить обложку")) else { return };
        std::thread::spawn(move || {
            let res = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|bytes| {
                // Ограничение оригинала: файл до 20 МБ.
                if bytes.len() > 20_000_000 {
                    return Err("файл больше 20 МБ".to_string());
                }
                net.api.upload_playlist_cover(&uid, &kind, &path, bytes)
            });
            if let Err(e) = res {
                net.say(format!("{}: {e}", crate::i18n::tr("Не удалось изменить обложку плейлиста")));
            }
            Self::refresh_playlist(&net, &uid, &kind);
            Self::reload_pins(&net);
        });
    }

    fn list_remove_track(self: &Rc<Self>, row: usize) {
        let Some((uid, kind, title)) = self.own_playlist() else { return };
        let Some(net) = self.st.borrow().net.clone() else { return };
        let Some(at) = self.st.borrow().view.get(row).copied() else { return };
        let Some(t) = self.list().and_then(|d| match &*d {
            Data::List(l) => l.tracks.get(at).cloned(),
            _ => None,
        }) else {
            return;
        };
        std::thread::spawn(move || {
            match net.api.remove_from_playlist(&uid, &kind, &t, false) {
                Ok(()) => net.say(format!("Трек «{}» удален из плейлиста «{title}»", t.title)),
                Err(e) => net.say(format!("{}: {e}", crate::i18n::tr("Не удалось удалить трек из плейлиста"))),
            }
            Self::refresh_playlist(&net, &uid, &kind);
        });
    }

    fn list_delete(self: &Rc<Self>) {
        let Some((uid, kind, _)) = self.own_playlist() else { return };
        let Some(net) = self.st.borrow().net.clone() else { return };
        let target = format!("playlist:{uid}:{kind}");
        if self.is_pinned(&target) {
            let p: Vec<_> = self.st.borrow().pins.iter().filter(|p| p.target != target).cloned().collect();
            self.set_pins(p);
        }
        self.go_back();
        std::thread::spawn(move || {
            if let Err(e) = net.api.delete_playlist(&uid, &kind) {
                net.say(format!("{}: {e}", crate::i18n::tr("Не удалось удалить плейлист")));
            }
            net.refresh("playlists");
            net.refresh("collection");
            Self::reload_pins(&net);
        });
    }

    fn list_like(self: &Rc<Self>) {
        let Some(data) = self.list() else { return };
        let Data::List(l) = &*data else { return };
        let (kind, id) = match (&l.playlist, &l.album) {
            (Some(p), _) => ("playlists", format!("{}:{}", p.uid, p.kind)),
            (_, Some(a)) => ("albums", a.id.clone()),
            _ => return,
        };
        self.toggle_entity_like(kind, id, l.title.clone());
    }

    fn toggle_entity_like(self: &Rc<Self>, kind: &'static str, id: String, title: String) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        let key = if kind == "playlists" { format!("playlist:{id}") } else { id.clone() };
        let (desired, start) = {
            let mut st = self.st.borrow_mut();
            let had = match kind {
                "albums" => st.liked_albums.contains(&key),
                _ => st.liked_playlists.contains(&key),
            };
            let entry = st.entity_likes.entry((kind, id.clone()));
            let start = matches!(entry, std::collections::hash_map::Entry::Vacant(_));
            let pending = entry.or_insert(EntityLike { confirmed: had, desired: had });
            pending.desired = !pending.desired;
            (pending.desired, start)
        };
        self.show_entity_like(kind, &key, desired);
        if start { Self::save_entity_like(net, kind, id, key, title, desired); }
    }

    fn merge_entity_snapshot(&self, kind: &'static str, ids: Vec<String>) {
        let mut st = self.st.borrow_mut();
        let mut fresh: std::collections::HashSet<String> = ids.into_iter().collect();
        let State { saved_entities, entity_likes, .. } = &mut *st;
        merge_entity_likes(kind, &mut fresh, saved_entities, entity_likes);
        let selected = match kind {
            "albums" => st.list_key.as_deref().and_then(|k| k.strip_prefix("album:")),
            _ => st.list_key.as_deref().filter(|k| k.starts_with("playlist:")),
        }.map(|key| fresh.contains(key));
        match kind {
            "albums" => st.liked_albums = fresh,
            _ => st.liked_playlists = fresh,
        }
        drop(st);
        if let Some(liked) = selected {
            let ui = self.ui();
            let app = ui.global::<App>();
            app.set_list_liked(liked);
        }
    }

    fn show_entity_like(&self, kind: &str, key: &str, liked: bool) {
        let mut st = self.st.borrow_mut();
        let set = match kind {
            "albums" => &mut st.liked_albums,
            _ => &mut st.liked_playlists,
        };
        if liked { set.insert(key.into()); } else { set.remove(key); }
        let visible = match kind {
            "albums" => st.list_key.as_deref().and_then(|k| k.strip_prefix("album:")) == Some(key),
            _ => st.list_key.as_deref() == Some(key),
        };
        drop(st);
        if visible {
            let ui = self.ui();
            let app = ui.global::<App>();
            app.set_list_liked(liked);
        }
    }

    fn save_entity_like(net: Net, kind: &'static str, id: String, key: String, title: String, sent: bool) {
        std::thread::spawn(move || {
            let result = net.api.like_entity(&net.uid, kind, &id, !sent);
            if result.is_ok() && kind == "albums" { net.refresh("collection"); }
            let _ = slint::invoke_from_event_loop(move || with_ctx(|c| {
                let mut st = c.st.borrow_mut();
                if !st.net.as_ref().is_some_and(|active| Arc::ptr_eq(&active.toast, &net.toast)) { return; }
                let request = (kind, id.clone());
                if result.is_ok() { st.saved_entities.insert((kind, key.clone()), sent); }
                let Some(pending) = st.entity_likes.get_mut(&request) else { return };
                let next = pending.complete(sent, result.is_ok());
                let desired = pending.desired;
                if next.is_none() { st.entity_likes.remove(&request); }
                drop(st);
                c.show_entity_like(kind, &key, desired);
                if let Some(next) = next {
                    Self::save_entity_like(net, kind, id, key, title, next);
                } else {
                    net.say(match result {
                        Ok(()) if desired => format!("«{title}» в коллекции"),
                        Ok(()) => format!("«{title}» удалён из коллекции"),
                        Err(e) => format!("Не удалось сохранить: {e}"),
                    });
                }
            }));
        });
    }

    /// Ссылка на открытый список или код для вставки на сайт.
    fn list_link(&self, html: bool) -> Option<String> {
        let data = self.list()?;
        let Data::List(l) = &*data else { return None };
        let uid = self.st.borrow().uid.clone();
        let (page, frame) = match (&l.playlist, &l.album) {
            (Some(p), _) => {
                let owner = if p.login.is_empty() { &p.uid } else { &p.login };
                (format!("users/{owner}/playlists/{}", p.kind), format!("playlist/{owner}/{}", p.kind))
            }
            (_, Some(a)) => (format!("album/{}", a.id), format!("album/{}", a.id)),
            _ => (format!("users/{uid}/playlists/3"), format!("playlist/{uid}/3")),
        };
        let url = format!("https://music.yandex.ru/{page}");
        Some(if html { iframe_code(&frame, 614, 450, &format!("<a href=\"{url}\">{}</a>", l.title)) } else { url })
    }

    fn open_artist_menu(self: &Rc<Self>, x: f32, y: f32) {
        let ui = self.ui();
        let icons = ui.global::<Icons>();
        let disliked = self.current_artist().is_some_and(|(id, _)| self.st.borrow().artist_mark(&id) == ArtistMark::Dislike);
        let items = vec![
            self.menu_entry("artist-vibe", "Моя волна по исполнителю", icons.get_m_vibe(), false),
            if disliked {
                self.menu_entry("artist-dislike", "Убрать из «Не нравится»", icons.get_m_disliked(), false)
            } else {
                self.menu_entry("artist-dislike", "Не нравится", icons.get_m_dislike(), false)
            },
            self.menu_entry("artist-share", "Поделиться", icons.get_m_share(), false),
        ];
        self.show_menu(items, x, y, false);
    }

    /// При первом запуске открытое меню могло скопировать ещё пустые изображения.
    fn refresh_menu_icons(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        if !app.get_menu_open() { return; }
        let Some(first) = app.get_menu_items().row_data(0) else { return };
        let (x, y, submenu) = (app.get_menu_x(), app.get_menu_y(), app.get_submenu_open());
        match first.id.as_str() {
            "like" => {
                let track = self.st.borrow().menu_track.clone();
                if let Some(track) = track { self.open_menu_for(track, x, y, app.get_menu_below()); }
            }
            "list-pin" => self.open_list_menu(x, y),
            "artist-vibe" => self.open_artist_menu(x, y),
            _ => return,
        }
        app.set_submenu_open(submenu);
    }

    fn load_disliked_artists(self: &Rc<Self>) {
        if self.st.borrow().disliked_artists.is_some() {
            return;
        }
        let Some(net) = self.st.borrow().net.clone() else { return };
        let uid = self.st.borrow().uid.clone();
        std::thread::spawn(move || {
            let Ok(list) = net.api.disliked_artists(&uid) else { return };
            let ids = list.into_iter().map(|a| a.id).collect();
            let _ = slint::invoke_from_event_loop(move || with_ctx(|c| {
                if c.same_session(&net) { c.st.borrow_mut().disliked_artists = Some(ids); }
            }));
        });
    }

    fn show_artist_mark(&self) {
        if let Some((id, _)) = self.current_artist() {
            self.ui().global::<App>().set_ar_liked(self.st.borrow().artist_mark(&id) == ArtistMark::Like);
        }
    }

    fn toggle_artist_mark(&self, id: String, name: String, button: ArtistMark) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        let mut st = self.st.borrow_mut();
        let old = st.artist_mark(&id);
        let desired = if old == button { ArtistMark::Neutral } else { button };
        let entry = st.pending_artist_marks.entry(id.clone());
        let start = matches!(entry, std::collections::hash_map::Entry::Vacant(_));
        entry.or_insert(EntityLike { confirmed: old, desired }).desired = desired;
        drop(st);
        self.show_artist_mark();
        if start { Self::save_artist_mark(net, id, name, old, desired); }
    }

    fn save_artist_mark(net: Net, id: String, name: String, previous: ArtistMark, sent: ArtistMark) {
        std::thread::spawn(move || {
            let (dislike, remove) = sent.request(previous);
            let result = if dislike { net.api.dislike_artist(&net.uid, &id, remove) }
                else { net.api.like_entity(&net.uid, "artists", &id, remove) };
            let _ = slint::invoke_from_event_loop(move || with_ctx(|c| {
                if !c.same_session(&net) { return; }
                let mut st = c.st.borrow_mut();
                if result.is_ok() { st.saved_artist_marks.insert(id.clone(), sent); }
                let saved = st.saved_artist_marks.get(&id).copied().unwrap_or_else(|| st.remote_artist_mark(&id));
                let Some(pending) = st.pending_artist_marks.get_mut(&id) else { return };
                pending.confirmed = saved;
                let next = pending.complete(sent, result.is_ok());
                if next.is_none() { st.pending_artist_marks.remove(&id); }
                drop(st);
                c.show_artist_mark();
                if let Some(next) = next {
                    Self::save_artist_mark(net, id, name, saved, next);
                } else {
                    net.say(match result {
                        Err(e) => format!("Не удалось сохранить: {e}"),
                        Ok(()) => match (sent, dislike) {
                            (ArtistMark::Like, _) => format!("«{name}» в коллекции"),
                            (ArtistMark::Dislike, _) => format!("{name}: не нравится"),
                            (ArtistMark::Neutral, true) => format!("{name} убран из «Не нравится»"),
                            (ArtistMark::Neutral, false) => format!("«{name}» удалён из коллекции"),
                        },
                    });
                }
            }));
        });
    }

    fn show_menu(&self, items: Vec<MenuEntry>, x: f32, y: f32, below: bool) {
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_menu_themed(app.get_page() != Page::Wave && !app.get_fullscreen());
        app.set_menu_below(below);
        app.set_menu_items(ModelRc::from(Rc::new(VecModel::from(items))));
        app.set_menu_x(x);
        app.set_menu_y(y);
        app.set_submenu_open(-1);
        app.set_menu_open(true);
    }

    fn load_menu_playlists(self: &Rc<Self>) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        let uid = self.st.borrow().uid.clone();
        if let Some(Ok(data)) = net.get("playlists") {
            if let Data::Playlists(pls) = &*data {
                let own: Vec<Playlist> = pls.iter().filter(|p| p.uid == uid).cloned().collect();
                let names: Vec<SharedString> = own.iter().map(|p| SharedString::from(p.title.as_str())).collect();
                let ui = self.ui();
                let app = ui.global::<App>();
                app.set_menu_playlist_has(ModelRc::from(Rc::new(VecModel::from(vec![false; own.len()]))));
                app.set_menu_playlists(ModelRc::from(Rc::new(VecModel::from(names))));
                let kinds: Vec<String> = own.iter().map(|p| p.kind.clone()).collect();
                self.st.borrow_mut().menu_playlists = own;
                if let Some(t) = self.st.borrow().menu_track.clone() {
                    Self::mark_menu_playlists(net, uid, kinds, t.id);
                }
            }
        }
    }

    /// Галочки у плейлистов, где трек уже есть.
    fn mark_menu_playlists(net: Net, uid: String, kinds: Vec<String>, track_id: String) {
        std::thread::spawn(move || {
            let has = match net.api.playlists_with_track(&uid, &kinds, &track_id) {
                Ok(has) => has,
                Err(e) => return log(format!("плейлисты с треком {track_id}: {e}")),
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|c| {
                    let st = c.st.borrow();
                    if st.menu_track.as_ref().is_none_or(|t| t.id != track_id) {
                        return;
                    }
                    let marks: Vec<bool> = st.menu_playlists.iter().map(|p| has.contains(&p.kind)).collect();
                    c.ui().global::<App>().set_menu_playlist_has(ModelRc::from(Rc::new(VecModel::from(marks))));
                });
            });
        });
    }

    fn menu_action(self: &Rc<Self>, id: &str) {
        if matches!(id, "label-sort-popular" | "label-sort-date") {
            let ui = self.ui();
            let app = ui.global::<App>();
            let by_popularity = id == "label-sort-popular";
            if app.get_page() == Page::Grid && app.get_grid_label_sortable()
                && app.get_grid_label_popular() != by_popularity {
                app.set_grid_label_popular(by_popularity);
                self.refresh_grid();
            }
            return;
        }
        if let Some(target) = id.strip_prefix("unpin:") {
            if self.is_pinned(target) { self.toggle_pin(target.to_owned()); }
            return;
        }
        if let Some(rest) = id.strip_prefix("artist-") {
            let Some((aid, name)) = self.current_artist() else { return };
            match rest {
                "vibe" => {
                    self.player.send(Cmd::Wave(Source::Seed { station: format!("artist:{aid}"), header: "Моя волна по исполнителю".into(), title: name }));
                    self.start();
                }
                "dislike" => self.toggle_artist_mark(aid, name, ArtistMark::Dislike),
                "share" => {
                    sys::copy_text(&format!("https://music.yandex.ru/artist/{aid}"));
                    self.toast("Ссылка скопирована");
                }
                _ => {}
            }
            return;
        }
        if let Some(rest) = id.strip_prefix("list-") {
            match rest {
                "pin" => {
                    if let Some(t) = self.list_target() {
                        self.toggle_pin(t);
                    }
                }
                "like" => self.list_like(),
                "trailer" => self.ui().global::<App>().invoke_list_trailer(),
                "download" => self.ui().global::<App>().invoke_list_download(),
                "download-stop" => {
                    if let Some(stop) = self.st.borrow().download_stop() {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                "vibe" => {
                    let Some(data) = self.list() else { return };
                    let Data::List(l) = &*data else { return };
                    let (station, header) = match (&l.playlist, &l.album) {
                        (Some(p), _) => (format!("playlist:{}_{}", p.uid, p.kind), "Моя волна по плейлисту"),
                        (_, Some(a)) => (format!("album:{}", a.id), "Моя волна по альбому"),
                        _ => return,
                    };
                    self.player.send(Cmd::Wave(Source::Seed { station, header: header.into(), title: l.title.clone() }));
                    self.start();
                }
                "rename" => self.ui().global::<App>().set_list_edit_request(1),
                "describe" => self.ui().global::<App>().set_list_edit_request(2),
                "cover" => self.list_change_cover(),
                "delete" => self.list_delete(),
                "share-link" | "share-html" => {
                    if let Some(s) = self.list_link(rest == "share-html") {
                        sys::copy_text(&s);
                        self.toast(if rest == "share-html" { "HTML-код скопирован" } else { "Ссылка скопирована" });
                    }
                }
                _ => {}
            }
            return;
        }
        let Some(t) = self.st.borrow().menu_track.clone() else { return };
        let net = self.st.borrow().net.clone();
        match id {
            "like" => self.like(t),
            "trailer" => self.play_trailer(format!("tracks/{}", t.id)),
            "vibe" => {
                self.player.send(Cmd::Wave(Source::Seed { station: format!("track:{}", t.id), header: "Моя волна по треку".into(), title: t.title.clone() }));
                self.start();
            }
            "download" => {
                let Some(net) = net else { return };
                let api = net.api.clone();
                let dir = music_dir(&self.st.borrow().cfg);
                let flac = self.st.borrow().cfg.download_flac;
                self.toast(format!("Скачиваем «{}»", t.title));
                std::thread::spawn(move || {
                    let names = legacy_names(std::slice::from_ref(&t));
                    let res = save_track(&api, &dir, &t, &names, flac, &AtomicBool::new(false));
                    net.say(match res {
                        Ok(p) => format!("Сохранено: {}", p.display()),
                        Err(e) => format!("Не удалось скачать: {e}"),
                    });
                });
            }
            "next" => {
                self.player.send(Cmd::Enqueue { track: t, next: true });
                self.toast("Трек будет следующим");
            }
            "last" => {
                self.player.send(Cmd::Enqueue { track: t, next: false });
                self.toast("Трек добавлен в конец очереди");
            }
            "dislike" => self.dislike(),
            "lyrics" => self.open_fullscreen(2),
            "queue" => self.open_fullscreen(1),
            "share-link" => {
                sys::copy_text(&track_link(&t));
                self.toast("Ссылка скопирована");
            }
            "share-html" => {
                let frame = match &t.album_id {
                    Some(a) => format!("album/{a}/track/{}", t.id),
                    None => format!("track/{}", t.id),
                };
                let mut msg = format!("<a href=\"{}\">{}</a>", track_link(&t), t.title);
                if let Some(a) = t.artist_list.first() {
                    msg += &format!(" — <a href=\"https://music.yandex.ru/artist/{}\">{}</a>", a.id, a.name);
                }
                sys::copy_text(&iframe_code(&frame, 614, 244, &msg));
                self.toast("HTML-код скопирован");
            }
            "album" => {
                if let Some(a) = &t.album_id {
                    self.ui().global::<App>().set_fullscreen(false);
                    self.navigate(Page::List, Some(format!("album:{a}")), true);
                }
            }
            "artist" => {
                if let Some(a) = t.artist_list.first() {
                    self.ui().global::<App>().set_fullscreen(false);
                    self.navigate(Page::Artist, Some(format!("artist:{}", a.id)), true);
                }
            }
            "info" => self.open_track_about(t),
            "complain" => sys::open_url("https://yandex.ru/support/music/"),
            _ => {}
        }
    }

    fn menu_playlist(self: &Rc<Self>, i: usize) {
        let (t, pl, net) = {
            let st = self.st.borrow();
            (st.menu_track.clone(), st.menu_playlists.get(i).cloned(), st.net.clone())
        };
        let (Some(pl), Some(net)) = (pl, net) else { return };
        if let Some(t) = t {
            let has = self.ui().global::<App>().get_menu_playlist_has().row_data(i).unwrap_or(false);
            if !has {
                let title = t.title.clone();
                net.add_to_playlist(&pl, vec![t], title);
                return;
            }
            // Как у оригинала: повторный выбор плейлиста убирает из него трек.
            let t = t.detached();
            std::thread::spawn(move || {
                match net.api.remove_from_playlist(&pl.uid, &pl.kind, &t, true) {
                    Ok(()) => net.say(format!("Трек «{}» удален из плейлиста «{}»", t.title, pl.title)),
                    Err(e) => net.say(format!("{}: {e}", crate::i18n::tr("Не удалось удалить трек из плейлиста"))),
                }
                Self::refresh_playlist(&net, &pl.uid, &pl.kind);
            });
            return;
        }
        // Меню открыто у альбома или плейлиста — добавляем все его треки.
        let Some(data) = self.list() else { return };
        let Data::List(l) = &*data else { return };
        if !l.tracks.is_empty() {
            net.add_to_playlist(&pl, l.tracks.clone(), l.title.clone());
        }
    }

    // ---------- полноэкранный плеер ----------

    fn open_fullscreen(self: &Rc<Self>, mode: i32) {
        let ui = self.ui();
        let app = ui.global::<App>();
        if self.player.state().track.is_none() {
            return;
        }
        app.set_fs_mode(mode);
        app.set_fullscreen(true);
        app.set_menu_open(false);
        app.set_eq_open(false);
        self.load_cover_big();
        self.sync_lyrics();
    }

    fn load_cover_big(self: &Rc<Self>) {
        let Some(t) = self.player.state().track else { return };
        let size = self.size_for(400.0);
        let Some(url) = t.cover_url(size) else { return };
        let w = self.ui.clone();
        let id = t.id.clone();
        self.images.get(&url, size, move |img| {
            with_ctx(|c| {
                if c.st.borrow().track_id.as_deref() == Some(id.as_str()) {
                    if let Some(ui) = w.upgrade() {
                        ui.global::<App>().set_cover_big(img.clone());
                    }
                }
            })
        });
    }

    /// Текст песни грузится, только когда его показывают.
    fn sync_lyrics(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        if !app.get_fullscreen() || app.get_fs_mode() != 2 {
            return;
        }
        let Some(t) = self.player.state().track else { return };
        let Some(net) = self.st.borrow().net.clone() else { return };
        let key = format!("lyrics:{}", t.id);
        match net.get(&key) {
            None => app.set_lyrics_loading(true),
            Some(res) => {
                app.set_lyrics_loading(false);
                if self.st.borrow().lyrics_for.as_deref() == Some(key.as_str()) {
                    return;
                }
                self.st.borrow_mut().lyrics_for = Some(key);
                let (lines, writers, source, synced) = match res.as_deref() {
                    Ok(Data::Lyrics(l)) if !l.synced.is_empty() => (l.synced.clone(), l.writers.clone(), l.source.clone(), true),
                    Ok(Data::Lyrics(l)) => (
                        l.text.iter().map(|s| crate::api::SyncLine { from: -1.0, to: None, text: s.clone() }).collect(),
                        l.writers.clone(),
                        l.source.clone(),
                        false,
                    ),
                    _ => (Vec::new(), String::new(), String::new(), false),
                };
                app.set_lyrics_writers(writers.into());
                app.set_lyrics_source(source.into());
                app.set_lyrics(ModelRc::from(Rc::new(VecModel::from(
                    lines.iter().map(|l| LyricLine { text: l.text.clone().into(), time: l.from }).collect::<Vec<_>>(),
                ))));
                {
                    let mut st = self.st.borrow_mut();
                    st.lyrics = lines;
                    st.lyrics_static = !synced;
                    st.lyric_target = -1;
                }
                app.set_lyrics_scrolled(!synced);
                app.set_lyrics_epoch(app.get_lyrics_epoch() + 1);
                self.update_lyric_current(self.player.state().position);
            }
        }
    }

    fn reset_lyrics(&self) {
        {
            let mut st = self.st.borrow_mut();
            st.lyrics_for = None;
            st.lyrics.clear();
            st.lyrics_static = false;
            st.lyric_target = -1;
            st.lyrics_wheel_timer = None;
        }
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_lyrics(ModelRc::default());
        app.set_lyric_current(-1);
        app.set_lyric_target(-1);
        app.set_lyrics_scrolled(false);
        app.set_lyrics_epoch(app.get_lyrics_epoch() + 1);
    }

    /// Стадия и текущая строка, как у SyncLyricsScroller: до первой строки — точки,
    /// за 3 с до неё — отсчёт, после конца последней — финал.
    fn update_lyric_current(&self, pos: f32) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let mut st = self.st.borrow_mut();
        if st.lyrics_static || st.lyrics.is_empty() {
            app.set_lyric_state(2);
            app.set_lyric_current(-1);
            return;
        }
        let start = st.lyrics[0].from;
        let end = st.lyrics.last().and_then(|l| l.to);
        let state = if start >= 3.0 && start - pos > 0.0 && start - pos <= 3.0 {
            1
        } else if start > 0.0 && start > pos {
            0
        } else if end.is_some_and(|e| pos > e) {
            3
        } else {
            2
        };
        let active = if (start > 0.0 && pos < start) || end.is_some_and(|e| pos > e) {
            None
        } else {
            st.lyrics.iter().position(|l| pos >= l.from && l.to.is_none_or(|to| to >= pos))
        };
        if let Some(i) = active {
            st.lyric_target = i as i32;
        }
        if state != 2 {
            st.lyric_target = -1;
        }
        app.set_lyric_state(state);
        app.set_lyric_counter(if state == 1 { (start - pos).ceil() as i32 } else { 0 });
        app.set_lyric_current(active.map(|i| i as i32).unwrap_or(-1));
        app.set_lyric_target(st.lyric_target);
    }

    /// Колесо над текстом: все строки видны и кликабельны, через 3 с без прокрутки — обратно.
    fn lyrics_wheel(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_lyrics_upper(false);
        app.set_lyrics_scrolled(true);
        if self.st.borrow().lyrics_static {
            return;
        }
        let w = self.ui.clone();
        let t = Timer::default();
        t.start(TimerMode::SingleShot, Duration::from_secs(3), move || {
            if let Some(ui) = w.upgrade() {
                ui.global::<App>().set_lyrics_scrolled(false);
            }
        });
        self.st.borrow_mut().lyrics_wheel_timer = Some(t);
    }

    /// Мышь двигалась: на 3 с видны строки выше текущей.
    fn lyrics_mouse(self: &Rc<Self>) {
        let ui = self.ui();
        let app = ui.global::<App>();
        if app.get_lyrics_scrolled() {
            return;
        }
        app.set_lyrics_upper(true);
        if let Some(t) = &self.st.borrow().lyrics_mouse_timer {
            t.restart();
            return;
        }
        let w = self.ui.clone();
        let t = Timer::default();
        t.start(TimerMode::SingleShot, Duration::from_secs(3), move || {
            if let Some(ui) = w.upgrade() {
                ui.global::<App>().set_lyrics_upper(false);
            }
        });
        self.st.borrow_mut().lyrics_mouse_timer = Some(t);
    }

    fn sync_queue(self: &Rc<Self>, s: &crate::player::Shared) {
        let ids: Vec<String> = s.upcoming.iter().map(|t| t.id.clone()).collect();
        if self.st.borrow().queue_ids == ids {
            return;
        }
        self.st.borrow_mut().queue_ids = ids;
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_queue(self.track_model(&s.upcoming));
        let (header, title) = match &s.source {
            Source::Wave => (String::new(), "Моя волна".to_string()),
            Source::Seed { header, title, .. } => (header.clone(), title.clone()),
            Source::List { kind, title, .. } => (
                match kind {
                    ListKind::Album => "альбом",
                    ListKind::Artist => "исполнитель",
                    _ => "плейлист",
                }
                .to_string(),
                title.clone(),
            ),
        };
        app.set_queue_header(header.into());
        app.set_queue_title(title.into());
    }

    /// Пятно и фон волны: в паузе — исходная картинка и чёрный фон, во время игры — цвета трека.
    fn sync_vibe(self: &Rc<Self>, playing: bool, color: Option<[u8; 3]>) {
        let key = (playing, if playing { color } else { None });
        if self.st.borrow().vibe_key == Some(key) && self.st.borrow().blob_src.is_some() {
            return;
        }
        let ui = self.ui();
        let app = ui.global::<App>();
        let bucket = vibe_bucket(color);
        let stops: Vec<Color> = if playing {
            bucket.4.iter().map(|c| rgb(*c)).collect()
        } else {
            vec![Color::from_rgb_u8(0, 0, 0); 16]
        };
        app.set_vibe_edge(stops[11]);
        app.set_vibe_stops(ModelRc::from(Rc::new(VecModel::from(stops))));
        let Some(src) = self.st.borrow().blob_src.clone() else {
            if std::mem::replace(&mut self.st.borrow_mut().blob_requested, true) {
                return;
            }
            let w = self.ui.clone();
            self.images.get(BLOB_URL, 400, move |img| {
                with_ctx(|c| {
                    c.st.borrow_mut().blob_src = img.to_rgba8();
                    c.st.borrow_mut().vibe_key = None;
                    if let Some(ui) = w.upgrade() {
                        ui.global::<App>().set_blob(img.clone());
                    }
                    let s = c.player.state();
                    let color = s.track.as_ref().and_then(|t| t.color);
                    c.sync_vibe(s.playing && c.st.borrow().started, color);
                })
            });
            return;
        };
        self.st.borrow_mut().vibe_key = Some(key);
        if playing {
            app.set_blob(recolor(&src, bucket.4[2]));
        } else {
            app.set_blob(Image::from_rgba8(src));
        }
    }

    // ---------- настройки звука ----------

    fn apply_eq(&self) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let st = self.st.borrow();
        let c = &st.cfg;
        self.player.eq.set(c.eq_on, c.eq_preamp, &c.eq_gains);
        app.set_eq_on(c.eq_on);
        app.set_eq_preamp(c.eq_preamp);
        app.set_eq_preset(c.eq_preset.min(crate::eq::PRESETS.len() - 1) as i32);
        let gains = app.get_eq_gains();
        if gains.row_count() == c.eq_gains.len() {
            // Замена модели пересоздаёт ползунки и прерывает захват указателя.
            for (i, &gain) in c.eq_gains.iter().enumerate() {
                if gains.row_data(i) != Some(gain) {
                    gains.set_row_data(i, gain);
                }
            }
        } else {
            app.set_eq_gains(ModelRc::from(Rc::new(VecModel::from(c.eq_gains.to_vec()))));
        }
        app.set_quality(c.quality as i32);
        crate::api::QUALITY.store(c.quality, std::sync::atomic::Ordering::Relaxed);
        c.save();
    }
}

impl Ctx {
    /// Где мы сейчас: страница и её ключ — для истории переходов.
    fn here(&self) -> (Page, Option<String>) {
        let st = self.st.borrow();
        let ui = self.ui();
        let app = ui.global::<App>();
        match app.get_page() {
            Page::Artist => (Page::Artist, st.artist_key.clone()),
            Page::List => (Page::List, st.list_key.clone()),
            Page::Grid => (Page::Grid, st.grid_key.clone()),
            Page::Landing => (Page::Landing, st.landing_key.clone()),
            Page::Search if app.get_search_tracks_only() => (Page::Search, Some(SEARCH_TRACK_KIND.into())),
            p => (p, None),
        }
    }

    fn go_back(self: &Rc<Self>) {
        let Some(prev) = self.st.borrow_mut().history.pop() else { return };
        let cur = self.here();
        self.st.borrow_mut().forward.push(cur);
        self.navigate(prev.0, prev.1, false);
    }

    fn go_forward(self: &Rc<Self>) {
        let Some(next) = self.st.borrow_mut().forward.pop() else { return };
        let cur = self.here();
        self.st.borrow_mut().history.push(cur);
        self.navigate(next.0, next.1, false);
    }

    /// Ушли со страницы: отдать её модели и обложки. Данные остаются в кэше сети,
    /// при возврате страница собирается заново.
    fn release_page(&self, page: Page) {
        let ui = self.ui();
        let app = ui.global::<App>();
        let mut st = self.st.borrow_mut();
        match page {
            Page::List => {
                st.list_shown = None;
                st.view.clear();
                st.search_index.clear();
                drop(st);
                app.set_list_count(0);
                app.set_tracks(ModelRc::default());
                app.set_list_cover(Image::default());
                app.set_list_artist_cover(Image::default());
            }
            Page::Artist => {
                st.artist_shown = None;
                drop(st);
                app.set_ar_cover(Image::default());
                app.set_ar_popular(ModelRc::default());
                for f in [App::set_ar_release, App::set_ar_studio, App::set_ar_albums, App::set_ar_playlists, App::set_ar_compilations, App::set_ar_similar] {
                    f(&app, ModelRc::default());
                }
            }
            Page::Grid => {
                st.grid.clear();
                st.cards.remove("label-releases");
                st.cards.remove("label-artists");
                drop(st);
                app.set_grid_label_releases(ModelRc::default());
                app.set_grid_label_artists(ModelRc::default());
                self.images.cancel_pending();
                app.set_grid_count(0);
                app.set_grid_cards(ModelRc::default());
            }
            Page::Search => {
                st.search_shown = None;
                st.results_shown = None;
                drop(st);
                for f in [App::set_search_mixes, App::set_search_missed, App::set_search_newpl, App::set_search_history, App::set_sr_artists, App::set_sr_albums, App::set_sr_playlists] {
                    f(&app, ModelRc::default());
                }
                app.set_search_chart(ModelRc::default());
                app.set_sr_tracks(ModelRc::default());
            }
            Page::Collection => {
                st.coll_shown = None;
                drop(st);
                app.set_likes_cover(Image::default());
                app.set_likes_preview(ModelRc::default());
                app.set_top_artists(ModelRc::default());
                app.set_playlists(ModelRc::default());
                app.set_albums(ModelRc::default());
            }
            Page::Trends | Page::Concerts | Page::Nonmusic | Page::Landing => {
                drop(st);
                self.release_landing();
            }
            Page::Chart => {
                drop(st);
                self.release_chart();
            }
            Page::History => {
                drop(st);
                self.release_history();
            }
            Page::Artists => {
                drop(st);
                app.set_top_artists(ModelRc::default());
                app.set_my_artists(ModelRc::default());
            }
            _ => {}
        }
        self.images.trim();
        if self.lite.get() {
            sys::release_memory();
        }
    }

    fn like(self: &Rc<Self>, t: Track) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        self.player.send(Cmd::LikeTrack(t, Some(Box::new(move |track, liked, result| {
            if let Err(e) = result {
                net.say(format!("Лайк не сохранился: {e}"));
                return;
            }
            net.like_saved(track, liked);
            let (track, uid) = (track.clone(), net.uid.clone());
            let _ = slint::invoke_from_event_loop(move || with_ctx(|c| {
                if c.st.borrow().uid == uid { c.like_toast(&track, liked); }
            }));
        }))));
    }

    fn dislike(self: &Rc<Self>) {
        let net = self.st.borrow().net.clone();
        self.player.send(Cmd::Dislike(Some(Box::new(move |track, added, result| {
            let Some(net) = net.filter(|_| result.is_ok()) else { return };
            if added { net.dislike_saved(track) } else { net.forget("dislikes") }
        }))));
    }

    fn like_toast(self: &Rc<Self>, t: &Track, liked: bool) {
        let msg = if liked {
            format!("Трек «{}» добавлен в плейлист «Мне нравится»", t.title)
        } else {
            format!("Трек «{}» удален из плейлиста «Мне нравится»", t.title)
        };
        self.toast(msg);
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_toast_cover(Image::default());
        app.set_toast_has_cover(true);
        let size = self.size_for(40.0);
        if let Some(url) = t.cover_url(size) {
            let w = self.ui.clone();
            self.images.get(&url, size, move |img| {
                if let Some(ui) = w.upgrade() {
                    ui.global::<App>().set_toast_cover(img.clone());
                }
            });
        }
    }

    /// Трек из строки списка: `block` говорит, из какого.
    fn row_track(&self, block: &str, i: usize) -> Option<Track> {
        match block {
            "list" => self.view_track(i),
            "ar-popular" => self.st.borrow().artist_tracks.get(i).cloned(),
            "chart" => self.st.borrow().chart.get(i).cloned(),
            "results" => self.st.borrow().results.get(i).cloned(),
            "history" => self.history_track(i),
            "likes-preview" => {
                let data = self.collection()?;
                let Data::Collection(c) = &*data else { return None };
                c.likes_preview.get(i).cloned()
            }
            _ => self.landing_track(block, i),
        }
    }

    fn play_trailer(self: &Rc<Self>, path: String) {
        let Some(net) = self.st.borrow().net.clone() else { return };
        let api = net.api.clone();
        let request = self.player.begin_source_request();
        std::thread::spawn(move || {
            let res = api.trailer(&path);
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|c| match res {
                    _ if !c.player.is_current_request(request) => {}
                    Ok((title, tracks)) => {
                        c.st.borrow_mut().trailer = Some(Trailer {
                            ids: tracks.iter().map(|t| t.id.clone()).collect(),
                            seeked: None,
                            start: 0.0,
                            seek_at: None,
                            done: None,
                            began: false,
                        });
                        c.player.send_for_request(request, Cmd::PlayList {
                            source: Source::List { kind: ListKind::Tracks, id: format!("trailer:{path}"), title },
                            tracks,
                            start: 0,
                            after: After::Wave(Source::Wave),
                        });
                        c.start();
                    }
                    Err(e) => c.toast(format!("Трейлер недоступен: {e}")),
                })
            });
        });
    }

    /// Трейлер: перемотать на середину, через 12,5 с — следующий, после последнего — пауза.
    fn sync_trailer(self: &Rc<Self>, s: &crate::player::Shared) {
        let Some((ids, seeked, start, seek_at, done, began)) =
            self.st.borrow().trailer.as_ref().map(|t| (t.ids.clone(), t.seeked.clone(), t.start, t.seek_at, t.done.clone(), t.began))
        else {
            return;
        };
        let Some(t) = &s.track else { return };
        let Some(at) = ids.iter().position(|i| *i == t.id) else {
            if began {
                self.st.borrow_mut().trailer = None;
            }
            return;
        };
        if !began {
            if let Some(tr) = self.st.borrow_mut().trailer.as_mut() {
                tr.began = true;
            }
        }
        // Позиция пошла — аудио загружено, перемотка дойдёт до плеера.
        if s.loading || s.duration <= 0.0 || s.position <= 0.0 || done.as_deref() == Some(t.id.as_str()) {
            return;
        }
        if seeked.as_deref() != Some(t.id.as_str()) {
            let from = (s.duration * 0.33).min((s.duration - TRAILER_PART - 1.0).max(0.0));
            self.player.send(Cmd::Seek(from));
            if let Some(tr) = self.st.borrow_mut().trailer.as_mut() {
                tr.seeked = Some(t.id.clone());
                tr.start = from;
                tr.seek_at = Some(Instant::now());
            }
            return;
        }
        // Перемотка не сработала (или ещё не дошла) — отсчёт от фактической позиции.
        if s.position + 3.0 < start && seek_at.is_some_and(|a| a.elapsed() > Duration::from_millis(1500)) {
            if let Some(tr) = self.st.borrow_mut().trailer.as_mut() {
                tr.start = s.position;
            }
            return;
        }
        if s.position + 3.0 < start {
            return;
        }
        if s.position >= start + TRAILER_PART {
            if let Some(tr) = self.st.borrow_mut().trailer.as_mut() {
                tr.done = Some(t.id.clone());
            }
            if at + 1 >= ids.len() {
                self.st.borrow_mut().trailer = None;
                if s.playing {
                    self.player.send(Cmd::PlayPause);
                }
            } else {
                self.player.send(Cmd::Next);
            }
        }
    }

    /// Скачать список в «Музыка/Яндекс Музыка/<название>», по одному треку, с прогрессом.
    fn download_tracks(self: &Rc<Self>, folder: String, tracks: Vec<Track>) {
        if tracks.is_empty() {
            return;
        }
        if self.st.borrow().download.is_some() {
            self.toast("Скачивание уже идёт");
            return;
        }
        let Some(net) = self.st.borrow().net.clone() else { return };
        let stop = Arc::new(AtomicBool::new(false));
        let Some(list_key) = self.st.borrow().list_key.clone() else { return };
        self.st.borrow_mut().download = Some(ListDownload { list_key: ListDownload::key(&net.uid, &list_key), uid: net.uid.clone(), stop: stop.clone() });
        self.toast(format!("Скачиваем {}", tracks_word(tracks.len() as u64)));
        let api = net.api.clone();
        let dir = music_dir(&self.st.borrow().cfg).join(file_name(&folder));
        let flac = self.st.borrow().cfg.download_flac;
        std::thread::spawn(move || {
            let total = tracks.len();
            let (mut ok, mut failed) = (0, 0);
            let names = legacy_names(&tracks);
            for (i, t) in tracks.iter().enumerate() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match save_track(&api, &dir, t, &names, flac, &stop) {
                    Ok(_) => ok += 1,
                    Err(_) if stop.load(Ordering::Relaxed) => break,
                    Err(e) => {
                        failed += 1;
                        log(format!("скачивание {}: {e}", t.id));
                    }
                }
                if (i + 1) % 10 == 0 && i + 1 < total {
                    net.say(format!("Скачано {} из {total}", i + 1));
                }
            }
            net.say(if stop.load(Ordering::Relaxed) {
                format!("Скачивание отменено, скачано {ok} из {total}: {}", dir.display())
            } else if failed == 0 {
                format!("Скачано {ok} из {total}: {}", dir.display())
            } else {
                format!("Скачано {ok} из {total}, не удалось {failed}: {}", dir.display())
            });
            let _ = slint::invoke_from_event_loop(|| with_ctx(|c| c.st.borrow_mut().download = None));
        });
    }
}

fn music_dir(cfg: &Config) -> std::path::PathBuf {
    if !cfg.download_dir.is_empty() {
        return cfg.download_dir.clone().into();
    }
    sys::music_dir().join("Яндекс Музыка")
}

fn legacy_stem(t: &Track) -> String {
    file_name_max(&format!("{} — {}", t.artists, t.title), 120)
}

struct DownloadNames<'a> {
    legacy: HashMap<String, Option<&'a str>>,
    numbered: std::cell::OnceCell<std::io::Result<HashMap<String, std::collections::BTreeSet<u64>>>>,
}

fn numbered_downloads(dir: &std::path::Path) -> std::io::Result<HashMap<String, std::collections::BTreeSet<u64>>> {
    let mut found = HashMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let Some((id, n)) = stem.strip_prefix("yamusic-").and_then(|s| s.rsplit_once('-')) else { continue };
        if let Ok(n) = n.parse::<u64>() { found.entry(id.to_owned()).or_insert_with(std::collections::BTreeSet::new).insert(n); }
    }
    Ok(found)
}

fn legacy_names(tracks: &[Track]) -> DownloadNames<'_> {
    let mut names = HashMap::new();
    for track in tracks {
        let id = Some(track.id.as_str());
        names.entry(legacy_stem(track).to_lowercase()).and_modify(|known| {
            if *known != id { *known = None; }
        }).or_insert(id);
    }
    DownloadNames { legacy: names, numbered: Default::default() }
}

fn track_file_stem(t: &Track) -> String {
    use sha2::{Digest, Sha256};
    let id = if !t.id.is_empty() && t.id.len() <= 24 && t.id.bytes().all(|b| b.is_ascii_digit()) {
        t.id.clone()
    } else {
        format!("{:x}", Sha256::digest(t.id.as_bytes()))[..24].to_owned()
    };
    let name = legacy_stem(t);
    // Оставляем место для ID и расширения в пределах 255 байт файловой системы.
    let mut end = name.len().min(255 - ".tagged.part".len() - " []".len() - id.len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [{id}]", &name[..end])
}

/// Трек уже скачан: готовый файл (не `.part`) с ненулевым размером.
const AUDIO_FORMATS: &[&str] = &["flac", "mp3", "m4a", "aac"];

fn download_location(dir: &std::path::Path, t: &Track, names: &DownloadNames<'_>, formats: &[&str],
    mut verify: impl FnMut(&mut std::fs::File) -> std::io::Result<bool>) -> std::io::Result<(String, Option<std::path::PathBuf>)> {
    use std::io::ErrorKind;
    let legacy = legacy_stem(t);
    let legacy_key = legacy.to_lowercase();
    if names.legacy.get(&legacy_key) == Some(&Some(t.id.as_str())) {
        for ext in formats {
            let path = dir.join(format!("{legacy}.{ext}"));
            if let Ok(mut file) = open_download(&path) {
                if file.metadata()?.len() > 0 && verify(&mut file)? { return Ok((legacy, Some(path))); }
            }
        }
    }
    let base = track_file_stem(t);
    let id = base.rsplit_once(" [").expect("имя содержит ID").1.trim_end_matches(']');
    let mut free = None;
    let index = names.numbered.get_or_init(|| numbered_downloads(dir)).as_ref()
        .map_err(|e| std::io::Error::new(e.kind(), e.to_string()))?;
    let existing = index.get(id);
    let next = (1u64..).find(|n| existing.is_none_or(|set| !set.contains(n))).unwrap();
    let candidates = std::iter::once(0).chain(existing.into_iter().flatten().copied());
    for n in candidates.chain(std::iter::once(next)) {
        let stem = if n == 0 { base.clone() } else { format!("yamusic-{id}-{n}") };
        let reserved = names.legacy.contains_key(&stem.to_lowercase());
        let mut occupied = false;
        for ext in formats {
            let path = dir.join(format!("{stem}.{ext}"));
            match open_download(&path) {
                Ok(mut file) => {
                    if file.metadata()?.len() > 0 && verify(&mut file)? { return Ok((stem, Some(path))); }
                    occupied = true;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) if e.kind() == ErrorKind::InvalidInput => occupied = true,
                Err(e) => return Err(e),
            }
        }
        if !occupied && !reserved {
            free.get_or_insert(stem);
        }
    }
    free.map(|stem| (stem, None)).ok_or_else(|| ErrorKind::AlreadyExists.into())
}

fn confirmed_legacy(file: &mut std::fs::File, id: &str, cache: &std::path::Path, stop: &AtomicBool, verify: impl FnOnce(&mut std::fs::File) -> std::io::Result<bool>) -> std::io::Result<bool> {
    use sha2::{Digest, Sha256};
    if crate::api::saved_payload(file).is_err() { return Ok(false); }
    let mut hash = Sha256::new();
    std::io::copy(&mut crate::api::cancellable_reader(&mut *file, stop), &mut hash)?;
    let receipt = cache.join(format!("{:x}-{:x}", Sha256::digest(id.as_bytes()), hash.finalize()));
    if std::fs::symlink_metadata(&receipt).is_ok_and(|m| m.is_file()) { return Ok(true); }
    if !verify(file)? { return Ok(false); }
    if std::fs::create_dir_all(cache).is_ok() {
        let _ = std::fs::File::create_new(receipt);
    }
    Ok(true)
}

fn open_download(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::io::{Error, ErrorKind};
    let invalid = || Error::new(ErrorKind::InvalidInput, "Ожидался обычный файл");
    if !std::fs::symlink_metadata(path)?.is_file() { return Err(invalid()); }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(target_os = "linux")] {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    }
    #[cfg(windows)] {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() { return Err(invalid()); }
    Ok(file)
}

/// Каждая загрузка готовит собственный файл и публикует его после записи тегов.
fn save_track(api: &Api, dir: &std::path::Path, t: &Track, names: &DownloadNames<'_>, flac: bool, stop: &AtomicBool) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let receipts = crate::config::dir().join("legacy-downloads");
    let locate = |formats: &[&str]| download_location(dir, t, names, formats, |file| {
        confirmed_legacy(file, &t.id, &receipts, stop, |file| {
            crate::api::check_download(stop)?;
            let (url, _) = api.stream_url(&t.id).map_err(std::io::Error::other)?;
            api.matches_saved_audio(&url, file, stop)
        })
    }).map_err(|e| e.to_string());
    let (mut stem, existing) = locate(AUDIO_FORMATS)?;
    crate::api::check_download(stop).map_err(|e| e.to_string())?;
    let mut upgrade_url = None;
    if let Some(path) = existing {
        if !flac || path.extension().is_some_and(|ext| ext == "flac") { return Ok(path); }
        // Готовый MP3 подходит как запасной вариант, но не заменяет запрошенный FLAC.
        let url = match api.flac_url(&t.id) {
            Ok(Some(url)) => url,
            Ok(None) => return Ok(path),
            Err(e) => { log(format!("flac {}: {e}", t.id)); return Ok(path); }
        };
        if path.extension().is_some_and(|ext| ext == "m4a") {
            let mut file = open_download(&path).map_err(|e| e.to_string())?;
            if api.matches_saved_audio(&url, &mut file, stop).map_err(|e| e.to_string())? { return Ok(path); }
        }
        let (available, lossless) = locate(&["flac"])?;
        if let Some(path) = lossless { return Ok(path); }
        stem = available;
        upgrade_url = Some(url);
    }
    let scratch = tempfile::tempdir_in(dir).map_err(|e| e.to_string())?;
    let part = scratch.path().join("audio.part");
    let mut last = String::new();
    for attempt in 0..3 {
        if stop.load(Ordering::Relaxed) { return Err(crate::api::STOPPED.into()); }
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2 * attempt));
        }
        if stop.load(Ordering::Relaxed) { return Err(crate::api::STOPPED.into()); }
        let url = upgrade_url.clone().map_or_else(|| crate::save::download_url(api, t, flac), Ok);
        match url.and_then(|u| api.download_to(&u, &part, stop)) {
            Ok(n) if n > 0 => {
                last.clear();
                break;
            }
            Ok(_) => last = "пустой файл".into(),
            Err(e) => last = e,
        }
    }
    if !last.is_empty() { return Err(last); }
    let mut head = [0u8; 16];
    {
        use std::io::Read as _;
        let mut f = std::fs::File::open(&part).map_err(|e| e.to_string())?;
        let n = f.read(&mut head).map_err(|e| e.to_string())?;
        if n < head.len() {
            return Err("файл слишком короткий".into());
        }
    }
    let mut ext = audio_ext(&head);
    let remux = flac && ext == "m4a";
    let cover = if ext == "mp3" || remux { t.cover_url(400).and_then(|u| api.fetch_bytes(&u).ok()) } else { None };
    let title = if t.version.is_empty() { t.title.clone() } else { format!("{} ({})", t.title, t.version) };
    let tags = crate::tags::Tags { title: &title, artist: &t.artists, album: &t.album_title, year: t.year, track: t.position, cover: cover.as_deref() };
    let completed = if remux {
        let tagged = scratch.path().join("remux.flac");
        match crate::flac::from_mp4(&part, &tagged, crate::tags::flac_blocks(&tags)) {
            Ok(()) => { ext = "flac"; tagged }
            Err(e) => { log(format!("flac {}: {e}", t.id)); part.clone() }
        }
    } else if ext == "mp3" {
        let tag = crate::tags::id3(&tags);
        let tagged = scratch.path().join("tagged.part");
        {
            use std::io::{Read as _, Seek as _, Write as _};
            let mut src = std::fs::File::open(&part).map_err(|e| e.to_string())?;
            let mut probe = vec![0u8; 10];
            let _ = src.read(&mut probe);
            let skip = crate::tags::existing_id3_len(&probe) as u64;
            src.seek(std::io::SeekFrom::Start(skip)).map_err(|e| e.to_string())?;
            let mut out = std::fs::File::create(&tagged).map_err(|e| e.to_string())?;
            out.write_all(&tag).map_err(|e| e.to_string())?;
            std::io::copy(&mut src, &mut out).map_err(|e| e.to_string())?;
            out.sync_all().map_err(|e| e.to_string())?;
        }
        tagged
    } else {
        part.clone()
    };
    let path = dir.join(format!("{stem}.{ext}"));
    let mut file = open_download(&completed).map_err(|e| e.to_string())?;
    let temp = tempfile::TempPath::try_from_path(completed).map_err(|e| e.to_string())?;
    if stop.load(Ordering::Relaxed) { return Err(crate::api::STOPPED.into()); }
    match temp.persist_noclobber(&path) {
        Ok(()) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut winner = open_download(&path).map_err(|e| e.to_string())?;
            let same = crate::api::same_saved_files(&mut file, &mut winner, stop).map_err(|e| e.to_string())?;
            if !same { return Err("Выбранное имя уже занято другим файлом".into()); }
        }
        Err(e) => return Err(e.error.to_string()),
    }
    let _ = confirmed_legacy(&mut file, &t.id, &receipts, stop, |_| Ok(true));
    Ok(path)
}

/// Колбэки новых экранов: поиск, исполнитель, полноэкранный плеер, меню, настройки звука.
fn register_extra(app: &App) {
    app.on_open_fullscreen(|m| with_ctx(|c| c.open_fullscreen(m)));
    app.on_open_track_menu(|x, y| with_ctx(|c| c.open_track_menu(x, y)));
    app.on_open_list_menu(|x, y| with_ctx(|c| c.open_list_menu(x, y)));
    app.on_row_menu(|block, i, x, y| {
        with_ctx(|c| {
            if let Some(t) = c.row_track(&block, i.max(0) as usize) {
                c.open_menu_for(t, x, y, true);
            }
        })
    });
    app.on_list_pin(|| {
        with_ctx(|c| {
            if let Some(t) = c.list_target() {
                c.toggle_pin(t);
            }
        })
    });
    app.on_list_like(|| with_ctx(|c| c.list_like()));
    app.on_list_save_title(|t| with_ctx(|c| c.list_save("name", t.to_string())));
    app.on_list_save_desc(|t| with_ctx(|c| c.list_save("description", t.to_string())));
    app.on_list_change_cover(|| with_ctx(|c| c.list_change_cover()));
    app.on_list_remove_track(|i| with_ctx(|c| c.list_remove_track(i.max(0) as usize)));
    app.on_account_action(|id| with_ctx(|c| c.account_action(&id)));
    app.on_setting(|id| with_ctx(|c| c.setting(&id)));
    app.on_open_grid(|block, title| {
        with_ctx(|c| {
            if matches!(block.as_str(), "label-releases" | "label-artists") {
                let key = {
                    let st = c.st.borrow();
                    let Some(id) = st.grid_key.as_deref().and_then(|key| key.strip_prefix("label:")) else { return };
                    format!("{block}:{id}")
                };
                let label = c.ui().global::<App>().get_grid_title();
                c.st.borrow_mut().grid_titles.insert(key.clone(), format!("{title} {label}"));
                c.navigate(Page::Grid, Some(key), true);
                return;
            }
            // Полки исполнителя запоминают его id: страница должна открываться и из истории.
            let key = match c.current_artist() {
                Some((id, _)) if block.starts_with("ar-") => format!("{block}:{id}"),
                _ => block.to_string(),
            };
            c.st.borrow_mut().grid_titles.insert(key.clone(), title.to_string());
            c.navigate(Page::Grid, Some(key), true);
        })
    });
    app.on_share_label(|| {
        with_ctx(|c| {
            let url = {
                let st = c.st.borrow();
                let Some(id) = st.grid_key.as_deref().and_then(|key| key.strip_prefix("label:")) else { return };
                format!("https://music.yandex.ru/label/{id}")
            };
            sys::copy_text(&url);
            c.toast("Ссылка скопирована");
        });
    });
    app.on_open_label_sort(|x, y| {
        with_ctx(|c| {
            let ui = c.ui();
            let app = ui.global::<App>();
            let by_popularity = app.get_grid_label_popular();
            let mut popular = c.menu_entry("label-sort-popular", "По популярности", Image::default(), false);
            popular.checked = by_popularity;
            let mut date = c.menu_entry("label-sort-date", "По дате выхода", Image::default(), false);
            date.checked = !by_popularity;
            c.show_menu(vec![popular, date], x, y, true);
        });
    });
    app.on_select_grid_tab(|i| {
        with_ctx(|c| {
            let tab = i.max(0) as usize;
            if tab >= c.st.borrow().grid.len() { return; }
            c.st.borrow_mut().grid_tab = tab;
            c.show_grid_tab();
        })
    });
    app.on_open_cover(|what| {
        with_ctx(|c| {
            let uri = match what.as_str() {
                "artist" => c.st.borrow().artist_shown.clone().and_then(|d| match &*d {
                    Data::Artist(a) => a.cover.clone(),
                    _ => None,
                }),
                _ => c.list().and_then(|d| match &*d {
                    Data::List(l) => l.covers.first().cloned(),
                    _ => None,
                }),
            };
            let Some(uri) = uri else { return };
            let ui = c.ui();
            let app = ui.global::<App>();
            app.set_cover_view(Image::default());
            app.set_cover_open(true);
            let size = c.size_for(600.0);
            let w = c.ui.clone();
            c.images.get(&crate::api::pic(&uri, size), size, move |img| {
                if let Some(ui) = w.upgrade() {
                    ui.global::<App>().set_cover_view(img.clone());
                }
            });
        })
    });
    app.on_toggle_navbar(|| {
        with_ctx(|c| {
            let ui = c.ui();
            let app = ui.global::<App>();
            let v = !app.get_nav_collapsed();
            app.set_nav_collapsed(v);
            let mut st = c.st.borrow_mut();
            st.cfg.navbar_collapsed = v;
            st.cfg.save();
        })
    });
    app.on_menu_action(|id| with_ctx(|c| c.menu_action(&id)));
    app.on_login_start(|| with_ctx(|c| c.login_start()));
    app.on_list_open_artist(|i| {
        with_ctx(|c| {
            let Some(data) = c.list() else { return };
            let Data::List(l) = &*data else { return };
            let id = l.album.as_ref().and_then(|a| a.artist_list.get(i.max(0) as usize)).map(|a| a.id.clone());
            if let Some(id) = id {
                c.navigate(Page::Artist, Some(format!("artist:{id}")), true);
            }
        })
    });
    app.on_login_open(|| sys::open_url("https://ya.ru/device"));
    app.on_login_copy(|| {
        with_ctx(|c| {
            sys::copy_text(&c.ui().global::<App>().get_login_code());
            c.toast("Код скопирован");
        })
    });
    app.on_menu_playlist(|i| with_ctx(|c| c.menu_playlist(i.max(0) as usize)));
    app.on_jump_queue(|i| with_ctx(|c| c.player.send(Cmd::Jump(i.max(0) as usize))));
    app.on_open_current_album(|| {
        with_ctx(|c| {
            if let Some(a) = c.player.state().track.and_then(|t| t.album_id) {
                c.ui().global::<App>().set_fullscreen(false);
                c.navigate(Page::List, Some(format!("album:{a}")), true);
            }
        })
    });
    app.on_open_current_artist(|i| {
        with_ctx(|c| {
            if let Some(a) = c.player.state().track.and_then(|t| t.artist_list.get(i.max(0) as usize).cloned()) {
                c.ui().global::<App>().set_fullscreen(false);
                c.navigate(Page::Artist, Some(format!("artist:{}", a.id)), true);
            }
        })
    });
    app.on_seek_lyric(|i| {
        with_ctx(|c| {
            let t = c.st.borrow().lyrics.get(i.max(0) as usize).map(|l| l.from);
            if let Some(t) = t.filter(|t| *t >= 0.0) {
                c.player.send(Cmd::Seek(t + 0.01));
                c.st.borrow_mut().lyrics_wheel_timer = None;
                c.ui().global::<App>().set_lyrics_scrolled(false);
                c.update_lyric_current(t + 0.01);
            }
        })
    });
    app.on_lyrics_wheel(|| with_ctx(|c| c.lyrics_wheel()));
    app.on_lyrics_mouse(|| with_ctx(|c| c.lyrics_mouse()));

    app.on_search_edited(|text| {
        with_ctx(|c| {
            let text = text.to_string();
            if text.trim().is_empty() {
                c.st.borrow_mut().search_timer = None;
                c.search_now(String::new());
                return;
            }
            let t = Timer::default();
            t.start(TimerMode::SingleShot, Duration::from_millis(350), move || {
                with_ctx(|c| {
                    if c.ui().global::<App>().get_search_text().as_str() == text {
                        c.search_now(text.clone());
                    }
                });
            });
            c.st.borrow_mut().search_timer = Some(t);
        })
    });
    app.on_open_search_tracks(|| with_ctx(|c| c.navigate(Page::Search, Some(SEARCH_TRACK_KIND.into()), true)));
    app.on_open_suggest(|i| {
        with_ctx(|c| {
            let ui = c.ui();
            let app = ui.global::<App>();
            if let Some(s) = app.get_search_suggest().row_data(i.max(0) as usize) {
                app.set_search_text(s.clone());
                c.search_now(s.to_string());
            }
        })
    });
    app.on_open_card(|block, i| with_ctx(|c| c.open_card(&block, i.max(0) as usize)));
    app.on_play_chart(|i| {
        with_ctx(|c| {
            let tracks = c.st.borrow().chart.clone();
            c.play_tracks(tracks, i.max(0) as usize, ListKind::Playlist, "414787002:1076", "Чарт");
        })
    });
    app.on_like_chart(|i| {
        with_ctx(|c| {
            let Some(t) = c.st.borrow().chart.get(i.max(0) as usize).cloned() else { return };
            c.like(t);
        })
    });
    app.on_play_result(|i| {
        with_ctx(|c| {
            let tracks = c.st.borrow().results.clone();
            c.play_tracks(tracks, i.max(0) as usize, ListKind::Tracks, "search", "Поиск");
        })
    });
    app.on_like_result(|i| {
        with_ctx(|c| {
            let Some(t) = c.st.borrow().results.get(i.max(0) as usize).cloned() else { return };
            c.like(t);
        })
    });

    app.on_artist_play(|| {
        with_ctx(|c| {
            let Some((id, name)) = c.current_artist() else { return };
            if let Some(net) = c.st.borrow().net.clone() {
                net.play_artist(&c.player, &id, &name, None);
            }
            c.start();
        })
    });
    app.on_artist_play_track(|i| {
        with_ctx(|c| {
            let Some((id, name)) = c.current_artist() else { return };
            let tracks = c.st.borrow().artist_tracks.clone();
            c.play_tracks(tracks, i.max(0) as usize, ListKind::Artist, &id, &name);
        })
    });
    app.on_artist_like_track(|i| {
        with_ctx(|c| {
            let Some(t) = c.st.borrow().artist_tracks.get(i.max(0) as usize).cloned() else { return };
            c.like(t);
        })
    });
    app.on_artist_like(|| {
        with_ctx(|c| {
            let Some((id, name)) = c.current_artist() else { return };
            c.toggle_artist_mark(id, name, ArtistMark::Like);
        })
    });
    app.on_artist_pin(|| {
        with_ctx(|c| {
            if let Some((id, _)) = c.current_artist() {
                c.toggle_pin(format!("artist:{id}"));
            }
        })
    });
    app.on_artist_familiar(|| {
        with_ctx(|c| {
            if let Some((id, _)) = c.current_artist() {
                c.navigate(Page::List, Some(format!("familiar:{id}")), true);
            }
        })
    });

    app.on_set_quality(|q| {
        with_ctx(|c| {
            c.st.borrow_mut().cfg.quality = q.clamp(0, 2) as u8;
            c.apply_eq();
        })
    });
    app.on_eq_toggle(|| {
        with_ctx(|c| {
            let on = !c.st.borrow().cfg.eq_on;
            c.st.borrow_mut().cfg.eq_on = on;
            c.apply_eq();
        })
    });
    app.on_eq_set_band(|i, v| {
        with_ctx(|c| {
            let gain = (v * 2.0).round() / 2.0;
            {
                let mut st = c.st.borrow_mut();
                let Some(g) = st.cfg.eq_gains.get_mut(i.max(0) as usize) else { return };
                if *g == gain {
                    return;
                }
                *g = gain;
                st.cfg.eq_preset = 0;
            }
            c.apply_eq();
        })
    });
    app.on_eq_set_preamp(|v| {
        with_ctx(|c| {
            let gain = (v * 20.0).round() / 20.0;
            if c.st.borrow().cfg.eq_preamp == gain {
                return;
            }
            c.st.borrow_mut().cfg.eq_preamp = gain;
            c.apply_eq();
        })
    });
    app.on_eq_select_preset(|i| {
        with_ctx(|c| {
            let i = (i.max(0) as usize).min(crate::eq::PRESETS.len() - 1);
            {
                let mut st = c.st.borrow_mut();
                st.cfg.eq_preset = i;
                if i > 0 {
                    st.cfg.eq_gains = crate::eq::PRESETS[i].1;
                    st.cfg.eq_preamp = 1.0;
                }
            }
            c.apply_eq();
        })
    });
    app.on_open_wave_settings(|| with_ctx(|c| c.open_wave_settings()));
    app.on_tune_wave(|group, option| {
        with_ctx(|c| {
            let (settings, activity) = {
                let mut st = c.st.borrow_mut();
                let Some(entry) = st.wave_tune.get_mut(group.max(0) as usize) else { return };
                let option = (option.max(0) as usize).min(entry.0.options.len().saturating_sub(1));
                let picked = entry.0.options[option].clone();
                // Занятие — отдельная волна: повторное нажатие возвращает к настроенной «Моей волне».
                let activity = if is_activity(&picked.seed) {
                    entry.1 = if entry.1 == option { NOTHING } else { option };
                    (entry.1 == option).then_some(picked)
                } else {
                    entry.1 = option;
                    for (g, chosen, _) in st.wave_tune.iter_mut() {
                        if g.options.iter().any(|o| is_activity(&o.seed)) {
                            *chosen = NOTHING;
                        }
                    }
                    None
                };
                let settings: Vec<String> = st
                    .wave_tune
                    .iter()
                    .filter_map(|(g, chosen, _)| g.options.get(*chosen))
                    .filter(|o| !o.any && !is_activity(&o.seed))
                    .map(|o| o.seed.clone())
                    .collect();
                (settings, activity)
            };
            c.show_wave_settings();
            c.st.borrow_mut().trailer = None;
            match activity {
                Some(a) => c.player.send(Cmd::Wave(Source::Seed { station: a.seed, header: "Моя волна".into(), title: a.name })),
                None => {
                    c.st.borrow_mut().tune_seeds = settings.clone();
                    c.player.send(Cmd::TuneWave(settings));
                }
            }
            c.start();
        })
    });
    app.on_reset_wave(|| {
        with_ctx(|c| {
            c.st.borrow_mut().trailer = None;
            c.st.borrow_mut().tune_seeds.clear();
            c.player.send(Cmd::Wave(Source::Wave));
            c.start();
        })
    });
    app.on_row_open_album(|block, i| {
        with_ctx(|c| {
            let Some(t) = c.row_track(&block, i.max(0) as usize) else { return };
            if let Some(a) = t.album_id {
                c.navigate(Page::List, Some(format!("album:{a}")), true);
            }
        })
    });
    app.on_row_open_artist(|block, i, j| {
        with_ctx(|c| {
            let Some(t) = c.row_track(&block, i.max(0) as usize) else { return };
            if let Some(a) = t.artist_list.get(j.max(0) as usize) {
                c.navigate(Page::Artist, Some(format!("artist:{}", a.id)), true);
            }
        })
    });
    app.on_list_trailer(|| {
        with_ctx(|c| {
            let Some(data) = c.list() else { return };
            let Data::List(l) = &*data else { return };
            let path = match (&l.playlist, &l.album) {
                (Some(p), _) => format!("users/{}/playlists/{}", p.uid, p.kind),
                (_, Some(a)) => format!("albums/{}", a.id),
                _ => return,
            };
            c.play_trailer(path);
        })
    });
    app.on_artist_trailer(|| {
        with_ctx(|c| {
            if let Some((id, _)) = c.current_artist() {
                c.play_trailer(format!("artists/{id}"));
            }
        })
    });
    app.on_list_download(|| {
        with_ctx(|c| {
            let Some(data) = c.list() else { return };
            let Data::List(l) = &*data else { return };
            c.download_tracks(l.title.clone(), l.tracks.clone());
        })
    });
    app.set_eq_presets(ModelRc::from(Rc::new(VecModel::from(
        crate::eq::PRESETS.iter().map(|p| SharedString::from(p.0)).collect::<Vec<_>>(),
    ))));
    with_ctx(|c| c.apply_eq());
}

#[cfg(test)]
#[test]
fn downloads_with_same_title_keep_both_recordings() {
    let first = crate::api::parse_track(&serde_json::json!({"id":1,"title":"Трек"})).unwrap();
    let mut second = first.clone();
    second.id = "2".into();
    assert_ne!(track_file_stem(&first), track_file_stem(&second));
    second.title = "я".repeat(140);
    assert!(format!("{}.tagged.part", track_file_stem(&second)).len() <= 255);
}

#[cfg(test)]
#[test]
fn legacy_download_rejects_names_shared_by_different_ids() {
    let mut t = crate::api::parse_track(&serde_json::json!({"id":12,"title":"Песня","artists":[{"id":1,"name":"Группа"}]})).unwrap();
    let mut tracks = vec![t.clone(), t.clone()];
    assert_eq!(legacy_names(&tracks).legacy["группа — песня"], Some("12"));
    t.id = "13".into();
    tracks.push(t);
    assert_eq!(legacy_names(&tracks).legacy["группа — песня"], None);
}

#[cfg(test)]
#[test]
fn legacy_receipt_is_scoped_to_track_and_complete_payload() {
    use std::io::{Seek, Write};
    let cache = tempfile::tempdir().unwrap();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"fLaC-long-enough-for-a-header").unwrap();
    assert!(confirmed_legacy(&mut file, "12", cache.path(), &AtomicBool::new(false), |_| Ok(true)).unwrap());
    assert!(confirmed_legacy(&mut file, "12", cache.path(), &AtomicBool::new(false), |_| panic!("unnecessary network request")).unwrap());
    assert!(!confirmed_legacy(&mut file, "13", cache.path(), &AtomicBool::new(false), |_| Ok(false)).unwrap());
    file.seek(std::io::SeekFrom::End(-1)).unwrap();
    file.write_all(b"x").unwrap();
    assert!(!confirmed_legacy(&mut file, "12", cache.path(), &AtomicBool::new(false), |_| Ok(false)).unwrap());
}

#[cfg(test)]
#[test]
fn colliding_legacy_name_does_not_occupy_another_tracks_id() {
    use std::io::{Read, Seek};
    let first = crate::api::parse_track(&serde_json::json!({"id":12,"title":"Песня","artists":[{"id":1,"name":"Группа"}]})).unwrap();
    let mut second = first.clone();
    second.id = "13".into();
    second.title += " [12]";
    let tracks = [first, second];
    let names = legacy_names(&tracks);
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("Группа — Песня [12].mp3");
    std::fs::write(&path, b"ID3 second track 13").unwrap();
    let (stem, existing) = download_location(folder.path(), &tracks[0], &names, AUDIO_FORMATS, |_| Ok(false)).unwrap();
    assert!(existing.is_none());
    assert_ne!(stem, track_file_stem(&tracks[0]));
    let (_, existing) = download_location(folder.path(), &tracks[1], &names, AUDIO_FORMATS, |file| {
        file.rewind().unwrap();
        let mut data = Vec::new();
        file.read_to_end(&mut data).unwrap();
        Ok(data == b"ID3 second track 13")
    }).unwrap();
    assert_eq!(existing, Some(path));
}

#[cfg(test)]
#[test]
fn reuse_numbered_download_after_earlier_collision_disappears() {
    let t = crate::api::parse_track(&serde_json::json!({"id": 12, "title": "Song"})).unwrap();
    let folder = tempfile::tempdir().unwrap();
    let names = legacy_names(std::slice::from_ref(&t));
    let expected = folder.path().join("yamusic-12-2.mp3");
    std::fs::write(&expected, b"confirmed existing audio").unwrap();
    let (_, existing) = download_location(folder.path(), &t, &names, AUDIO_FORMATS, |_| Ok(true)).unwrap();
    assert_eq!(existing, Some(expected));
}


#[cfg(test)]
#[test]
fn failed_legacy_verification_aborts_name_selection() {
    let t = crate::api::parse_track(&serde_json::json!({"id":12, "title":"Song"})).unwrap();
    let folder = tempfile::tempdir().unwrap();
    let names = legacy_names(std::slice::from_ref(&t));
    std::fs::write(folder.path().join(format!("{}.mp3", legacy_stem(&t))), b"previous download").unwrap();
    let error = download_location(folder.path(), &t, &names, AUDIO_FORMATS, |_| Err(std::io::ErrorKind::TimedOut.into())).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[cfg(test)]
#[test]
fn lossless_download_keeps_the_existing_mp3_and_reuses_the_flac() {
    let t = crate::api::parse_track(&serde_json::json!({"id": 7, "title": "Song"})).unwrap();
    let folder = tempfile::tempdir().unwrap();
    let names = legacy_names(std::slice::from_ref(&t));
    let stem = track_file_stem(&t);
    let mp3 = folder.path().join(format!("{stem}.mp3"));
    std::fs::write(&mp3, b"saved mp3").unwrap();
    let locate = |formats: &[&str]| download_location(folder.path(), &t, &names, formats, |_| Ok(true)).unwrap();
    assert_eq!(locate(AUDIO_FORMATS).1.as_ref(), Some(&mp3));
    assert_eq!(locate(&["flac"]), (stem.clone(), None));
    let flac = folder.path().join(format!("{stem}.flac"));
    std::fs::write(&flac, b"saved flac").unwrap();
    assert_eq!(locate(AUDIO_FORMATS).1, Some(flac));
    assert!(mp3.is_file());
}

#[cfg(test)]
#[test]
fn download_cancellation_belongs_to_the_selected_list() {
    let job = ListDownload { uid: "10".into(), list_key: "playlist:10:7".into(), stop: Arc::default() };
    // У плейлистов разных владельцев могут совпадать и название, и номер.
    assert!(job.stop_for("10", Some("playlist:20:7")).is_none());
    assert!(job.stop_for("10", Some("album:7")).is_none());
    assert!(job.stop_for("10", None).is_none());
    job.stop_for("10", Some("playlist:10:7")).unwrap().store(true, Ordering::Relaxed);
    assert!(job.stop.load(Ordering::Relaxed));

    // У «Мне нравится» ключ страницы одинаков для всех аккаунтов.
    let likes = ListDownload { list_key: ListDownload::key("10", "likes"), ..job };
    assert!(likes.stop_for("10", Some("playlist:10:3")).is_some());
    assert!(likes.stop_for("20", Some("likes")).is_none());
    assert!(likes.stop_for("10", Some("likes")).is_some());
}
