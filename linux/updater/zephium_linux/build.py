"""Builds the patched tree into a .deb with upstream's own release settings."""

import hashlib
import json
import os
import shutil
import tarfile
import tempfile
from pathlib import Path

from .config import TARGET, TOOLCHAIN, TREE, Settings
from .log import Log
from .upstream import fetch

NODE_DIST = "https://nodejs.org/dist"


def build(settings: Settings, log: Log) -> Path:
    environment = _environment(settings, _ensure_toolchain(log))
    bundles = TARGET / "release" / "bundle" / "deb"
    shutil.rmtree(bundles, ignore_errors=True)
    log.say("building the frontend dependencies")
    log.run(["pnpm", "install", "--frozen-lockfile"], cwd=TREE, env=environment)
    log.say("building Zephium (release profile, this takes a while)")
    log.run(["pnpm", "-C", "desktop", "build", "--bundles", "deb"], cwd=TREE, env=environment)
    packages = sorted(bundles.glob("*.deb"))
    if len(packages) != 1:
        raise RuntimeError(f"expected one .deb in {bundles}, found {len(packages)}")
    return packages[0]


def check(settings: Settings, log: Log) -> None:
    """The quick compile check a repair is judged by before a full build."""
    environment = frontend(settings, log)
    log.run(["cargo", "check", "-p", "zephium-desktop", "--features", "tauri/custom-protocol"],
            cwd=TREE, env=environment)


def frontend(settings: Settings, log: Log) -> dict:
    """Builds frame/dist, which the desktop crate embeds even to type-check,
    and returns the environment that built it."""
    environment = _environment(settings, _ensure_toolchain(log))
    log.run(["pnpm", "install", "--frozen-lockfile"], cwd=TREE, env=environment)
    log.run(["pnpm", "frame:build"], cwd=TREE, env=environment)
    return environment


def _environment(settings: Settings, node_bin: Path) -> dict:
    return {
        **os.environ,
        "PATH": f"{node_bin}:{TOOLCHAIN / 'pnpm' / 'bin'}:{os.environ['PATH']}",
        "CARGO_TARGET_DIR": str(TARGET),
        "CARGO_BUILD_JOBS": str(settings.build_jobs),
        "CI": "true",
    }


def _ensure_toolchain(log: Log) -> Path:
    """The exact Node and pnpm the tree pins, installed beside the project."""
    node_version = (TREE / ".node-version").read_text().strip()
    pnpm_version = json.loads((TREE / "package.json").read_text())["packageManager"].split("@")[1]
    node_home = TOOLCHAIN / f"node-v{node_version}-linux-x64"
    if not (node_home / "bin" / "node").exists():
        log.say(f"installing Node {node_version}")
        _install_node(node_version, log)
    node_bin = node_home / "bin"
    with_node = {**os.environ, "PATH": f"{node_bin}:{os.environ['PATH']}"}
    pnpm = TOOLCHAIN / "pnpm" / "bin" / "pnpm"
    if not pnpm.exists() or log.run([pnpm, "--version"], capture=True, env=with_node).strip() != pnpm_version:
        log.say(f"installing pnpm {pnpm_version}")
        log.run([node_bin / "npm", "install", "--global", "--prefix", TOOLCHAIN / "pnpm",
                 f"pnpm@{pnpm_version}"], env=with_node)
    return node_bin


def _install_node(version: str, log: Log) -> None:
    name = f"node-v{version}-linux-x64.tar.xz"
    sums = fetch(f"{NODE_DIST}/v{version}/SHASUMS256.txt").decode()
    expected = next(line.split()[0] for line in sums.splitlines() if line.endswith(f"  {name}"))
    archive = fetch(f"{NODE_DIST}/v{version}/{name}")
    if hashlib.sha256(archive).hexdigest() != expected:
        raise RuntimeError(f"{name} does not match nodejs.org's SHASUMS256.txt")
    TOOLCHAIN.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(suffix=".tar.xz") as file:
        file.write(archive)
        file.flush()
        with tarfile.open(file.name) as tar:
            tar.extractall(TOOLCHAIN, filter="tar")
    log.say(f"Node {version} installed in {TOOLCHAIN}")
