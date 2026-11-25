//! Плеер в шторке уведомлений и на экране блокировки. Саму шторку ведёт MediaService.java.

use crate::config::log;
use crate::player::{Cmd, Player};
use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::{jint, jlong};
use jni::{JNIEnv, NativeMethod};
use std::ffi::c_void;
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const CLASS_NAME: &str = "io.github.spolzermod.yamusicrs.MediaService";

static PLAYER: OnceLock<Player> = OnceLock::new();
static WAKE: OnceLock<Sender<()>> = OnceLock::new();
static CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// Что сейчас показано в шторке.
struct Shown {
    id: String,
    playing: bool,
    liked: bool,
    duration: f32,
    position: f32,
    at: Instant,
}

impl Shown {
    fn position_now(&self) -> f32 {
        if self.playing {
            self.position + self.at.elapsed().as_secs_f32()
        } else {
            self.position
        }
    }
}

/// Плеер что-то поменял.
pub fn changed() {
    if let Some(tx) = WAKE.get() {
        let _ = tx.send(());
    }
}

pub fn start(player: Player) {
    if !register() {
        log("шторка: MediaService не загрузился");
        return;
    }
    let _ = PLAYER.set(player.clone());
    let (tx, rx) = channel();
    let _ = WAKE.set(tx);
    std::thread::spawn(move || {
        let mut shown: Option<Shown> = None;
        let mut cover: (String, Option<Vec<u8>>) = Default::default();
        while rx.recv().is_ok() {
            // Плеер зовёт пачками, хватит одного обновления на пачку.
            std::thread::sleep(Duration::from_millis(150));
            while rx.try_recv().is_ok() {}
            let s = player.state();
            let Some(t) = s.track else {
                if shown.take().is_some() {
                    hide();
                }
                continue;
            };
            // Шторку оригинал показывает, только когда музыку включили.
            if shown.is_none() && !s.playing {
                continue;
            }
            let liked = player.is_liked(&t.id);
            let same = shown.as_ref().is_some_and(|w| {
                w.id == t.id
                    && w.playing == s.playing
                    && w.liked == liked
                    && w.duration == s.duration
                    && (w.position_now() - s.position).abs() < 2.0
            });
            if same {
                continue;
            }
            if cover.0 != t.id {
                cover = (t.id.clone(), t.cover_url(400).and_then(|u| download(&u)));
            }
            update(&t.title, &t.artists, cover.1.as_deref(), s.duration, s.position, s.playing, liked);
            shown = Some(Shown {
                id: t.id,
                playing: s.playing,
                liked,
                duration: s.duration,
                position: s.position,
                at: Instant::now(),
            });
        }
    });
}

fn download(url: &str) -> Option<Vec<u8>> {
    let resp = reqwest::blocking::get(url).ok()?.error_for_status().ok()?;
    resp.bytes().ok().map(|b| b.to_vec())
}

fn register() -> bool {
    crate::sys::with_env(|env, activity| {
        let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
        let name = env.new_string(CLASS_NAME)?;
        let class = env
            .call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?
            .l()?;
        let class = JClass::from(class);
        env.register_native_methods(
            &class,
            &[
                NativeMethod { name: "action".into(), sig: "(I)V".into(), fn_ptr: on_action as *mut c_void },
                NativeMethod { name: "seek".into(), sig: "(J)V".into(), fn_ptr: on_seek as *mut c_void },
            ],
        )?;
        let global = env.new_global_ref(&class)?;
        let _ = CLASS.set(global);
        Ok(())
    })
    .is_some()
}

fn update(title: &str, artists: &str, cover: Option<&[u8]>, duration: f32, position: f32, playing: bool, liked: bool) {
    let Some(class) = CLASS.get() else { return };
    let ok = crate::sys::with_env(|env, activity| {
        let title = env.new_string(title)?;
        let artists = env.new_string(artists)?;
        let cover = match cover {
            Some(b) => JObject::from(env.byte_array_from_slice(b)?),
            None => JObject::null(),
        };
        let class: &JClass = class.as_obj().into();
        env.call_static_method(
            class,
            "update",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;[BJJZZ)V",
            &[
                JValue::Object(activity),
                JValue::Object(&title),
                JValue::Object(&artists),
                JValue::Object(&cover),
                JValue::Long((duration * 1000.0) as i64),
                JValue::Long((position * 1000.0) as i64),
                JValue::Bool(playing.into()),
                JValue::Bool(liked.into()),
            ],
        )?;
        Ok(())
    });
    if ok.is_none() {
        log("шторка: update не прошёл");
    }
}

fn hide() {
    let Some(class) = CLASS.get() else { return };
    crate::sys::with_env(|env, _| {
        let class: &JClass = class.as_obj().into();
        env.call_static_method(class, "hide", "()V", &[])?;
        Ok(())
    });
}

extern "system" fn on_action(_env: JNIEnv, _class: JClass, code: jint) {
    let Some(p) = PLAYER.get() else { return };
    let s = p.state();
    match code {
        0 if !s.playing => p.send(Cmd::PlayPause),
        1 if s.playing => p.send(Cmd::PlayPause),
        2 => p.send(Cmd::Next),
        3 => p.send(Cmd::Prev),
        4 => {
            if let Some(t) = s.track {
                p.send(Cmd::LikeTrack(t, None));
            }
        }
        5 => p.send(Cmd::Dislike(None)),
        _ => {}
    }
}

extern "system" fn on_seek(_env: JNIEnv, _class: JClass, ms: jlong) {
    if let Some(p) = PLAYER.get() {
        p.send(Cmd::Seek(ms as f32 / 1000.0));
    }
}
