"""Where everything lives, and the few choices a person can make."""

import os
import tomllib
from dataclasses import dataclass
from pathlib import Path

HOME = Path.home()
PROJECT = Path(__file__).resolve().parents[2]

CONFIG_FILE = Path(os.environ.get("XDG_CONFIG_HOME", HOME / ".config")) / "zephium-linux" / "config.toml"
STATE_DIR = Path(os.environ.get("XDG_STATE_HOME", HOME / ".local" / "state")) / "zephium-linux"

UPSTREAM_REPO = "zephium-browser/Zephium"
UPSTREAM_URL = f"https://github.com/{UPSTREAM_REPO}.git"

# The project keeps the patch series, the source checkout and the build
# caches; the install itself is per user, outside it.
PATCHES = PROJECT / "patches"
CLONE = PROJECT / "upstream"
TREE = PROJECT / "build" / "tree"
TARGET = PROJECT / "build" / "target"
TOOLCHAIN = PROJECT / "toolchain"
RIG = PROJECT / "tools" / "rig"
WEBKIT_SHIM = PROJECT / "tools" / "test-only" / "webkit-version-shim.so"

INSTALL_ROOT = HOME / ".local" / "opt" / "zephium"
CURRENT = INSTALL_ROOT / "current"
LAUNCHER = HOME / ".local" / "bin" / "zephium"
DESKTOP_ENTRY = HOME / ".local" / "share" / "applications" / "app.zephium.desktop"
ICONS = HOME / ".local" / "share" / "icons" / "hicolor"


@dataclass(frozen=True)
class Settings:
    # "release" follows published releases; "main" follows every upstream commit.
    channel: str = "release"
    # Ask Claude Code to adapt the patches when upstream moved under them.
    ai_repair: bool = True
    ai_repair_model: str = "opus"
    # Installed versions kept for `zephium-linux rollback`.
    keep_versions: int = 3
    build_jobs: int = 8
    # Machines (tailscale ssh targets) that get each install made here.
    peers: tuple[str, ...] = ()
    # The GitHub fork ("owner/name") the port is published to, if any.
    publish_repo: str = ""
    # Every upload (to peers, to GitHub) is paced to this.
    upload_kbps: int = 300


def load_settings() -> Settings:
    if not CONFIG_FILE.exists():
        return Settings()
    with CONFIG_FILE.open("rb") as file:
        values = tomllib.load(file)
    known = {name: values[name] for name in Settings.__dataclass_fields__ if name in values}
    if "peers" in known:
        known["peers"] = tuple(known["peers"])
    settings = Settings(**known)
    if settings.channel not in ("release", "main"):
        raise SystemExit(f"{CONFIG_FILE}: channel must be \"release\" or \"main\", not {settings.channel!r}")
    return settings
