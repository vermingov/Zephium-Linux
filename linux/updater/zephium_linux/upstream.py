"""What upstream has published: the newest release, and whether it carries Linux."""

import json
import os
import re
import urllib.request
from dataclasses import dataclass

from .config import UPSTREAM_REPO

API = f"https://api.github.com/repos/{UPSTREAM_REPO}"
OTHER_ARCH = re.compile(r"aarch64|arm64|armv7|armhf|i686|riscv", re.IGNORECASE)
# Tauri's own platform keys in latest.json, most preferred first. A .deb
# unpacks into the same layout as our builds; an AppImage runs as it is.
TAURI_LINUX_KEYS = ("linux-x86_64-deb", "linux-x86_64", "linux-x86_64-appimage")


@dataclass(frozen=True)
class Release:
    tag: str
    version: str
    assets: dict[str, str]  # name -> download URL


@dataclass(frozen=True)
class OfficialBuild:
    version: str
    tag: str
    name: str
    url: str
    # Base64 minisign signature as Tauri writes it, when upstream signed it.
    signature: str | None
    sha256: str | None


def fetch(url: str, *, accept: str = "application/octet-stream") -> bytes:
    headers = {"User-Agent": "zephium-linux", "Accept": accept}
    if token := os.environ.get("GITHUB_TOKEN"):
        headers["Authorization"] = f"Bearer {token}"
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60) as reply:
        return reply.read()


def latest_release() -> Release:
    raw = json.loads(fetch(f"{API}/releases/latest", accept="application/vnd.github+json"))
    return Release(
        tag=raw["tag_name"],
        version=raw["tag_name"].removeprefix("v"),
        assets={asset["name"]: asset["browser_download_url"] for asset in raw["assets"]},
    )


def official_linux_build(release: Release) -> OfficialBuild | None:
    return _from_update_manifest(release) or _from_assets(release)


def _from_update_manifest(release: Release) -> OfficialBuild | None:
    if "latest.json" not in release.assets:
        return None
    platforms = json.loads(fetch(release.assets["latest.json"])).get("platforms", {})
    for key in TAURI_LINUX_KEYS:
        if entry := platforms.get(key):
            name = entry["url"].rsplit("/", 1)[-1]
            return OfficialBuild(
                version=release.version,
                tag=release.tag,
                name=name,
                url=entry["url"],
                signature=entry.get("signature"),
                sha256=_published_sha256(release, name),
            )
    return None


def _from_assets(release: Release) -> OfficialBuild | None:
    def rank(name: str) -> int | None:
        lowered = name.lower()
        if OTHER_ARCH.search(name):
            return None
        for position, suffix in enumerate((".deb", ".appimage")):
            if lowered.endswith(suffix):
                return position
        return None

    candidates = sorted((rank(name), name) for name in release.assets if rank(name) is not None)
    if not candidates:
        return None
    name = candidates[0][1]
    signature = release.assets.get(f"{name}.sig")
    return OfficialBuild(
        version=release.version,
        tag=release.tag,
        name=name,
        url=release.assets[name],
        signature=fetch(signature).decode().strip() if signature else None,
        sha256=_published_sha256(release, name),
    )


def _published_sha256(release: Release, name: str) -> str | None:
    if "SHA256SUMS" not in release.assets:
        return None
    for line in fetch(release.assets["SHA256SUMS"]).decode().splitlines():
        digest, _, listed = line.strip().partition(" ")
        if listed.strip().lstrip("*") == name:
            return digest.lower()
    return None
