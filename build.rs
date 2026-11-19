/// Имя приложения для людей: заголовок окна, трей, диспетчер задач, Discord, список устройств
/// в аккаунте Яндекса. В коде доступно как `env!("APP_NAME")`.
const APP_NAME: &str = "Яндекс Музыка RS";

fn main() {
    #[cfg(feature = "asio")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-env-changed=CPAL_ASIO_DIR");
        let sdk = std::env::var_os("CPAL_ASIO_DIR").expect("set CPAL_ASIO_DIR to the ASIO SDK directory");
        let headers = std::path::PathBuf::from(sdk).join("common");
        println!("cargo:rerun-if-changed={}", headers.display());
        println!("cargo:rerun-if-changed=src/asio_host.cpp");
        cc::Build::new().cpp(true).std("c++17").include(headers).file("src/asio_host.cpp").compile("yamusic_asio");
    }
    println!("cargo:rustc-env=APP_NAME={APP_NAME}");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
    )
    .unwrap();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && (cfg!(windows) || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc"))
    {
        resources::embed();
    }
}

/// Ресурсы exe: описание (его показывают диспетчер задач и свойства файла) и значок из
/// `icons/app.ico`. Для кросс-сборки MSVC используется llvm-rc.
mod resources {
    use super::APP_NAME;
    use std::path::{Path, PathBuf};

    pub fn embed() {
        let (compiler, flags): (PathBuf, &[&str]) = if cfg!(windows) {
            let Some(rc) = find_rc() else {
                println!("cargo:warning=rc.exe не найден, exe без значка и описания");
                return;
            };
            (rc, &["/nologo"])
        } else {
            ("llvm-rc".into(), &["/no-preprocess", "/C", "65001"])
        };
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("icons").join("app.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        std::fs::copy(&icon, out.join("app.ico")).expect("icons/app.ico");
        let script = format!("#pragma code_page(65001)\n1 ICON \"app.ico\"\n{}", version_info());
        std::fs::write(out.join("app.rc"), script).unwrap();

        let res = out.join("app.res");
        let ok = std::process::Command::new(&compiler)
            .current_dir(&out)
            .args(flags)
            .arg("/fo")
            .arg(&res)
            .arg("app.rc")
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        } else {
            println!("cargo:warning={} не смог собрать ресурсы, exe без значка и описания", compiler.display());
        }
    }

    fn version_info() -> String {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap();
        let exe = std::env::var("CARGO_PKG_NAME").unwrap();
        let nums: Vec<&str> = version.split(['.', '-']).take(3).collect();
        let num = format!("{},0", nums.join(","));
        format!(
            r#"1 VERSIONINFO
FILEVERSION {num}
PRODUCTVERSION {num}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "041904B0"
    BEGIN
      VALUE "FileDescription", "{APP_NAME}"
      VALUE "ProductName", "{APP_NAME}"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "InternalName", "{exe}"
      VALUE "OriginalFilename", "{exe}.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x419, 1200
  END
END
"#
        )
    }

    fn find_rc() -> Option<PathBuf> {
        let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
        let mut vers: Vec<PathBuf> = std::fs::read_dir(kits).ok()?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        vers.sort();
        vers.into_iter().rev().map(|v| v.join("x64").join("rc.exe")).find(|p| p.exists())
    }
}
