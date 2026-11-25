//! Профиль и настройки.

use crate::api::{self, Api};
use crate::config::{log, Config, CROSSFADE_SECS};
use crate::player::{Cmd, Player};
use crate::{eq, original, sys, update, App, Prof, T};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

const SUPPORT_CHAT: &str =
    "https://yandex.ru/chat?context=%7B%22entrypoint%22%3A%22web_desktop%22%7D#/user/ee219503-b69b-9484-405b-6877a510e13c";

thread_local! {
    static API: RefCell<Option<Arc<Api>>> = const { RefCell::new(None) };
    static RELEASE_PAGE: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Картинки тем из APK оригинала.
pub fn load_pictures(ui: &App) {
    let Some(mut apk) = original::Apk::open_density_split() else {
        log("сплит с картинками оригинала не найден");
        return;
    };
    let mut picture = |name: &str| {
        let img = image::load_from_memory(&apk.raster(name)?).ok()?.into_rgba8();
        Some(Image::from_rgba8(SharedPixelBuffer::clone_from_slice(img.as_raw(), img.width(), img.height())))
    };
    let p = ui.global::<Prof>();
    p.set_theme_system(picture("system_theme").unwrap_or_default());
    p.set_theme_light(picture("light_theme").unwrap_or_default());
    p.set_theme_dark(picture("dark_theme").unwrap_or_default());
}

/// Настройки из config.json в интерфейс и плеер, кнопки экранов настроек.
pub fn bind(ui: &App, player: &Player) {
    let c = Config::load();
    let p = ui.global::<Prof>();
    p.set_theme(match c.theme.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    });
    apply_theme(ui);
    p.set_kids(!c.allow_explicit);
    p.set_to_end(c.add_to_end);
    p.set_wave_motion(c.wave_motion);
    p.set_crossfade(c.crossfade_secs > 0);
    p.set_updates(c.check_updates);
    p.set_eq_on(c.eq_on);
    p.set_eq_preset(c.eq_preset.min(eq::PRESETS.len() - 1) as i32);
    p.set_eq_gains(gains_model(&c.eq_gains));
    let names: Vec<SharedString> = eq::PRESETS.iter().map(|(n, _)| (*n).into()).collect();
    p.set_eq_presets(ModelRc::from(Rc::new(VecModel::from(names))));
    p.set_version(env!("CARGO_PKG_VERSION").into());
    api::ALLOW_EXPLICIT.store(c.allow_explicit, Relaxed);
    api::ADD_TO_END.store(c.add_to_end, Relaxed);
    player.send(Cmd::AllowExplicit(c.allow_explicit));
    player.send(Cmd::Crossfade(c.crossfade()));
    player.eq.set(c.eq_on, c.eq_preamp, &c.eq_gains);
    if c.check_updates {
        check_updates(ui);
    }

    let weak = ui.as_weak();
    let pl = player.clone();
    p.on_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        let p = ui.global::<Prof>();
        let mut c = Config::load();
        c.theme = ["system", "light", "dark"][p.get_theme().clamp(0, 2) as usize].into();
        apply_theme(&ui);
        let allow = !p.get_kids();
        if allow != c.allow_explicit {
            api::ALLOW_EXPLICIT.store(allow, Relaxed);
            pl.send(Cmd::AllowExplicit(allow));
        }
        c.allow_explicit = allow;
        c.add_to_end = p.get_to_end();
        c.wave_motion = p.get_wave_motion();
        let crossfade = if p.get_crossfade() { CROSSFADE_SECS } else { 0 };
        if crossfade != c.crossfade_secs {
            c.crossfade_secs = crossfade;
            pl.send(Cmd::Crossfade(c.crossfade()));
        }
        api::ADD_TO_END.store(c.add_to_end, Relaxed);
        if p.get_updates() && !c.check_updates {
            check_updates(&ui);
        }
        c.check_updates = p.get_updates();
        c.save();
    });

    let weak = ui.as_weak();
    p.on_less_ai_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        let on = ui.global::<Prof>().get_less_ai();
        api::LESS_AI.store(on, Relaxed);
        let Some(api) = API.with(|a| a.borrow().clone()) else { return };
        std::thread::spawn(move || {
            if let Err(e) = api.set_account_setting("aiContentReductionEnabled", if on { "true" } else { "false" }) {
                log(format!("меньше ИИ: {e}"));
            }
        });
    });

    let (weak, pl) = (ui.as_weak(), player.clone());
    p.on_eq_changed(move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut c = Config::load();
        c.eq_on = ui.global::<Prof>().get_eq_on();
        pl.eq.set(c.eq_on, c.eq_preamp, &c.eq_gains);
        c.save();
    });

    let (weak, pl) = (ui.as_weak(), player.clone());
    p.on_eq_band(move |i, v| {
        let Some(ui) = weak.upgrade() else { return };
        let mut c = Config::load();
        let Some(g) = c.eq_gains.get_mut(i.max(0) as usize) else { return };
        *g = (v * 2.0).round() / 2.0;
        c.eq_preset = 0;
        let p = ui.global::<Prof>();
        p.get_eq_gains().set_row_data(i as usize, *g);
        p.set_eq_preset(0);
        pl.eq.set(c.eq_on, c.eq_preamp, &c.eq_gains);
        c.save();
    });

    let (weak, pl) = (ui.as_weak(), player.clone());
    p.on_eq_pick(move |i| {
        let Some(ui) = weak.upgrade() else { return };
        let i = (i.max(0) as usize).min(eq::PRESETS.len() - 1);
        let mut c = Config::load();
        c.eq_preset = i;
        // «Своя настройка» оставляет то, что накручено руками.
        if i > 0 {
            c.eq_gains = eq::PRESETS[i].1;
        }
        let p = ui.global::<Prof>();
        p.set_eq_preset(i as i32);
        p.set_eq_gains(gains_model(&c.eq_gains));
        pl.eq.set(c.eq_on, c.eq_preamp, &c.eq_gains);
        c.save();
    });

    p.on_link(|id| {
        let url = match id.as_str() {
            "plus" => "https://plus.yandex.ru/my".to_string(),
            "support" => SUPPORT_CHAT.into(),
            "account" => "https://id.yandex.ru/personal".into(),
            "help" => "https://yandex.ru/support/music-app-android/".into(),
            "donate" => "https://pay.cloudtips.ru/p/befb620d".into(),
            "terms" => "https://yandex.ru/legal/music_mobile_agreement/".into(),
            "privacy" => "https://yandex.ru/legal/confidential/".into(),
            "recommendations" => "https://music.yandex.ru/legal/recommendations/ru/#music".into(),
            "components" => env!("CARGO_PKG_REPOSITORY").into(),
            "update" => RELEASE_PAGE.with(|r| r.borrow().clone()),
            _ => return,
        };
        if !url.is_empty() {
            sys::open_url(&url);
        }
    });
}

/// Имя, логин, аватар и «Меньше ИИ» вошедшего аккаунта.
pub fn start(ui: &App, api: Arc<Api>) {
    API.with(|a| *a.borrow_mut() = Some(api.clone()));
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let about = api.about();
        let login = api.account().map(|a| a.login).unwrap_or_default();
        let less_ai = api.account_settings().ok().and_then(|v| v.get("aiContentReductionEnabled").and_then(|b| b.as_bool()));
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            let p = ui.global::<Prof>();
            p.set_login(login.into());
            if let Some(on) = less_ai {
                api::LESS_AI.store(on, Relaxed);
                p.set_less_ai(on);
            }
            match about {
                Ok((name, avatar)) => {
                    p.set_name(name.into());
                    if let Some(url) = avatar {
                        let url = url.replace("islands-retina-50", "islands-200");
                        let weak = ui.as_weak();
                        crate::IMAGES.with(|i| {
                            i.get(&url, 160, move |img| {
                                if let Some(ui) = weak.upgrade() {
                                    ui.global::<Prof>().set_avatar(img.clone());
                                }
                            })
                        });
                    }
                }
                Err(e) => log(format!("профиль: {e}")),
            }
        });
    });
}

/// Забыть аккаунт: профиль пустой до следующего входа.
pub fn clear(ui: &App) {
    API.with(|a| *a.borrow_mut() = None);
    let p = ui.global::<Prof>();
    p.set_screen(0);
    p.set_name("".into());
    p.set_login("".into());
    p.set_avatar(Image::default());
}

fn apply_theme(ui: &App) {
    let dark = match ui.global::<Prof>().get_theme() {
        1 => false,
        2 => true,
        _ => sys::night_mode(),
    };
    ui.global::<T>().set_dark(dark);
}

fn check_updates(ui: &App) {
    let weak = ui.as_weak();
    std::thread::spawn(move || match update::check() {
        Ok(Some(r)) => {
            let _ = slint::invoke_from_event_loop(move || {
                RELEASE_PAGE.with(|p| *p.borrow_mut() = r.page);
                if let Some(ui) = weak.upgrade() {
                    ui.global::<Prof>().set_update(r.version.into());
                }
            });
        }
        Ok(None) => {}
        Err(e) => log(format!("обновления: {e}")),
    });
}

fn gains_model(g: &[f32; 10]) -> ModelRc<f32> {
    ModelRc::from(Rc::new(VecModel::from(g.to_vec())))
}
