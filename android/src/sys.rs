//! То, что на Android даёт система: папки приложения, модель телефона, браузер и путь
//! к установленным приложениям. Всё, кроме папок, идёт через JNI.

use jni::objects::{JObject, JString, JValue};
use jni::JNIEnv;
use slint::android::AndroidApp;
use std::path::PathBuf;
use std::sync::OnceLock;

static APP: OnceLock<AndroidApp> = OnceLock::new();

pub fn init(app: &AndroidApp) {
    let _ = APP.set(app.clone());
}

fn files_dir() -> PathBuf {
    APP.get().and_then(|a| a.internal_data_path()).unwrap_or_else(|| PathBuf::from("/data/local/tmp"))
}

/// Папка настроек: личная папка приложения, другим приложениям её не прочесть.
pub fn config_base() -> PathBuf {
    files_dir()
}

/// `files` и `cache` лежат рядом в папке приложения; cache система чистит сама, когда мало места.
pub fn cache_base() -> PathBuf {
    let files = files_dir();
    files.parent().map(|p| p.join("cache")).unwrap_or(files)
}

/// Модель телефона: Яндекс показывает её в списке устройств аккаунта.
pub fn hostname() -> Option<String> {
    with_env(|env, _| {
        let model = env.get_static_field("android/os/Build", "MODEL", "Ljava/lang/String;")?.l()?;
        java_string(env, model)
    })
}

/// Открыть адрес в браузере.
pub fn open_url(url: &str) {
    let Some(url) = crate::client::browser_url(url) else { return };
    let ok = with_env(|env, activity| {
        let text = env.new_string(&url)?;
        let uri = env
            .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&text)])?
            .l()?;
        let action = env.new_string("android.intent.action.VIEW")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;Landroid/net/Uri;)V",
            &[JValue::Object(&action), JValue::Object(&uri)],
        )?;
        const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
        env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(FLAG_ACTIVITY_NEW_TASK)])?;
        env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
        Ok(())
    });
    if ok.is_none() {
        crate::config::log(format!("не удалось открыть {url}"));
    }
}

/// Отдать ссылку системному «Поделиться».
pub fn share(text: &str) {
    let ok = with_env(|env, activity| {
        let action = env.new_string("android.intent.action.SEND")?;
        let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;)V", &[JValue::Object(&action)])?;
        let mime = env.new_string("text/plain")?;
        env.call_method(&intent, "setType", "(Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&mime)])?;
        let key = env.new_string("android.intent.extra.TEXT")?;
        let value = env.new_string(text)?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&key), JValue::Object(&value)],
        )?;
        let chooser = env
            .call_static_method(
                "android/content/Intent",
                "createChooser",
                "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;",
                &[JValue::Object(&intent), JValue::Object(&JObject::null())],
            )?
            .l()?;
        env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&chooser)])?;
        Ok(())
    });
    if ok.is_none() {
        crate::config::log("не удалось открыть «Поделиться»");
    }
}

/// Путь к APK установленного приложения. Без `<queries>` в манифесте Android 11+ его не покажет.
pub fn package_path(package: &str) -> Option<PathBuf> {
    with_env(|env, activity| {
        let pm = env.call_method(activity, "getPackageManager", "()Landroid/content/pm/PackageManager;", &[])?.l()?;
        let name = env.new_string(package)?;
        let info = env
            .call_method(
                &pm,
                "getApplicationInfo",
                "(Ljava/lang/String;I)Landroid/content/pm/ApplicationInfo;",
                &[JValue::Object(&name), JValue::Int(0)],
            )?
            .l()?;
        let dir = env.get_field(&info, "sourceDir", "Ljava/lang/String;")?.l()?;
        java_string(env, dir)
    })
    .map(PathBuf::from)
}

/// Общая папка «Музыка» во внешней памяти.
pub fn music_dir() -> Option<PathBuf> {
    with_env(|env, _| {
        let kind = env.get_static_field("android/os/Environment", "DIRECTORY_MUSIC", "Ljava/lang/String;")?.l()?;
        let dir = env
            .call_static_method(
                "android/os/Environment",
                "getExternalStoragePublicDirectory",
                "(Ljava/lang/String;)Ljava/io/File;",
                &[JValue::Object(&kind)],
            )?
            .l()?;
        let path = env.call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?;
        java_string(env, path)
    })
    .map(PathBuf::from)
}

/// Показать файл в медиатеке, чтобы его сразу видели другие плееры.
pub fn scan_file(path: &std::path::Path) {
    with_env(|env, activity| {
        let path = env.new_string(path.to_string_lossy())?;
        let paths = env.new_object_array(1, "java/lang/String", &path)?;
        let null = JObject::null();
        env.call_static_method(
            "android/media/MediaScannerConnection",
            "scanFile",
            "(Landroid/content/Context;[Ljava/lang/String;[Ljava/lang/String;Landroid/media/MediaScannerConnection$OnScanCompletedListener;)V",
            &[JValue::Object(activity), JValue::Object(&paths), JValue::Object(&null), JValue::Object(&null)],
        )?;
        Ok(())
    });
}

/// Можно ли писать в «Музыку». До Android 10 включительно на это нужно разрешение: спросим его.
pub fn storage_permission() -> bool {
    with_env(|env, activity| {
        let sdk = env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()?;
        if sdk > 29 {
            return Ok(true);
        }
        let perm = env.new_string("android.permission.WRITE_EXTERNAL_STORAGE")?;
        let granted = env.call_method(activity, "checkSelfPermission", "(Ljava/lang/String;)I", &[JValue::Object(&perm)])?.i()? == 0;
        if !granted {
            let perms = env.new_object_array(1, "java/lang/String", &perm)?;
            env.call_method(activity, "requestPermissions", "([Ljava/lang/String;I)V", &[JValue::Object(&perms), JValue::Int(1)])?;
        }
        Ok(granted)
    })
    .unwrap_or(false)
}

/// Включена ли тёмная тема системы.
pub fn night_mode() -> bool {
    with_env(|env, activity| {
        let res = env.call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])?.l()?;
        let conf = env.call_method(&res, "getConfiguration", "()Landroid/content/res/Configuration;", &[])?.l()?;
        // UI_MODE_NIGHT_MASK и UI_MODE_NIGHT_YES.
        Ok(env.get_field(&conf, "uiMode", "I")?.i()? & 0x30 == 0x20)
    })
    .unwrap_or(false)
}

/// Высота статус-бара в dp.
pub fn status_bar_height() -> Option<f32> {
    with_env(|env, activity| {
        let res = env.call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])?.l()?;
        let name = env.new_string("status_bar_height")?;
        let kind = env.new_string("dimen")?;
        let package = env.new_string("android")?;
        let id = env
            .call_method(&res, "getIdentifier", "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I", &[(&name).into(), (&kind).into(), (&package).into()])?
            .i()?;
        let px = env.call_method(&res, "getDimensionPixelSize", "(I)I", &[id.into()])?.i()?;
        let metrics = env.call_method(&res, "getDisplayMetrics", "()Landroid/util/DisplayMetrics;", &[])?.l()?;
        let density = env.get_field(&metrics, "density", "F")?.f()?;
        Ok(px as f32 / density)
    })
}

fn java_string(env: &mut JNIEnv, obj: JObject) -> jni::errors::Result<String> {
    Ok(env.get_string(&JString::from(obj))?.into())
}

/// Вызов Java из любого потока. Исключение Java гасится и превращается в None.
pub(crate) fn with_env<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> Option<T> {
    let app = APP.get()?;
    // SAFETY: указатели на JavaVM и активити живут, пока жив процесс приложения.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let res = f(&mut env, &activity);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
    res.ok()
}
