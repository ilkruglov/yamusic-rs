//! Окно «О треке» (`CurrentTrackInfo` оригинала): участники записи, у выпуска и главы — ещё
//! описание и ссылка на подкаст или книгу. Данные грузятся при каждом открытии.

use super::*;
use crate::api::{Res, TrackInfo};

thread_local! {
    /// Номер открытия и альбом для строки-ссылки: ответ для закрытого окна не нужен.
    static TA: RefCell<(u64, Option<String>)> = const { RefCell::new((0, None)) };
}

fn line(title: &str, value: &str, link: bool) -> InfoLine {
    InfoLine { title: title.into(), value: value.into(), link, shimmer: false }
}

/// Заглушка на месте того, что грузится или не загрузилось (`TextShimmer` оригинала).
fn shimmer() -> InfoLine {
    InfoLine { shimmer: true, ..Default::default() }
}

impl Ctx {
    pub(super) fn open_track_about(self: &Rc<Self>, t: Track) {
        let net = self.st.borrow().net.clone();
        let Some(net) = net else {
            self.toast("Сначала необходимо авторизоваться");
            return;
        };
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_track_about_title(t.title.clone().into());
        app.set_track_about_version(t.version.clone().into());
        app.set_track_about_explicit(t.explicit);
        // Как у оригинала, пока грузится: заглушка трека (только у выпуска и главы — у обычного
        // трека из `full-info` ничего, кроме шапки, не показывается, а шапка уже известна) и
        // заглушка участников.
        let with_info = t.is_episode() || t.is_chapter();
        let pending = if with_info { vec![shimmer(), shimmer()] } else { vec![shimmer()] };
        app.set_track_about_lines(ModelRc::from(Rc::new(VecModel::from(pending))));
        app.set_track_about_open(true);
        let gen = TA.with(|a| {
            let mut a = a.borrow_mut();
            a.0 += 1;
            a.1 = None;
            a.0
        });
        let api = net.api.clone();
        std::thread::spawn(move || {
            // Запросы параллельно, как у оригинала; окно показывает ответы разом.
            let (credits, info, description) = std::thread::scope(|s| {
                let info = with_info.then(|| s.spawn(|| api.track_full_info(&t.id, t.album_id.as_deref())));
                // У выпуска описание — из `supplement`, без него — `shortDescription`.
                let description = t.is_episode().then(|| s.spawn(|| api.track_supplement(&t.id)));
                let credits = api.track_credits(&t.id);
                let info = info.and_then(|h| h.join().ok());
                let description = description.and_then(|h| h.join().ok()).and_then(Result::ok).filter(|d| !d.is_empty());
                (credits, info, description)
            });
            let _ = slint::invoke_from_event_loop(move || {
                with_ctx(|c| c.show_track_about(gen, &t, credits, info, description));
            });
        });
    }

    fn show_track_about(
        self: &Rc<Self>,
        gen: u64,
        t: &Track,
        credits: Res<Vec<(String, String)>>,
        info: Option<Res<TrackInfo>>,
        description: Option<String>,
    ) {
        let ui = self.ui();
        let app = ui.global::<App>();
        if TA.with(|a| a.borrow().0) != gen || !app.get_track_about_open() {
            return;
        }
        let mut lines = Vec::new();
        let mut album = None;
        match &info {
            Some(Ok(info)) => {
                let text = description.as_deref().unwrap_or(&info.short_description);
                if !text.is_empty() {
                    lines.push(line("", text, false));
                }
                if !info.album_title.is_empty() {
                    // Подпись ссылки — `entity-names.audiobook` / `entity-names.podcast`.
                    let what = if t.is_chapter() { "Аудиокнига" } else { "Подкаст" };
                    lines.push(line(&crate::i18n::tr(what), &info.album_title, info.album_id.is_some()));
                    album = info.album_id.clone();
                }
            }
            Some(Err(e)) => {
                log(format!("full-info {}: {e}", t.id));
                lines.push(shimmer());
            }
            None => {}
        }
        let credits_failed = credits.is_err();
        match credits {
            Ok(list) => lines.extend(list.iter().map(|(r, n)| line(r, n, false))),
            Err(e) => {
                log(format!("credits {}: {e}", t.id));
                lines.push(shimmer());
            }
        }
        // Не загрузилось ничего — окно закрывается с тостом. Упала одна часть — на её месте
        // остаётся заглушка, как у оригинала.
        if credits_failed && !matches!(info, Some(Ok(_))) {
            app.set_track_about_open(false);
            self.toast("При выполнении действия произошла ошибка");
            return;
        }
        TA.with(|a| a.borrow_mut().1 = album);
        app.set_track_about_lines(ModelRc::from(Rc::new(VecModel::from(lines))));
    }

    /// Ссылка на подкаст или книгу: окно закрывается, открывается альбом.
    fn track_about_link(self: &Rc<Self>) {
        let Some(album) = TA.with(|a| a.borrow().1.clone()) else { return };
        let ui = self.ui();
        let app = ui.global::<App>();
        app.set_track_about_open(false);
        app.set_fullscreen(false);
        self.navigate(Page::List, Some(format!("album:{album}")), true);
    }
}

pub(super) fn register(app: &App) {
    // Ссылка в окне одна (альбом), поэтому номер строки не нужен.
    app.on_track_about_link(|_| with_ctx(|c| c.track_about_link()));
}
