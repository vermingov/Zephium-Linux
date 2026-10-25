"""Hands this machine's install to other machines over `tailscale ssh`.

A peer does not build: it receives the exact files that passed the smoke test
here, so a full disk or a missing toolchain over there costs nothing. The
upload is paced, because a home uplink is small and a full one stalls the house.
"""

import json
import shlex
import subprocess

from .config import INSTALL_ROOT, PROJECT, Settings
from .log import CommandFailed, Log
from .pacing import paced
from .state import Installed

REMOTE_HOME = "~/.local/share/zephium-linux"
REMOTE_TOOL = f"{REMOTE_HOME}/updater/zephium-linux"
REMOTE_STATE = "~/.local/state/zephium-linux/state.json"
INCOMING = "~/.local/opt/zephium/.incoming"


def push(current: Installed | None, settings: Settings, log: Log) -> list[str]:
    """Bring every peer to `current`; one line per peer for the run's outcome."""
    if current is None:
        return []
    outcomes = []
    for peer in settings.peers:
        try:
            outcomes.append(_bring_up_to_date(peer, current, settings, log))
        except (CommandFailed, OSError, ValueError) as failure:
            log.say(f"{peer}: not updated: {failure}")
            outcomes.append(f"{peer}: not updated ({failure})")
    return outcomes


def _bring_up_to_date(peer: str, current: Installed, settings: Settings, log: Log) -> str:
    if _remote_current(peer, log) == current.id:
        return f"{peer}: up to date"
    log.say(f"{peer}: sending {current.id} (paced to {settings.upload_kbps} kB/s)")
    _send_tool(peer, log)
    _send_install(peer, current.id, settings.upload_kbps, log)
    arguments = [current.id, current.kind, current.version, current.upstream_commit or "-",
                 current.patch_digest or "-"]
    _remote(peer, f"{REMOTE_TOOL} receive {' '.join(map(shlex.quote, arguments))}", log)
    return f"{peer}: now on {current.id}"


def _ssh(peer: str, command: str) -> list[str]:
    """The peer's login shell runs `command`; it may be zsh, so commands avoid globs."""
    return ["tailscale", "ssh", peer, "--", command]


def _remote(peer: str, command: str, log: Log) -> str:
    return log.run(_ssh(peer, command), capture=True, timeout=600)


def _remote_fed(peer: str, command: str, chunks, log: Log) -> None:
    args = _ssh(peer, command)
    log.file.write(f"$ {' '.join(args)} < stream\n")
    process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=log.file, stderr=subprocess.STDOUT)
    try:
        for chunk in chunks:
            process.stdin.write(chunk)
    finally:
        process.stdin.close()
    if process.wait() != 0:
        raise CommandFailed(args, process.returncode, log.tail(20))


def _remote_current(peer: str, log: Log) -> str | None:
    raw = _remote(peer, f"cat {REMOTE_STATE} 2>/dev/null || true", log).strip()
    if not raw:
        return None
    installed = json.loads(raw).get("installed", [])
    return installed[-1]["id"] if installed else None


def _send_tool(peer: str, log: Log) -> None:
    """The updater itself, so the peer can install, report and roll back."""
    archive = subprocess.run(["tar", "-C", PROJECT, "--exclude=__pycache__", "-czf", "-", "updater"],
                             capture_output=True, check=True).stdout
    _remote_fed(peer, f"mkdir -p {REMOTE_HOME} ~/.local/bin && tar -xzf - -C {REMOTE_HOME} && "
                      f"ln -sfn {REMOTE_TOOL} ~/.local/bin/zephium-linux", [archive], log)


def _send_install(peer: str, version_id: str, kilobytes_per_second: int, log: Log) -> None:
    tar = subprocess.Popen(["tar", "-C", INSTALL_ROOT, "-czf", "-", version_id], stdout=subprocess.PIPE)
    try:
        _remote_fed(peer, f"rm -rf {INCOMING} && mkdir -p {INCOMING} && tar -xzf - -C {INCOMING}",
                    paced(tar.stdout, kilobytes_per_second), log)
    finally:
        tar.stdout.close()
        if tar.wait() != 0:
            raise CommandFailed(["tar", version_id], tar.returncode, "")
