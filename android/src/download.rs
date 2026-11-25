//! Скачивание треков в «Музыку» телефона, с тегами и обложкой, как на десктопе.

use crate::api::{self, Api, Track};
use crate::config::log;
use crate::save::{file_name_max, save_track, saved_already};
use crate::{sys, toast};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static BUSY: AtomicBool = AtomicBool::new(false);

/// `folder` пустой — файлы ложатся прямо в «Музыка/Яндекс Музыка».
pub fn start(api: Arc<Api>, folder: &str, tracks: Vec<Track>) {
    if tracks.is_empty() {
        return;
    }
    if !sys::storage_permission() {
        toast("Разрешите доступ к файлам и нажмите ещё раз");
        return;
    }
    let Some(music) = sys::music_dir() else {
        toast("Не нашлась папка «Музыка»");
        return;
    };
    if BUSY.swap(true, Ordering::SeqCst) {
        toast("Скачивание уже идёт");
        return;
    }
    let mut dir = music.join("Яндекс Музыка");
    if !folder.is_empty() {
        dir = dir.join(file_name_max(folder, 100));
    }
    // Lossless качаем, когда в настройках выбрано «Превосходное».
    let flac = api::QUALITY.load(Ordering::Relaxed) == 0;
    let total = tracks.len();
    toast(if total == 1 { "Скачиваем трек".into() } else { format!("Скачиваем {}", crate::pages::tracks_word(total as u64)) });
    std::thread::spawn(move || {
        let tmp = sys::cache_base().join("downloads");
        let (mut ok, mut failed) = (0, 0);
        for t in &tracks {
            if saved_already(&dir, t) {
                ok += 1;
                continue;
            }
            match save_track(&api, &tmp, t, flac).and_then(|file| move_to(&file, &dir)) {
                Ok(path) => {
                    sys::scan_file(&path);
                    ok += 1;
                }
                Err(e) => {
                    failed += 1;
                    log(format!("скачивание {}: {e}", t.id));
                }
            }
        }
        BUSY.store(false, Ordering::SeqCst);
        toast(match (total, failed) {
            (1, 0) => "Трек сохранён в «Музыку»".into(),
            (1, _) => "Не удалось скачать трек".into(),
            (_, 0) => format!("Скачано {ok} из {total}"),
            _ => format!("Скачано {ok} из {total}, не удалось {failed}"),
        });
    });
}

/// Перенести готовый трек из кэша в «Музыку»: создать там .part Android 11+ не даёт.
fn move_to(file: &Path, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let dst = dir.join(file.file_name().ok_or("пустое имя файла")?);
    if std::fs::rename(file, &dst).is_err() {
        std::fs::copy(file, &dst).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(file);
    }
    Ok(dst)
}
