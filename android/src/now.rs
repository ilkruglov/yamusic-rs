//! Что играет: картинки трека, текст песни, исполнители, похожие треки и плейлисты для плеера.
//! Качается в фоне при смене трека, состояние живёт в UI-потоке.

use crate::api::{self, Api, Playlist, SyncLine, Track};
use crate::config::log;
use crate::images::Images;
use crate::{App, ArtistCard, Now, Row};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

struct State {
    ui: slint::Weak<App>,
    api: Arc<Api>,
    images: Images,
    uid: String,
    track: Option<Track>,
    synced: Vec<SyncLine>,
    artists: Vec<String>,
    liked_artists: HashSet<String>,
    similar: Vec<Track>,
    playlists: Vec<Playlist>,
}

thread_local! {
    static NOW: RefCell<Option<State>> = const { RefCell::new(None) };
    // id трека отдельно от NOW: картинка из кэша приходит прямо внутри with().
    static PLAYING: RefCell<String> = const { RefCell::new(String::new()) };
}

#[derive(Clone, Copy)]
enum Pic {
    Cover,
    CoverBig,
    Avatar,
    Cutout,
}

fn set_pic(now: &Now, pic: Pic, img: Image) {
    match pic {
        Pic::Cover => now.set_cover(img),
        Pic::CoverBig => now.set_cover_big(img),
        Pic::Avatar => now.set_avatar(img),
        Pic::Cutout => now.set_cutout(fade_bottom(&img)),
    }
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    NOW.with(|n| n.borrow_mut().as_mut().map(f))
}

/// Ответ из фонового потока: применить, только если трек всё ещё тот же.
fn deliver<T: Send + 'static>(id: String, res: api::Res<T>, what: &'static str, f: impl FnOnce(&mut State, &App, T) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with(|s| match res {
            Ok(v) if s.track.as_ref().is_some_and(|t| t.id == id) => {
                if let Some(ui) = s.ui.upgrade() {
                    f(s, &ui, v);
                }
            }
            Ok(_) => {}
            Err(e) => log(format!("{what}: {e}")),
        });
    });
}

pub fn start(ui: &App, api: Arc<Api>, images: Images) {
    NOW.with(|n| {
        *n.borrow_mut() = Some(State {
            ui: ui.as_weak(),
            api,
            images,
            uid: String::new(),
            track: None,
            synced: Vec::new(),
            artists: Vec::new(),
            liked_artists: HashSet::new(),
            similar: Vec::new(),
            playlists: Vec::new(),
        })
    });
}

/// Плеер узнал аккаунт: подтянуть плейлисты и понравившихся исполнителей.
pub fn set_uid(uid: &str) {
    with(|s| {
        if s.uid == uid || uid.is_empty() {
            return;
        }
        s.uid = uid.to_string();
        load_playlists(s);
        let (api, uid) = (s.api.clone(), uid.to_string());
        std::thread::spawn(move || {
            let liked = api.liked_artists(&uid);
            let _ = slint::invoke_from_event_loop(move || {
                with(|s| match liked {
                    Ok(a) => {
                        s.liked_artists = a.into_iter().map(|a| a.id).collect();
                        refresh_liked(s);
                    }
                    Err(e) => log(format!("исполнители в коллекции: {e}")),
                });
            });
        });
    });
}

fn load_playlists(s: &State) {
    let (api, uid) = (s.api.clone(), s.uid.clone());
    std::thread::spawn(move || {
        let res = api.playlists(&uid);
        let _ = slint::invoke_from_event_loop(move || {
            with(|s| match res {
                Ok(list) => s.set_playlists(list),
                Err(e) => log(format!("плейлисты: {e}")),
            });
        });
    });
}

/// Заиграл другой трек: всё про старый убрать, про новый — запросить.
pub fn track_changed(track: Option<&Track>) {
    with(|s| {
        if s.track.as_ref().map(|t| &t.id) == track.map(|t| &t.id) {
            return;
        }
        s.track = track.cloned();
        PLAYING.with(|p| *p.borrow_mut() = track.map(|t| t.id.clone()).unwrap_or_default());
        s.synced.clear();
        s.artists.clear();
        s.similar.clear();
        let Some(ui) = s.ui.upgrade() else { return };
        let now = ui.global::<Now>();
        let Some(t) = track else {
            now.set_has_track(false);
            return;
        };
        now.set_has_track(true);
        now.set_title(t.title.clone().into());
        now.set_version(t.version.clone().into());
        now.set_artists(t.artists.clone().into());
        now.set_artist(t.artist_list.first().map(|a| a.name.clone()).unwrap_or_default().into());
        now.set_explicit(t.explicit);
        now.set_bg(t.color.map_or(slint::Color::from_rgb_u8(0x1f, 0x1f, 0x1f), |[r, g, b]| slint::Color::from_rgb_u8(r, g, b)));
        now.set_duration_text(time(t.duration_ms as f32 / 1000.0).into());
        now.set_has_lyrics(t.lyrics);
        now.set_lyrics(ModelRc::default());
        now.set_lyric(-1);
        now.set_artist_cards(ModelRc::default());
        now.set_similar(ModelRc::default());
        for pic in [Pic::Cover, Pic::CoverBig, Pic::Avatar, Pic::Cutout] {
            set_pic(&now, pic, Image::default());
        }

        let px = |dp: f32| (dp * ui.window().scale_factor()).round() as u32;
        let image = |s: &State, uri: Option<&String>, size: u32, pic: Pic| {
            let Some(uri) = uri else { return };
            let (weak, id) = (s.ui.clone(), t.id.clone());
            s.images.get(&api::pic(uri, size), size, move |img| {
                let Some(ui) = weak.upgrade() else { return };
                if PLAYING.with(|p| *p.borrow() == id) {
                    set_pic(&ui.global::<Now>(), pic, img.clone());
                }
            });
        };
        let first = t.artist_list.first();
        image(s, t.cover.as_ref(), px(96.0), Pic::Cover);
        image(s, t.cover.as_ref(), px(355.0), Pic::CoverBig);
        image(s, first.and_then(|a| a.cover.as_ref()), px(52.0), Pic::Avatar);
        image(s, first.and_then(|a| a.cutout.as_ref()), px(355.0), Pic::Cutout);

        if t.lyrics {
            let (api, id) = (s.api.clone(), t.id.clone());
            std::thread::spawn(move || {
                let res = api.lyrics(&id);
                deliver(id, res, "текст песни", |s, ui, l| {
                    let lines: Vec<slint::SharedString> = if l.synced.is_empty() {
                        l.text.iter().map(|x| x.as_str().into()).collect()
                    } else {
                        l.synced.iter().map(|x| x.text.as_str().into()).collect()
                    };
                    s.synced = l.synced;
                    let now = ui.global::<Now>();
                    now.set_has_lyrics(!lines.is_empty());
                    now.set_lyrics(ModelRc::new(VecModel::from(lines)));
                });
            });
        }

        let artists: Vec<String> = t.artist_list.iter().map(|a| a.id.clone()).filter(|id| !id.is_empty()).take(4).collect();
        if !artists.is_empty() {
            let (api, id) = (s.api.clone(), t.id.clone());
            std::thread::spawn(move || {
                let res: api::Res<Vec<_>> = Ok(artists.iter().filter_map(|a| api.artist_brief(a).ok()).collect());
                deliver(id, res, "исполнители трека", |s, ui, list| s.set_artists(ui, list));
            });
        }

        let (api, id) = (s.api.clone(), t.id.clone());
        std::thread::spawn(move || {
            let res = api.similar_tracks(&id);
            deliver(id, res, "похожие треки", |s, ui, list| s.set_similar(ui, list));
        });
    });
}

/// Номер строки, которая поётся на секунде `pos`; -1 — текст без времени.
pub fn lyric_at(pos: f32) -> i32 {
    with(|s| s.synced.iter().rposition(|l| l.from <= pos + 0.2).map_or(if s.synced.is_empty() { -1 } else { 0 }, |i| i as i32)).unwrap_or(-1)
}

pub fn api() -> Option<Arc<Api>> {
    with(|s| s.api.clone())
}

pub fn similar() -> Vec<Track> {
    with(|s| s.similar.clone()).unwrap_or_default()
}

pub fn playlist(i: usize) -> Option<(String, Playlist)> {
    with(|s| s.playlists.get(i).cloned().map(|p| (s.uid.clone(), p))).flatten()
}

/// Добавили трек в плейлист: число треков в строке поменялось.
pub fn reload_playlists() {
    with(|s| load_playlists(s));
}

/// Лайк исполнителя с карточки: сразу перекрасить сердце, запрос — в фоне.
/// Исполнитель карточки под плеером; -1 — первый исполнитель трека.
pub fn artist_id(i: i32) -> Option<String> {
    with(|s| s.artists.get(i.max(0) as usize).cloned()).flatten()
}

pub fn uid() -> String {
    with(|s| s.uid.clone()).unwrap_or_default()
}

pub fn artist_liked(id: &str) -> bool {
    with(|s| s.liked_artists.contains(id)).unwrap_or(false)
}

pub fn toggle_artist_like(i: usize) {
    if let Some(id) = with(|s| s.artists.get(i).cloned()).flatten() {
        toggle_artist(&id);
    }
}

/// Лайк исполнителя по id; возвращает, лайкнут ли он теперь.
pub fn toggle_artist(id: &str) -> bool {
    with(|s| {
        let id = id.to_string();
        let remove = !s.liked_artists.insert(id.clone());
        if remove {
            s.liked_artists.remove(&id);
        }
        refresh_liked(s);
        let (api, uid) = (s.api.clone(), s.uid.clone());
        std::thread::spawn(move || {
            if let Err(e) = api.like_entity(&uid, "artists", &id, remove) {
                log(format!("лайк исполнителя: {e}"));
            }
        });
        !remove
    })
    .unwrap_or(false)
}

fn refresh_liked(s: &State) {
    let Some(ui) = s.ui.upgrade() else { return };
    let model = ui.global::<Now>().get_artist_cards();
    for (i, id) in s.artists.iter().enumerate() {
        if let Some(mut card) = model.row_data(i) {
            card.liked = s.liked_artists.contains(id);
            model.set_row_data(i, card);
        }
    }
}

impl State {
    fn set_artists(&mut self, ui: &App, list: Vec<(api::Artist, Option<u64>)>) {
        let cards: Vec<ArtistCard> = list
            .iter()
            .map(|(a, listeners)| ArtistCard {
                name: a.name.clone().into(),
                listeners: listeners.map(|n| format!("{} за месяц", group_digits(n))).unwrap_or_default().into(),
                photo: Image::default(),
                color: a.color.map_or(slint::Color::from_argb_u8(0x26, 0xff, 0xff, 0xff), |[r, g, b]| slint::Color::from_rgb_u8(r, g, b)),
                liked: self.liked_artists.contains(&a.id),
            })
            .collect();
        self.artists = list.iter().map(|(a, _)| a.id.clone()).collect();
        let model = Rc::new(VecModel::from(cards));
        let px = (355.0 * ui.window().scale_factor()).round() as u32;
        for (i, (a, _)) in list.iter().enumerate() {
            self.row_image(&model, i, a.cover.as_ref(), px, |c: &mut ArtistCard, img| c.photo = img);
        }
        ui.global::<Now>().set_artist_cards(ModelRc::from(model));
    }

    fn set_similar(&mut self, ui: &App, mut list: Vec<Track>) {
        list.truncate(3);
        let model = Rc::new(VecModel::from(list.iter().map(track_row).collect::<Vec<_>>()));
        let px = (52.0 * ui.window().scale_factor()).round() as u32;
        for (i, t) in list.iter().enumerate() {
            self.row_image(&model, i, t.cover.as_ref(), px, |r: &mut Row, img| r.cover = img);
        }
        ui.global::<Now>().set_similar(ModelRc::from(model));
        self.similar = list;
    }

    fn set_playlists(&mut self, mut list: Vec<Playlist>) {
        let Some(ui) = self.ui.upgrade() else { return };
        list.truncate(3);
        let rows: Vec<Row> = list
            .iter()
            .map(|p| Row { title: p.title.clone().into(), subtitle: tracks_word(p.track_count).into(), cover: Image::default(), explicit: false })
            .collect();
        let model = Rc::new(VecModel::from(rows));
        let px = (80.0 * ui.window().scale_factor()).round() as u32;
        for (i, p) in list.iter().enumerate() {
            self.row_image(&model, i, p.covers.first(), px, |r: &mut Row, img| r.cover = img);
        }
        ui.global::<Now>().set_playlists(ModelRc::from(model));
        self.playlists = list;
    }

    fn row_image<T: Clone + 'static>(&self, model: &Rc<VecModel<T>>, i: usize, uri: Option<&String>, px: u32, set: fn(&mut T, Image)) {
        let Some(uri) = uri else { return };
        let m = model.clone();
        self.images.get(&api::pic(uri, px), px, move |img| {
            if let Some(mut row) = m.row_data(i) {
                set(&mut row, img.clone());
                m.set_row_data(i, row);
            }
        });
    }
}

/// Строка трека для списков; обложку дозагружает тот, кто строит модель.
pub fn track_row(t: &Track) -> Row {
    Row { title: t.title.clone().into(), subtitle: t.artists.clone().into(), cover: Image::default(), explicit: t.explicit }
}

/// Фото исполнителя тает книзу, к плашке трека, как у оригинала.
fn fade_bottom(img: &Image) -> Image {
    let Some(mut buf): Option<SharedPixelBuffer<Rgba8Pixel>> = img.to_rgba8() else { return img.clone() };
    let (w, h) = (buf.width() as usize, buf.height() as usize);
    let pixels = buf.make_mut_slice();
    for y in h * 6 / 10..h {
        let k = ((0.92 - y as f32 / h as f32) / 0.32).clamp(0.0, 1.0);
        let k = k * k * (3.0 - 2.0 * k);
        for p in &mut pixels[y * w..(y + 1) * w] {
            p.a = (p.a as f32 * k) as u8;
        }
    }
    Image::from_rgba8(buf)
}

pub fn time(sec: f32) -> String {
    let s = sec.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// «280 650»: разряды через неразрывный пробел, как у оригинала.
fn group_digits(n: u64) -> String {
    let d = n.to_string();
    let mut out = String::new();
    for (i, ch) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(ch);
    }
    out
}

fn tracks_word(n: u64) -> String {
    if n == 0 {
        return "Нет треков".into();
    }
    let (m10, m100) = (n % 10, n % 100);
    let w = if m10 == 1 && m100 != 11 {
        "трек"
    } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
        "трека"
    } else {
        "треков"
    };
    format!("{} {w}", group_digits(n))
}
