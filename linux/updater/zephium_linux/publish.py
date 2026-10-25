"""Publishes the port to its GitHub fork.

The fork's `linux` branch is the patched upstream tree exactly as it was built
here, plus one commit holding the fork's front page (`.github/README.md`, which
GitHub shows instead of upstream's README) and this tooling under `linux/`.
It is replayed onto each upstream release, so it is force-pushed; every
published build also gets a tag and, for releases, a GitHub release carrying
the install that passed the smoke test.
"""

import hashlib
import json
import shutil
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

from . import state
from .config import CLONE, INSTALL_ROOT, PROJECT, TREE, Settings
from .log import Log
from .pacing import paced
from .source import Target

REMOTE = "fork"
BRANCH = "linux"
PAGE = PROJECT / "publish"


def publish(target: Target, current: state.Installed, current_state: state.State,
            settings: Settings, log: Log) -> str | None:
    """Publish `current` (built from `target`, now patched in the tree) once."""
    if not settings.publish_repo or current.kind != "port":
        return None
    if current_state.published.get("id") == current.id:
        return None
    _point_remote_at(settings.publish_repo, log)
    _commit_page_and_tooling(log)
    log.run(["git", "-C", TREE, "push", "--quiet", "--force", REMOTE, f"HEAD:refs/heads/{BRANCH}"])
    tag = None
    if target.ref != "main":
        tag = _next_tag(target.version, log)
        log.run(["git", "-C", TREE, "tag", "--annotate", tag, "--message",
                 f"Zephium-Linux {target.version}: upstream {target.ref} with the Linux patches"])
        log.run(["git", "-C", TREE, "push", "--quiet", REMOTE, f"refs/tags/{tag}"])
        _release(settings, tag, target, current, log)
    current_state.published = {"id": current.id, "tag": tag, "at": time.time()}
    state.save(current_state)
    return f"published {tag or 'the linux branch'} to {settings.publish_repo}"


def _point_remote_at(repo: str, log: Log) -> None:
    url = f"https://github.com/{repo}.git"
    remotes = log.run(["git", "-C", CLONE, "remote"], capture=True).split()
    action = "set-url" if REMOTE in remotes else "add"
    log.run(["git", "-C", CLONE, "remote", action, REMOTE, url])


def _commit_page_and_tooling(log: Log) -> None:
    tracked = log.run(["git", "-C", PROJECT, "ls-files"], capture=True).splitlines()
    shutil.rmtree(TREE / "linux", ignore_errors=True)
    for name in tracked:
        if name.startswith("publish/"):
            continue
        destination = TREE / "linux" / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(PROJECT / name, destination, follow_symlinks=False)
    shutil.copy2(PAGE / "README.md", TREE / ".github" / "README.md")
    shutil.rmtree(TREE / ".github" / "linux", ignore_errors=True)
    shutil.copytree(PAGE / "linux", TREE / ".github" / "linux")
    log.run(["git", "-C", TREE, "add", "--all", "linux", ".github/README.md", ".github/linux"])
    log.run(["git", "-C", TREE, "commit", "--quiet", "--message",
             "linux: the port's front page, and its build and update tooling\n\n"
             "GitHub shows .github/README.md instead of upstream's README, which stays\n"
             "untouched. linux/ is what builds, tests and installs the port."])


def _next_tag(version: str, log: Log) -> str:
    published = log.run(["git", "-C", CLONE, "ls-remote", "--tags", REMOTE, f"v{version}-linux.*"],
                        capture=True)
    numbers = [int(line.rsplit(".", 1)[1]) for line in published.splitlines()
               if not line.endswith("^{}") and line.rsplit(".", 1)[1].isdigit()]
    return f"v{version}-linux.{max(numbers, default=0) + 1}"


def _release(settings: Settings, tag: str, target: Target, current: state.Installed, log: Log) -> None:
    name = f"Zephium-Linux-{target.version}-x86_64.tar.gz"
    with tempfile.TemporaryDirectory() as scratch:
        archive = Path(scratch) / name
        log.run(["tar", "-C", INSTALL_ROOT, "-czf", archive, current.id])
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        log.run(["gh", "release", "create", tag, "--repo", settings.publish_repo, "--verify-tag",
                 "--title", f"Zephium-Linux {target.version}", "--notes", _notes(settings.publish_repo, target, current, name, digest)])
        release = json.loads(log.run(["gh", "api", f"repos/{settings.publish_repo}/releases/tags/{tag}"],
                                     capture=True))
        log.say(f"uploading {name} ({archive.stat().st_size // 1048576} MB, paced to {settings.upload_kbps} kB/s)")
        _upload(release["upload_url"].split("{")[0], archive, settings.upload_kbps)


def _upload(url: str, archive: Path, kilobytes_per_second: int) -> None:
    token = subprocess.run(["gh", "auth", "token"], capture_output=True, text=True, check=True).stdout.strip()
    with archive.open("rb") as file:
        request = urllib.request.Request(
            f"{url}?name={archive.name}",
            data=paced(file, kilobytes_per_second),
            method="POST",
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/gzip",
                     "Content-Length": str(archive.stat().st_size), "Accept": "application/vnd.github+json",
                     "User-Agent": "zephium-linux"},
        )
        with urllib.request.urlopen(request, timeout=3600) as reply:
            reply.read()


def _notes(repo: str, target: Target, current: state.Installed, name: str, digest: str) -> str:
    return f"""Upstream Zephium [{target.ref}](https://github.com/zephium-browser/Zephium/releases/tag/{target.ref}) \
({target.commit[:10]}) with the Linux patches, built on CachyOS and started on a headless desktop \
before publishing. Build `{current.id}`.

Needs x86_64 Arch Linux or CachyOS with `webkit2gtk-4.1` 2.54.0 or newer.

```sh
mkdir -p ~/.local/opt/zephium && tar -xzf {name} -C ~/.local/opt/zephium
~/.local/opt/zephium/{current.id}/zephium
```

To build it yourself and keep it updated, see [linux/](https://github.com/{repo}/tree/{BRANCH}/linux).

SHA-256 of `{name}`: `{digest}`
"""
