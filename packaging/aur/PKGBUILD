# Maintainer: Spolzer <132726602+SpolzerMod@users.noreply.github.com>
#
# Пакет для AUR: готовый .deb из релиза GitHub, собранный в CI на Ubuntu 22.04.
# При новой версии: pkgver, затем `updpkgsums && makepkg --printsrcinfo > .SRCINFO`.

pkgname=yamusic-rs-bin
pkgver=1.1.2
pkgrel=1
pkgdesc='Неофициальный клиент Яндекс Музыки на Rust и Slint, без Electron'
arch=('x86_64')
url='https://github.com/SpolzerMod/yamusic-rs'
license=('GPL-3.0-only')
# Wayland, X11 и xkbcommon winit открывает через dlopen, namcap их не видит.
depends=('alsa-lib' 'fontconfig' 'freetype2' 'glibc' 'hicolor-icon-theme' 'libgcc' 'libstdc++'
         'libx11' 'libxcursor' 'libxi' 'libxkbcommon' 'libxkbcommon-x11' 'libxrandr' 'wayland')
optdepends=('pipewire-alsa: звук через PipeWire'
            'pulseaudio-alsa: звук через PulseAudio'
            'xdg-utils: открывать ссылки в браузере'
            'zenity: выбор папки и картинки (GNOME и другие)'
            'kdialog: выбор папки и картинки (KDE)'
            'noto-fonts-cjk: названия на китайском, японском и корейском')
options=('!debug' '!strip')
provides=('yamusic-rs')
conflicts=('yamusic-rs')
source=("$url/releases/download/v$pkgver/yamusic-rs_$pkgver-1_amd64.deb")
sha256sums=('2ea031bfe066a0c4e8c1327ca87cdf6896f621454858c4e7b91b9115d4f9291a')

package() {
  bsdtar -xf data.tar.* -C "$pkgdir"
  install -Dm644 "$pkgdir/usr/share/doc/yamusic-rs/copyright" "$pkgdir/usr/share/licenses/$pkgname/copyright"
  rm -r "$pkgdir/usr/share/doc"
}
