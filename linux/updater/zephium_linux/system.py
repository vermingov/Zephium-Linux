"""What the machine provides that a build or a launch depends on."""

import re
import shutil
import subprocess
from pathlib import Path

RUNTIME_LIBRARIES = ("webkit2gtk-4.1", "gtk+-3.0", "libsoup-3.0", "javascriptcoregtk-4.1")
BUILD_TOOLS = ("git", "cargo", "rustup", "bsdtar", "openssl", "grim", "pkg-config", "dbus-run-session")


def webkit_version() -> tuple[int, int, int] | None:
    found = subprocess.run(["pkg-config", "--modversion", "webkit2gtk-4.1"], capture_output=True, text=True)
    if found.returncode != 0:
        return None
    return tuple(int(part) for part in found.stdout.strip().split(".")[:3])


def webkit_floor(tree: Path) -> tuple[int, int, int] | None:
    """The minimum Zephium refuses to start below, as the source states it."""
    policy = tree / "crates" / "zephium-core" / "src" / "webkitgtk.rs"
    if not policy.exists():
        return None
    found = re.search(r"SECURITY_FLOOR: \[u32; 3\] = \[(\d+), (\d+), (\d+)\]", policy.read_text())
    return tuple(int(part) for part in found.groups()) if found else None


def missing_libraries() -> list[str]:
    return [name for name in RUNTIME_LIBRARIES
            if subprocess.run(["pkg-config", "--exists", name]).returncode != 0]


def missing_tools() -> list[str]:
    return [tool for tool in BUILD_TOOLS if shutil.which(tool) is None]


def dotted(version: tuple[int, ...] | None) -> str:
    return ".".join(map(str, version)) if version else "unknown"
