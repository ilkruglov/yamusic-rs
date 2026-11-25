/// Имя, которое Яндекс показывает в списке устройств аккаунта.
const APP_NAME: &str = "Яндекс Музыка RS";

fn main() {
    println!("cargo:rustc-env=APP_NAME={APP_NAME}");
    slint_build::compile("ui/app.slint").unwrap();
}
