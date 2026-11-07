"""One update run: upstream's own Linux build when there is one, else the
fork's. The fork's build is the .deb its Linux workflow made and passed on
GitHub; this machine downloads, checks and installs it, and never compiles."""

import hashlib
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

from . import ci, install, publish, signing, source, state
from .config import CLONE, Settings
from .log import Log
from .smoke import SmokeFailed, smoke_test
from .system import WEBKIT_POLICY, webkit_floor_in
from .upstream import OfficialBuild, fetch, latest_release, official_linux_build


class UpdateFailed(Exception):
    pass


def run(settings: Settings, log: Log, *, force: bool = False) -> str:
    current_state = state.load()
    release = latest_release()
    log.say(f"upstream's latest release is {release.tag}")
    if official := official_linux_build(release):
        return _install_official(official, current_state, settings, log)
    current = current_state.current
    if current and current.kind == "official":
        return f"kept upstream's Linux build {current.version}: release {release.tag} has none"

    if not settings.publish_repo:
        raise UpdateFailed("publish_repo is not set; the port is built by that fork's Linux workflow")
    try:
        found = ci.latest(settings.publish_repo, log)
    except ci.NoBuild as missing:
        raise UpdateFailed(str(missing)) from None
    if not force and current and current.upstream_commit == found.commit:
        return f"up to date: {current.id}"
    publish.fetch_fork(settings.publish_repo, log)
    source.resolve("main", log)  # upstream's main, which the ancestry below is read against
    if current and current.kind == "port" and _on_older_upstream(found.commit, current.upstream_commit, log):
        return f"kept {current.id}: the fork's newest passing build is on older upstream code"
    if not force and current_state.hold.get("commit") == found.commit:
        return f"holding at {current.id if current else 'nothing'}: {found.commit[:10]} was rolled back"

    log.say(f"installing the fork's build of {found.commit[:10]} (workflow run {found.run_id})")
    with tempfile.TemporaryDirectory() as scratch:
        deb = ci.download(settings.publish_repo, found, Path(scratch), log)
        version = ci.version_of(deb)
        version_id = f"{version}+{found.commit[:8]}"
        root = install.unpack_deb(deb, version_id, log)
    install.keep_webkit_floor(version_id, webkit_floor_in(_show(found.commit, WEBKIT_POLICY, log)))
    try:
        smoke_test(install.program(version_id), log)
    except SmokeFailed:
        shutil.rmtree(root, ignore_errors=True)
        raise
    install.activate(version_id, log)
    current_state.trusted_key = _updater_key(_show(found.commit, "desktop/tauri.conf.json", log))
    remember(current_state, state.Installed(id=version_id, version=version, kind="port",
                                             upstream_commit=found.commit, patch_digest=""), settings)
    return f"installed {version_id}"


def _on_older_upstream(candidate: str, installed: str, log: Log) -> bool:
    """Whether `candidate` is built on upstream code older than `installed`.
    Both may be fork commits or upstream commits; each is placed on upstream's
    main by its merge base, since the fork's main is replayed, not appended."""
    def base(commit: str) -> str:
        return log.run(["git", "-C", CLONE, "merge-base", commit, "origin/main"], capture=True).strip()
    candidate_base, installed_base = base(candidate), base(installed)
    if candidate_base == installed_base:
        return False
    ancestor = subprocess.run(["git", "-C", CLONE, "merge-base", "--is-ancestor", candidate_base, installed_base])
    return ancestor.returncode == 0


def _show(commit: str, path: str, log: Log) -> str:
    """A file as it is in `commit`, which the fork's main has."""
    return log.run(["git", "-C", CLONE, "show", f"{commit}:{path}"], capture=True)


def _install_official(official: OfficialBuild, current_state: state.State, settings: Settings, log: Log) -> str:
    version_id = f"{official.version}-official"
    current = current_state.current
    if current and current.id == version_id:
        return f"up to date: upstream's Linux build {official.version}"
    log.say(f"upstream publishes a Linux build: {official.name}")
    data = fetch(official.url)
    _check_official(data, official, current_state, log)
    with tempfile.TemporaryDirectory() as scratch:
        download = Path(scratch) / official.name
        download.write_bytes(data)
        if official.name.lower().endswith(".deb"):
            root = install.unpack_deb(download, version_id, log)
        else:
            root = install.place_appimage(download, version_id)
    try:
        smoke_test(install.program(version_id), log)
    except SmokeFailed:
        shutil.rmtree(root, ignore_errors=True)
        raise
    install.activate(version_id, log)
    remember(current_state, state.Installed(id=version_id, version=official.version, kind="official"), settings)
    return f"switched to upstream's own Linux build {official.version}"


def _check_official(data: bytes, official: OfficialBuild, current_state: state.State, log: Log) -> None:
    key = current_state.trusted_key or _updater_key_at(official.tag, log)
    if official.signature:
        comment = signing.verify(data, official.signature, key)
        log.say(f"signature good ({comment})")
    elif official.sha256:
        if hashlib.sha256(data).hexdigest() != official.sha256:
            raise UpdateFailed(f"{official.name} does not match upstream's SHA256SUMS")
        log.say("no signature published; SHA256SUMS matches")
    else:
        raise UpdateFailed(f"{official.name} is neither signed nor listed in SHA256SUMS; not installing it")


def _updater_key(config: str) -> str:
    return json.loads(config)["plugins"]["updater"]["pubkey"]


def _updater_key_at(tag: str, log: Log) -> str:
    target = source.resolve(tag, log)
    return _updater_key(_show(target.commit, "desktop/tauri.conf.json", log))


def remember(current_state: state.State, entry: state.Installed, settings: Settings) -> None:
    current_state.installed.append(entry)
    current_state.hold = {}
    keep = settings.keep_versions
    dropped, current_state.installed = current_state.installed[:-keep], current_state.installed[-keep:]
    install.remove(dropped)
    state.save(current_state)
