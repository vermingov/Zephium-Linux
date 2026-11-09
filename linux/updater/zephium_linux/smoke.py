"""Starts a build on a headless desktop and checks that it draws a browser.

Nothing here touches the real session: the rig runs its own compositor,
session bus and a throwaway profile, in directories no manual rig uses.
"""

import os
import subprocess
import tempfile
import time
from pathlib import Path

from .config import RIG, TREE, WEBKIT_SHIM
from .log import Log
from .system import dotted, webkit_floor, webkit_version

READY = "startup: main document initialized and window shown"
FAILED = ("startup: failed", "couldn't start", "is below security floor")
STARTUP_SECONDS = 90
# A browser window shows thousands of colours; a window that drew nothing, or
# a 1x1 chrome on the rig's flat backdrop, shows a handful.
MIN_COLOURS = 500


class SmokeFailed(Exception):
    pass


def smoke_test(binary: Path, log: Log) -> None:
    run_dir = Path(f"/tmp/zr-smoke-{os.getuid()}")
    with tempfile.TemporaryDirectory(prefix="zephium-smoke-") as scratch:
        environment = {**os.environ, "RIG_RUN": str(run_dir), "RIG_SCRATCH": scratch,
                       "RIG_ENV": _rig_env(log)}
        try:
            log.run([RIG, "zephium", binary], env=environment)
            _wait_until_ready(run_dir / "zephium.log")
            time.sleep(3)
            colours = _colours_on_screen(run_dir)
            if colours < MIN_COLOURS:
                raise SmokeFailed(f"the window drew almost nothing ({colours} colours on screen)")
            if subprocess.run([RIG, "alive"], env=environment).returncode != 0:
                raise SmokeFailed("the browser exited after starting")
            log.say(f"smoke test passed: started and drew {colours} colours")
        finally:
            log.file.write((run_dir / "zephium.log").read_text(errors="replace")
                           if (run_dir / "zephium.log").exists() else "")
            subprocess.run([RIG, "down"], env=environment, capture_output=True)


def _rig_env(log: Log) -> str:
    """Only while the system WebKitGTK is below Zephium's floor: report the
    floor so startup can be checked at all. The test opens nothing remote."""
    found, floor = webkit_version(), webkit_floor(TREE)
    if found and floor and found < floor:
        log.say(f"system WebKitGTK {dotted(found)} is below Zephium's floor {dotted(floor)}; "
                "smoke-testing with the version shim (update webkit2gtk-4.1 to browse)")
        if not WEBKIT_SHIM.exists():
            log.run(["cc", "-shared", "-fPIC", "-O2", "-o", WEBKIT_SHIM, WEBKIT_SHIM.with_suffix(".c")])
        return f"LD_PRELOAD={WEBKIT_SHIM}"
    return ""


def _wait_until_ready(browser_log: Path) -> None:
    deadline = time.monotonic() + STARTUP_SECONDS
    while time.monotonic() < deadline:
        text = browser_log.read_text(errors="replace") if browser_log.exists() else ""
        if READY in text:
            return
        if failure := next((marker for marker in FAILED if marker in text), None):
            raise SmokeFailed(f"startup failed ({failure!r} in the browser log)")
        time.sleep(1)
    raise SmokeFailed(f"no window within {STARTUP_SECONDS} s")


def _colours_on_screen(run_dir: Path) -> int:
    shot = subprocess.run(
        ["grim", "-t", "ppm", "-"],
        env={**os.environ, "XDG_RUNTIME_DIR": str(run_dir), "WAYLAND_DISPLAY": "wayland-1"},
        capture_output=True,
        check=True,
    ).stdout
    # P6: four whitespace-separated header fields, one whitespace byte, then
    # packed RGB (which may itself begin with bytes that look like spaces).
    position = 0
    for _ in range(4):
        while shot[position:position + 1].isspace():
            position += 1
        while not shot[position:position + 1].isspace():
            position += 1
    pixels = shot[position + 1:]
    return len({pixels[i:i + 3] for i in range(0, len(pixels) - 2, 3 * 7)})
