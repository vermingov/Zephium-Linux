"""Drill for the repair path, run by hand: python3 -I updater/tests/drill_repair.py

A made-up upstream commit rewrites the lines patch 0004 changes, so the series
conflicts; Claude Code must carry the patch over and leave a tree that passes
`cargo check`. It spends Claude usage (about a minute and $0.60 when written)
and leaves build/tree on the drill commit; the next update resets it."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from zephium_linux import build, repair, source
from zephium_linux.config import TREE, load_settings
from zephium_linux.log import Log

log = Log(Path(f"/tmp/zephium-repair-drill-{Path.home().name}.log"))
settings = load_settings()
real = source.resolve("v1.0.3", log)
source.checkout(real, log)
linux = TREE / "desktop/src/platform/linux.rs"
text = linux.read_text()
old = """    let sized_chrome = chrome.clone();
    fixed.connect_size_allocate(move |_, allocation| {
        sized_chrome.set_size_request(allocation.width(), allocation.height());
    });"""
assert old in text
linux.write_text(text.replace(old, """    let chrome_view = chrome.clone();
    fixed.connect_size_allocate(move |_, allocation| {
        // The chrome tracks the window's whole rectangle.
        chrome_view.set_size_request(allocation.width(), allocation.height());
    });"""))
log.run(["git", "-C", TREE, "commit", "-q", "-am", "drill: upstream renames the chrome handle"])
commit = log.run(["git", "-C", TREE, "rev-parse", "HEAD"], capture=True).strip()
target = source.Target(ref="drill", commit=commit, version=real.version)
try:
    source.apply_patches(log)
    log.say("DRILL BROKEN: the patches applied cleanly")
    sys.exit(2)
except source.PatchConflict as conflict:
    log.say("patch conflict reproduced; handing it to the repair")
    repair.repair(target, conflict.output, conflict=True, settings=settings, log=log)
build.check(settings, log)
log.say("repair drill passed: series applied and cargo check succeeded")
print(log.run(["git", "-C", TREE, "show", "--stat", "--format=%s", "HEAD"], capture=True))
print(log.run(["git", "-C", TREE, "show", "HEAD", "--", "desktop/src/platform/linux.rs"], capture=True))
