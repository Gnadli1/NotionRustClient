#!/bin/sh
# Wraps the binaries from build-release.sh into an AppImage.
#   packaging/build-appimage.sh /path/to/appimagetool   -> dist/Notion_Client-<version>-x86_64.AppImage
# appimagetool: https://github.com/AppImage/appimagetool/releases
set -eu
TOOL="$(readlink -f "$1")"
cd "$(dirname "$0")/.."
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
DIR="dist/Notion_Client.AppDir"
rm -rf "$DIR" && mkdir -p "$DIR/usr/bin"
cp target/docker/release/notion-egui target/docker/release/notion-cli packaging/notion-client "$DIR/usr/bin/"
strip "$DIR/usr/bin/notion-egui" "$DIR/usr/bin/notion-cli" 2>/dev/null || true
cp packaging/notion-egui.desktop packaging/notion-egui.svg "$DIR/"
ln -s notion-egui.svg "$DIR/.DirIcon"
printf '#!/bin/sh\nexec "$(dirname "$(readlink -f "$0")")/usr/bin/notion-client" "$@"\n' > "$DIR/AppRun"
chmod 755 "$DIR/AppRun" "$DIR/usr/bin/notion-client"
OUT="dist/Notion_Client-$VERSION-x86_64.AppImage"
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$DIR" "$OUT"
(cd dist && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")
echo "$OUT"
