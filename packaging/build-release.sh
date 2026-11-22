#!/bin/sh
# Builds the release tarball in a Debian 12 container (needs glibc 2.35+), so the
# binaries run on most current distros, not just the build machine's.
#   packaging/build-release.sh           -> dist/notion-client-<version>-x86_64-linux.tar.gz
set -eu
cd "$(dirname "$0")/.."
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
NAME="notion-client-$VERSION-x86_64-linux"

docker run --rm -u "$(id -u):$(id -g)" -e CARGO_HOME=/work/target/docker-cargo \
    -v "$PWD":/work -w /work rust:1-bookworm \
    cargo build --release --locked -p notion-egui -p notion-cli --target-dir target/docker

rm -rf "dist/$NAME" && mkdir -p "dist/$NAME"
cp target/docker/release/notion-egui target/docker/release/notion-cli "dist/$NAME/"
strip "dist/$NAME/notion-egui" "dist/$NAME/notion-cli" 2>/dev/null || true
cp packaging/install.sh packaging/notion-client packaging/notion-egui.desktop packaging/notion-egui.svg \
    README.md SETUP.md LICENSE "dist/$NAME/"
cp crates/notion-egui/assets/fonts/OFL.txt "dist/$NAME/OFL-Inter.txt"
chmod 755 "dist/$NAME/install.sh" "dist/$NAME/notion-client"
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"
(cd dist && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "dist/$NAME.tar.gz"
