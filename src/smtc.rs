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
    status: Playback,
    text: Option<(String, String, String, String)>,
    has_track: bool,
    _apartment: Apartment,
}

impl Session {
    pub(crate) fn new(hwnd: HWND, on_button: impl Fn(Event) + Send + 'static) -> Result<Self> {
        let apartment = Apartment::new()?;
        let factory = factory::<Controls, ISystemMediaTransportControlsInterop>()?;
        let controls: Controls = unsafe { factory.GetForWindow(hwnd)? };
        controls.SetIsEnabled(false)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsPreviousEnabled(false)?;
        controls.SetIsNextEnabled(false)?;
        let token = controls.ButtonPressed(&TypedEventHandler::<Controls, Pressed>::new(
            move |_, args| {
                let event = match args.ok()?.Button()? {
                    Button::Play => Event::Play,
                    Button::Pause => Event::Pause,
                    Button::Next => Event::Next,
                    Button::Previous => Event::Prev,
                    _ => return Ok(()),
                };
                on_button(event);
                Ok(())
            },
        ))?;
        Ok(Self {
            controls,
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
        if self
            .text
            .as_ref()
            .is_none_or(|(t, v, a, l)| t != title || v != version || a != artists || l != album)
        {
            let updater = self.controls.DisplayUpdater()?;
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
            updater.Update()?;
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
        let mut state = Shared {
            status: Status::Ready,
            paused: true,
            ..Default::default()
        };
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
