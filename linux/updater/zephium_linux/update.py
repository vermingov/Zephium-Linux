"""One update run: upstream's own Linux build when there is one, else ours."""

import hashlib
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

from . import build, install, publish, repair, signing, source, state
from .config import CLONE, PROJECT, TREE, Settings
from .log import CommandFailed, Log
from .smoke import SmokeFailed, smoke_test
from .system import webkit_floor
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

    target = source.resolve(release.tag if settings.channel == "release" else "main", log)
    digest = source.patch_digest()
    wanted = {"commit": target.commit, "patch_digest": digest}
    if not force and current and (current.upstream_commit, current.patch_digest) == (target.commit, digest):
        return f"up to date: {current.id}" + _publish_once(target, current_state, settings, log, prepared=False)
    if not force and current_state.hold == wanted:
        return f"holding at {current.id if current else 'nothing'}: {target.version} was rolled back"

    log.say(f"building {target.ref} ({target.commit[:10]}, version {target.version}) with {len(source.patch_files())} patches")
    deb = _patched_build(target, settings, log)
    version_id = f"{target.version}+{target.commit[:8]}.{source.patch_digest()[:8]}"
    root = install.unpack_deb(deb, version_id, log)
    install.keep_webkit_floor(version_id, webkit_floor(TREE))
    try:
        smoke_test(install.program(version_id), log)
    except SmokeFailed:
        shutil.rmtree(root, ignore_errors=True)
        raise
    install.activate(version_id, log)
    if source.refresh_patches(target, log):
        _record_patches(target, log)
    current_state.trusted_key = _updater_key(TREE / "desktop" / "tauri.conf.json")
    remember(current_state, state.Installed(id=version_id, version=target.version, kind="port",
                                             upstream_commit=target.commit,
                                             patch_digest=source.patch_digest()), settings)
    return f"installed {version_id}" + _publish_once(target, current_state, settings, log, prepared=True)


def _publish_once(target: source.Target, current_state: state.State, settings: Settings, log: Log,
                  *, prepared: bool) -> str:
    """Publish the current install if it never was; a failure here leaves the install alone."""
    current = current_state.current
    if not settings.publish_repo or current_state.published.get("id") == current.id:
        return ""
    try:
        if not prepared:
            source.checkout(target, log)
            source.apply_patches(log)
        return f"; {publish.publish(target, current, current_state, settings, log)}"
    except (CommandFailed, source.PatchConflict, OSError) as failure:
        log.say(f"not published: {failure}")
        return f"; not published ({failure})"


def _patched_build(target: source.Target, settings: Settings, log: Log) -> Path:
    """Patch and build; when either fails, one repair attempt, then the same checks."""
    source.checkout(target, log)
    repaired = False
    try:
        source.apply_patches(log)
    except source.PatchConflict as conflict:
        _repair_or_fail(target, conflict.output, conflict=True, settings=settings, log=log)
        repaired = True
    try:
        return build.build(settings, log)
    except CommandFailed as failure:
        if repaired:
            raise UpdateFailed(f"the repaired patches do not build:\n{failure.tail}") from None
        _repair_or_fail(target, log.tail(150), conflict=False, settings=settings, log=log)
        return build.build(settings, log)


def _repair_or_fail(target, problem: str, *, conflict: bool, settings: Settings, log: Log) -> None:
    what = "no longer apply" if conflict else "no longer build"
    if not repair.available(settings):
        raise UpdateFailed(f"the Linux patches {what} on {target.ref}; see the log")
    try:
        repair.repair(target, problem, conflict=conflict, settings=settings, log=log)
    except repair.RepairFailed as failure:
        raise UpdateFailed(f"the Linux patches {what} on {target.ref}, and the repair failed: {failure}") from None


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


def _updater_key(config: Path) -> str:
    return json.loads(config.read_text())["plugins"]["updater"]["pubkey"]


def _updater_key_at(tag: str, log: Log) -> str:
    target = source.resolve(tag, log)
    config = log.run(["git", "-C", CLONE, "show", f"{target.commit}:desktop/tauri.conf.json"],
                     capture=True)
    return json.loads(config)["plugins"]["updater"]["pubkey"]


def remember(current_state: state.State, entry: state.Installed, settings: Settings) -> None:
    current_state.installed.append(entry)
    current_state.hold = {}
    keep = settings.keep_versions
    dropped, current_state.installed = current_state.installed[:-keep], current_state.installed[-keep:]
    install.remove(dropped)
    state.save(current_state)


def _record_patches(target: source.Target, log: Log) -> None:
    """Keep the rebased series in the project's history, when it has one."""
    if not (PROJECT / ".git").exists():
        return
    log.run(["git", "-C", PROJECT, "add", "--all", "patches"])
    if subprocess.run(["git", "-C", PROJECT, "diff", "--cached", "--quiet"]).returncode != 0:
        log.run(["git", "-C", PROJECT, "commit", "--quiet", "-m",
                 f"patches: carry the series to {target.ref} ({target.commit[:10]})"])
