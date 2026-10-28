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


def pending(current_state: state.State, current: state.Installed, settings: Settings) -> bool:
    record = current_state.published
    return bool(settings.publish_repo) and current.kind == "port" and not (
        record.get("id") == current.id and record.get("released"))


def needs_tree(current_state: state.State, current: state.Installed) -> bool:
    """Whether the branch still has to be pushed, which needs the patched tree."""
    return current_state.published.get("id") != current.id


def publish(target: Target, current: state.Installed, current_state: state.State,
            settings: Settings, log: Log) -> str:
    """Publish `current` (built from `target`). Each step is recorded as it
    lands, so a run that fails halfway resumes instead of tagging twice."""
    _point_remote_at(settings.publish_repo, log)
    if needs_tree(current_state, current):
        _commit_page_and_tooling(log)
        log.run(["git", "-C", TREE, "push", "--quiet", "--force", REMOTE, f"HEAD:refs/heads/{BRANCH}"])
        tag = None
        if target.ref != "main":
            tag = _next_tag(target.version, log)
            log.run(["git", "-C", TREE, "tag", "--annotate", tag, "--message",
                     f"Zephium-Linux {target.version}: upstream {target.ref} with the Linux patches"])
            log.run(["git", "-C", TREE, "push", "--quiet", REMOTE, f"refs/tags/{tag}"])
        current_state.published = {"id": current.id, "tag": tag, "released": tag is None, "at": time.time()}
        state.save(current_state)
    tag = current_state.published["tag"]
    if not current_state.published["released"]:
        _release(settings, tag, target, current, log)
        current_state.published["released"] = True
        state.save(current_state)
    return f"published {tag or 'the linux branch'} to {settings.publish_repo}"


def mirror_upstream_main(settings: Settings, log: Log) -> None:
    """Keep the fork's main equal to upstream's, as GitHub's own sync button does."""
    if settings.publish_repo:
        log.run(["gh", "repo", "sync", settings.publish_repo, "--branch", "main"])


def _point_remote_at(repo: str, log: Log) -> None:
    url = f"https://github.com/{repo}.git"
    remotes = log.run(["git", "-C", CLONE, "remote"], capture=True).split()
    action = "set-url" if REMOTE in remotes else "add"
    log.run(["git", "-C", CLONE, "remote", action, REMOTE, url])


def _commit_page_and_tooling(log: Log) -> None:
    tracked = log.run(["git", "-C", PROJECT, "ls-files"], capture=True).splitlines()
    shutil.rmtree(TREE / "linux", ignore_errors=True)
    for name in tracked:
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
    """Create the release and its archive, whichever of them is still missing."""
    repo = settings.publish_repo
    name = f"Zephium-Linux-{target.version}-x86_64.tar.gz"
    with tempfile.TemporaryDirectory() as scratch:
        archive = Path(scratch) / name
        log.run(["tar", "-C", INSTALL_ROOT, "-czf", archive, current.id])
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        release = _existing_release(repo, tag)
        if release is None:
            log.run(["gh", "release", "create", tag, "--repo", repo, "--verify-tag",
                     "--title", f"Zephium-Linux {target.version}",
                     "--notes", _notes(repo, target, current, name, digest)])
            release = _existing_release(repo, tag)
        if any(asset["name"] == name for asset in release["assets"]):
            return
        log.say(f"uploading {name} ({archive.stat().st_size // 1048576} MB, paced to {settings.upload_kbps} kB/s)")
        _upload(release["upload_url"].split("{")[0], archive, settings.upload_kbps)


def _existing_release(repo: str, tag: str) -> dict | None:
    found = subprocess.run(["gh", "api", f"repos/{repo}/releases/tags/{tag}"], capture_output=True, text=True)
    return json.loads(found.stdout) if found.returncode == 0 else None


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
