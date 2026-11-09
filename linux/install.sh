#!/usr/bin/env bash
# Puts zephium-linux on PATH, installs its timer, and runs the first update.
set -euo pipefail

HERE=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
UNITS=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user

mkdir -p "$HOME/.local/bin" "$UNITS"
ln -sfn "$HERE/updater/zephium-linux" "$HOME/.local/bin/zephium-linux"
install -m 644 "$HERE/systemd/zephium-linux-update.service" "$HERE/systemd/zephium-linux-update.timer" "$UNITS/"
systemctl --user daemon-reload
systemctl --user enable --now zephium-linux-update.timer

"$HOME/.local/bin/zephium-linux" doctor || true
if [ "${1:-}" != "--no-update" ]; then
    "$HOME/.local/bin/zephium-linux" update
fi
