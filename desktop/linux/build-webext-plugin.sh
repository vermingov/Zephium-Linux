#!/bin/sh
# Builds the extension channel's web-process plugin and copies it to
# linux/webext/, where tauri.linux.conf.json packs it into the .deb. The
# copy keeps one fixed path whatever CARGO_TARGET_DIR is.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cargo build --release -p zephium-webext-process --manifest-path "$root/Cargo.toml"
target=${CARGO_TARGET_DIR:-$root/target}
rm -rf "$here/webext"
mkdir "$here/webext"
cp "$target/release/libzephium_webext_process.so" "$here/webext/"
