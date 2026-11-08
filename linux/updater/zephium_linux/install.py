"""Versioned installs under ~/.local/opt/zephium, one of them current.

Each version keeps its own directory; switching is one symlink rename, so a
running browser keeps the files it started from and the next launch gets the
new ones. The launcher, desktop entry and icons point at `current` and do not
change between versions.
"""

import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from .config import CURRENT, DESKTOP_ENTRY, ICONS, INSTALL_ROOT, LAUNCHER
from .log import Log
from .state import Installed

APP_ID = "app.zephium"
ENTRY_POINT = "zephium"


def unpack_deb(deb: Path, version_id: str, log: Log) -> Path:
    root = _fresh_root(version_id)
    with tempfile.TemporaryDirectory(dir=INSTALL_ROOT) as scratch:
        log.run(["bsdtar", "-xf", deb, "-C", scratch])
        payload = next(Path(scratch).glob("data.tar*"))
        log.run(["bsdtar", "-xf", payload, "-C", root])
    binaries = [path for path in (root / "usr" / "bin").iterdir() if os.access(path, os.X_OK)]
    if len(binaries) != 1:
        raise RuntimeError(f"expected one program in {deb.name}, found {[b.name for b in binaries]}")
    (root / ENTRY_POINT).symlink_to(binaries[0].relative_to(root))
    return root


def place_appimage(appimage: Path, version_id: str) -> Path:
    root = _fresh_root(version_id)
    shutil.copy2(appimage, root / appimage.name)
    (root / appimage.name).chmod(0o755)
    (root / ENTRY_POINT).symlink_to(appimage.name)
    return root


def activate(version_id: str, log: Log) -> None:
    root = INSTALL_ROOT / version_id
    pending = CURRENT.with_name("current.new")
    pending.unlink(missing_ok=True)
    pending.symlink_to(version_id)
    pending.replace(CURRENT)
    _write_launcher()
    _install_icons(root)
    _write_desktop_entry(root)
    for refresh in (["update-desktop-database", "-q", DESKTOP_ENTRY.parent],
                    ["gtk-update-icon-cache", "-q", "-t", ICONS]):
        if shutil.which(refresh[0]):
            subprocess.run(refresh, capture_output=True)
    log.say(f"current Zephium is now {version_id}")


def remove(entries: list[Installed]) -> None:
    for entry in entries:
        shutil.rmtree(INSTALL_ROOT / entry.id, ignore_errors=True)


def program(version_id: str) -> Path:
    return INSTALL_ROOT / version_id / ENTRY_POINT


# The WebKitGTK floor of the source a version was built from, kept beside it
# so a machine without that source can still say when its WebKitGTK is too old.
WEBKIT_FLOOR = "webkit-floor"


def keep_webkit_floor(version_id: str, floor: tuple[int, int, int] | None) -> None:
    if floor:
        (INSTALL_ROOT / version_id / WEBKIT_FLOOR).write_text(".".join(map(str, floor)) + "\n")


def current_webkit_floor() -> tuple[int, int, int] | None:
    kept = CURRENT / WEBKIT_FLOOR
    return tuple(int(part) for part in kept.read_text().split(".")) if kept.exists() else None


def accept_incoming(version_id: str) -> None:
    """Move a version a peer sent into place, if it can run here."""
    received = INSTALL_ROOT / ".incoming" / version_id
    if not (received / ENTRY_POINT).exists():
        raise RuntimeError(f"nothing complete was received for {version_id}")
    linked = subprocess.run(["ldd", received / ENTRY_POINT], capture_output=True, text=True).stdout
    if missing := [line.split()[0] for line in linked.splitlines() if "not found" in line]:
        raise RuntimeError(f"{version_id} needs libraries this machine lacks: {', '.join(missing)}")
    shutil.rmtree(INSTALL_ROOT / version_id, ignore_errors=True)
    received.rename(INSTALL_ROOT / version_id)
    shutil.rmtree(INSTALL_ROOT / ".incoming", ignore_errors=True)


def _fresh_root(version_id: str) -> Path:
    root = INSTALL_ROOT / version_id
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    return root


def _write_launcher() -> None:
    LAUNCHER.parent.mkdir(parents=True, exist_ok=True)
    LAUNCHER.write_text(f'#!/bin/sh\nexec "{CURRENT / ENTRY_POINT}" "$@"\n')
    LAUNCHER.chmod(0o755)


def _install_icons(root: Path) -> None:
    for icon in (root / "usr" / "share" / "icons" / "hicolor").glob("*/apps/*.png"):
        destination = ICONS / icon.parent.parent.name / "apps" / f"{APP_ID}.png"
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(icon, destination)


def _write_desktop_entry(root: Path) -> None:
    """The package's entry, pointed at the launcher and the installed icon."""
    packaged = next((root / "usr" / "share" / "applications").glob("*.desktop"), None)
    lines = packaged.read_text().splitlines() if packaged else [
        "[Desktop Entry]", "Type=Application", "Name=Zephium", "Terminal=false",
        f"StartupWMClass={APP_ID}", "Categories=Network;WebBrowser;",
        "MimeType=x-scheme-handler/http;x-scheme-handler/https;text/html;",
    ]
    rewritten = [line for line in lines if not line.startswith(("Exec=", "Icon=", "TryExec="))]
    header = rewritten.index("[Desktop Entry]") + 1
    rewritten[header:header] = [f"Exec={LAUNCHER} %U", f"Icon={APP_ID}"]
    # Upstream files Zephium under Office; launchers and xdg-settings look
    # for browsers under WebBrowser.
    rewritten = [line + "Network;WebBrowser;" if line.startswith("Categories=") and "WebBrowser" not in line
                 else line for line in rewritten]
    DESKTOP_ENTRY.parent.mkdir(parents=True, exist_ok=True)
    DESKTOP_ENTRY.write_text("\n".join(rewritten) + "\n")
