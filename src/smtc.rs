//! Системное управление воспроизведением привязано к окну трея.
use crate::player::Shared;
use crate::tray::Event;
use windows::core::{factory, Result};
use windows::Foundation::TypedEventHandler;
use windows::Media::{
    MediaPlaybackStatus as Playback, MediaPlaybackType, SystemMediaTransportControls as Controls,
    SystemMediaTransportControlsButton as Button,
    SystemMediaTransportControlsButtonPressedEventArgs as Pressed,
};
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::WinRT::{
    ISystemMediaTransportControlsInterop, RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
    RO_INIT_SINGLETHREADED,
};

use std::path::Path;
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Instant;
use windows::core::{Interface, GUID, HSTRING, PCWSTR};
use windows::Foundation::{TimeSpan, Uri};
use windows::Media::{PlaybackPositionChangeRequestedEventArgs, SystemMediaTransportControlsTimelineProperties};
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

const AUMID: &str = "SpolzerMod.YaMusicRS";

/// `System.AppUserModel.ID` (`{9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}`, pid 5).
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
    pid: 5,
};

/// Задаёт AppUserModelID процесса, чтобы Windows 11 показывала имя и значок приложения
/// в карточке системного плеера вместо «Unknown app». Вызывать в начале `main()` до создания окон.
pub fn register_app_id() {
    let aumid = crate::sys::wide(AUMID);
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(aumid.as_ptr());
    }
}

/// Создаёт или обновляет ярлык в меню «Пуск» с нашим `AppUserModelID`: по нему Windows 11
/// подставляет имя и значок приложения в карточку системного плеера. Если ярлык уже указывает
/// на этот же exe с нужным `AppUserModelID`, файл не трогаем.
fn ensure_start_menu_shortcut() -> windows::core::Result<()> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(());
    };
    let programs = crate::sys::config_base().join(r"Microsoft\Windows\Start Menu\Programs");
    if !programs.is_dir() {
        return Ok(());
    }
    let lnk = programs.join(format!("{}.lnk", env!("APP_NAME")));
    if lnk.exists() && shortcut_matches(&lnk, &exe) {
        return Ok(());
    }
    write_shortcut(&lnk, &exe)
}

fn shortcut_matches(lnk: &Path, exe: &Path) -> bool {
    let lnk_w = HSTRING::from(lnk.as_os_str());
    unsafe {
        let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER)
        else {
            return false;
        };
        let Ok(file) = link.cast::<IPersistFile>() else {
            return false;
        };
        if file.Load(PCWSTR(lnk_w.as_ptr()), STGM_READ).is_err() {
            return false;
        }
        let mut buf = [0u16; 1024];
        if link.GetPath(&mut buf, null_mut(), 0).is_err() {
            return false;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let target = String::from_utf16_lossy(&buf[..n]);
        if target.to_lowercase() != exe.to_string_lossy().to_lowercase() {
            return false;
        }
        let Ok(store) = link.cast::<IPropertyStore>() else {
            return false;
        };
        let Ok(val) = store.GetValue(&PKEY_APP_USER_MODEL_ID) else {
            return false;
        };
        val.to_string() == AUMID
    }
}

fn write_shortcut(lnk: &Path, exe: &Path) -> windows::core::Result<()> {
    let exe_w = HSTRING::from(exe.as_os_str());
    let lnk_w = HSTRING::from(lnk.as_os_str());
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        link.SetPath(PCWSTR(exe_w.as_ptr()))?;
        link.SetIconLocation(PCWSTR(exe_w.as_ptr()), 0)?;
        let store: IPropertyStore = link.cast()?;
        let val = PROPVARIANT::from(AUMID);
        store.SetValue(&PKEY_APP_USER_MODEL_ID, &val)?;
        store.Commit()?;
        let file: IPersistFile = link.cast()?;
        file.Save(PCWSTR(lnk_w.as_ptr()), true)?;
    }
    Ok(())
}

fn update_timeline(smtc: &Controls, position: f32, duration: f32) {
    let Ok(tl) = SystemMediaTransportControlsTimelineProperties::new() else { return };
    let end = ticks(duration);
    let pos = ticks(position.clamp(0.0, duration));
    let _ = tl.SetStartTime(ticks(0.0));
    let _ = tl.SetMinSeekTime(ticks(0.0));
    let _ = tl.SetEndTime(end);
    let _ = tl.SetMaxSeekTime(end);
    let _ = tl.SetPosition(pos);
    let _ = smtc.UpdateTimelineProperties(&tl);
}

fn ticks(secs: f32) -> TimeSpan {
    TimeSpan { Duration: (secs.max(0.0) as f64 * 10_000_000.0) as i64 }
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        unsafe {
            match RoInitialize(RO_INIT_SINGLETHREADED) {
                Err(e) if e.code() == RPC_E_CHANGED_MODE => RoInitialize(RO_INIT_MULTITHREADED)?,
                result => result?,
            }
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

pub(crate) struct Session {
    controls: Controls,
    token: i64,
    seek_token: i64,
    timeline: Option<(Instant, f32, f32)>,
    track_id: Option<String>,
    status: Playback,
    text: Option<(String, String, String, String)>,
    has_track: bool,
    _apartment: Apartment,
}

impl Session {
    pub(crate) fn new(hwnd: HWND, on_button: impl Fn(Event) + Send + Sync + 'static) -> Result<Self> {
        let apartment = Apartment::new()?;
        if let Err(e) = ensure_start_menu_shortcut() {
            crate::config::log(format!("SMTC: ярлык меню Пуск: {e}"));
        }
        let factory = factory::<Controls, ISystemMediaTransportControlsInterop>()?;
        let controls: Controls = unsafe { factory.GetForWindow(hwnd)? };
        controls.SetIsEnabled(false)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsPreviousEnabled(false)?;
        controls.SetIsNextEnabled(false)?;
        controls.SetIsStopEnabled(true)?;
        let on_seek = Arc::new(on_button);
        let on_button = on_seek.clone();
        let token = controls.ButtonPressed(&TypedEventHandler::<Controls, Pressed>::new(
            move |_, args| {
                let event = match args.ok()?.Button()? {
                    Button::Play => Event::Play,
                    Button::Pause | Button::Stop => Event::Pause,
                    Button::Next => Event::Next,
                    Button::Previous => Event::Prev,
                    _ => return Ok(()),
                };
                on_button(event);
                Ok(())
            },
        ))?;
        let seek_token = controls.PlaybackPositionChangeRequested(&TypedEventHandler::<Controls, PlaybackPositionChangeRequestedEventArgs>::new(move |_, args| {
            let ms = args.ok()?.RequestedPlaybackPosition()?.Duration / 10_000;
            on_seek(Event::Seek(ms.max(0)));
            Ok(())
        }))?;
        Ok(Self {
            controls,
            seek_token,
            timeline: None,
            track_id: None,
            token,
            status: Playback::Closed,
            text: None,
            has_track: false,
            _apartment: apartment,
        })
    }

    pub(crate) fn update(&mut self, state: &Shared) -> Result<()> {
        let track = state.track.as_ref();
        let (title, version, artists, album) =
            track.map_or(("Моя волна", "", "", ""), |t| {
                (
                    t.title.as_str(),
                    t.version.as_str(),
                    t.artists.as_str(),
                    t.album_title.as_str(),
                )
            });
        let id = track.map(|t| t.id.as_str());
        let track_changed = self.track_id.as_deref() != id;
        if track_changed || self
            .text
            .as_ref()
            .is_none_or(|(t, v, a, l)| t != title || v != version || a != artists || l != album)
        {
            let updater = self.controls.DisplayUpdater()?;
            updater.ClearAll()?;
            updater.SetType(MediaPlaybackType::Music)?;
            let music = updater.MusicProperties()?;
            let display_title = if version.is_empty() {
                title.into()
            } else {
                format!("{title} ({version})").into()
            };
            music.SetTitle(&display_title)?;
            music.SetArtist(&artists.into())?;
            music.SetAlbumTitle(&album.into())?;
            if let Some(url) = track.and_then(|t| t.cover_url(400)) {
                if let Ok(uri) = Uri::CreateUri(&HSTRING::from(url)) {
                    if let Ok(image) = RandomAccessStreamReference::CreateFromUri(&uri) {
                        let _ = updater.SetThumbnail(&image);
                    }
                }
            }
            updater.Update()?;
            self.track_id = id.map(str::to_owned);
            self.text = Some((title.into(), version.into(), artists.into(), album.into()));
        }
        if self.has_track != track.is_some() {
            self.controls.SetIsPreviousEnabled(track.is_some())?;
            self.controls.SetIsNextEnabled(track.is_some())?;
            self.has_track = track.is_some();
        }
        let status = if state.playing {
            Playback::Playing
        } else if state.loading && !state.paused {
            Playback::Changing
        } else {
            Playback::Paused
        };
        if track.is_some() && state.duration > 0.0 {
            let jumped = self.timeline.is_none_or(|(at, pos, dur)| {
                let expected = pos + if state.playing { at.elapsed().as_secs_f32() } else { 0.0 };
                (expected - state.position).abs() > 1.5 || (dur - state.duration).abs() > 0.5
            });
            if track_changed || self.status != status || jumped {
                update_timeline(&self.controls, state.position, state.duration);
                self.timeline = Some((Instant::now(), state.position, state.duration));
            }
        } else { self.timeline = None; }
        if self.status != status {
            self.controls.SetPlaybackStatus(status)?;
            if self.status == Playback::Closed {
                self.controls.SetIsEnabled(true)?;
            }
            self.status = status;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.controls.RemovePlaybackPositionChangeRequested(self.seek_token);
        let _ = self.controls.RemoveButtonPressed(self.token);
        let _ = self.controls.SetIsEnabled(false);
        let _ = self.controls.SetPlaybackStatus(Playback::Closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::Status;
    use std::{
        ptr::null_mut,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Reported,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    #[test]
    #[ignore = "requires a Windows desktop with a media session broker"]
    fn windows_broker_reads_metadata_and_routes_buttons() {
        let title = format!("yamusic-rs SMTC test {}", std::process::id());
        let class = crate::sys::wide("STATIC");
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert!(!hwnd.is_null());
        let (tx, buttons) = mpsc::channel();
        let (observed, rx) = mpsc::channel();
        let (clear, clear_requests) = mpsc::channel();
        let mut session = Session::new(HWND(hwnd), move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        let mut state = Shared::default();
        state.status = Status::Ready;
        state.paused = true;
        state.track = crate::api::parse_track(&serde_json::json!({
            "id":"7", "title": title, "version":"Live", "artists":[{"id":"8","name":"SMTC artist"}]
        }));
        session.update(&state).unwrap();

        let selected = state.track.clone();
        let reader = thread::spawn(move || {
            let _apartment = Apartment::new().unwrap();
            let manager = Manager::RequestAsync().unwrap().join().unwrap();
            let until = Instant::now() + Duration::from_secs(8);
            loop {
                let sessions = manager.GetSessions().unwrap();
                for index in 0..sessions.Size().unwrap() {
                    let remote = sessions.GetAt(index).unwrap();
                    let Ok(info) = remote.TryGetMediaPropertiesAsync().unwrap().join() else {
                        continue;
                    };
                    if info.Title().unwrap() != format!("{title} (Live)").as_str() {
                        continue;
                    }
                    assert_eq!(info.Artist().unwrap(), "SMTC artist");
                    // Проверяем только эту сессию, в том числе до первого трека.
                    clear.send(()).unwrap();
                    let until_clear = Instant::now() + Duration::from_secs(3);
                    while remote
                        .TryGetMediaPropertiesAsync()
                        .unwrap()
                        .join()
                        .unwrap()
                        .Title()
                        .unwrap()
                        != "Моя волна"
                    {
                        assert!(Instant::now() < until_clear, "old metadata still visible");
                        thread::sleep(Duration::from_millis(10));
                    }
                    // Оболочка обновляет метаданные и доступные команды независимо.
                    loop {
                        let controls = remote.GetPlaybackInfo().unwrap().Controls().unwrap();
                        if controls.IsPlayEnabled().unwrap() && !controls.IsNextEnabled().unwrap() {
                            break;
                        }
                        assert!(Instant::now() < until_clear, "old controls still visible");
                        thread::sleep(Duration::from_millis(10));
                    }
                    for event in [Event::Play, Event::Pause, Event::Next, Event::Prev] {
                        eprintln!("Requesting {event:?}");
                        let command = match event {
                            Event::Play => remote.TryPlayAsync(),
                            Event::Pause => remote.TryPauseAsync(),
                            Event::Next => remote.TrySkipNextAsync(),
                            Event::Prev => remote.TrySkipPreviousAsync(),
                            _ => unreachable!(),
                        };
                        assert!(command.unwrap().join().unwrap(), "command rejected");
                        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), event);
                        let wanted = if event == Event::Play {
                            Reported::Playing
                        } else {
                            Reported::Paused
                        };
                        let deadline = Instant::now() + Duration::from_secs(3);
                        while remote.GetPlaybackInfo().unwrap().PlaybackStatus().unwrap() != wanted
                        {
                            assert!(Instant::now() < deadline, "playback state not published");
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    return;
                }
                assert!(
                    Instant::now() < until,
                    "session missing from Windows media broker"
                );
                thread::sleep(Duration::from_millis(50));
            }
        });
        let deadline = Instant::now() + Duration::from_secs(25);
        while !reader.is_finished() {
            assert!(Instant::now() < deadline, "Windows media broker timed out");
            let mut message = unsafe { std::mem::zeroed() };
            unsafe {
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if clear_requests.try_recv().is_ok() {
                state.track = None;
                session.update(&state).unwrap();
            }
            while let Ok(event) = buttons.try_recv() {
                eprintln!("Received {event:?}");
                match event {
                    Event::Play => {
                        state.track = selected.clone();
                        state.playing = true;
                        state.paused = false;
                    }
                    Event::Pause => {
                        state.playing = false;
                        state.paused = true;
                    }
                    _ => {}
                }
                session.update(&state).unwrap();
                observed.send(event).unwrap();
            }
            thread::sleep(Duration::from_millis(5));
        }
        reader.join().unwrap();
        let controls = session.controls.clone();
        drop(session);
        assert!(!controls.IsEnabled().unwrap());
        assert_eq!(controls.PlaybackStatus().unwrap(), Playback::Closed);
        drop(controls);
        unsafe {
            DestroyWindow(hwnd);
        }
    }
}
