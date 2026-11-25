#!/bin/bash
# Собирает AppImage из уже собранного target/release/yamusic-rs: бинарник, ярлык и значки.
# Архитектура берётся из uname -m (x86_64 или aarch64) и подставляется и в ссылку на
# appimagetool, и в имя готового файла: на aarch64-раннере x86_64-версия appimagetool
# падает с "unsupported architecture". Библиотеки не вшиваются, их даёт система: сборка на
# Ubuntu 22.04 работает с glibc 2.35 и новее. appimagetool 1.9.1 скачивается сам и
# сверяется по sha256; его рантайм собран статически и не требует libfuse2.
# Результат: target/appimage/yamusic-rs-<arch>.AppImage.
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -m)" in
  x86_64)
    ARCH=x86_64
    TOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    ;;
  aarch64 | arm64)
    ARCH=aarch64
    TOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    ;;
  *)
    echo "Неподдерживаемая архитектура: $(uname -m)" >&2
    exit 1
    ;;
esac

TOOL_URL="https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-${ARCH}.AppImage"
OUT=target/appimage
APPDIR="$OUT/yamusic-rs.AppDir"
TOOL="$OUT/appimagetool-${ARCH}"

mkdir -p "$OUT"
if ! echo "$TOOL_SHA256  $TOOL" | sha256sum -c --status 2>/dev/null; then
  curl -fsSL -o "$TOOL" "$TOOL_URL"
  echo "$TOOL_SHA256  $TOOL" | sha256sum -c --quiet
  chmod +x "$TOOL"
fi

rm -rf "$APPDIR"
install -Dm755 target/release/yamusic-rs "$APPDIR/usr/bin/yamusic-rs"
install -Dm644 packaging/yamusic-rs.desktop "$APPDIR/usr/share/applications/yamusic-rs.desktop"
for size in 16 32 48 64 128 256; do
  install -Dm644 "packaging/icons/$size.png" "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps/yamusic-rs.png"
done
ln -s usr/bin/yamusic-rs "$APPDIR/AppRun"
ln -s usr/share/applications/yamusic-rs.desktop "$APPDIR/yamusic-rs.desktop"
ln -s usr/share/icons/hicolor/256x256/apps/yamusic-rs.png "$APPDIR/yamusic-rs.png"
ln -s yamusic-rs.png "$APPDIR/.DirIcon"

# Сам appimagetool — тоже AppImage: распаковка вместо FUSE работает и в контейнерах CI.
# ARCH задаём явно: на aarch64-хосте appimagetool иначе может не угадать имя архитектуры.
ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$APPDIR" "$OUT/yamusic-rs-${ARCH}.AppImage"
