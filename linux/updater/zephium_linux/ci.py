"""The .deb the fork's own Linux workflow built and passed.

The port is compiled on GitHub, not on the machines it is installed on. The
fork's workflow builds every push to main; zephium-linux downloads the newest
build that passed, checks it against the digest GitHub recorded for it, and
installs that.

Only `push` runs count: they build exactly the commit they name. The hourly
catch-up builds a commit that does not exist until it pushes it, and its push
then gets a push run of its own. Pull-request runs build code nobody merged.
"""

import hashlib
import io
import json
import zipfile
from dataclasses import dataclass
from pathlib import Path

from .log import Log

WORKFLOW = "linux.yml"
ARTIFACT = "zephium-linux-deb"
BRANCH = "main"


class NoBuild(Exception):
    pass


@dataclass
class Build:
    run_id: int
    commit: str
    artifact_id: int
    digest: str


def latest(repo: str, log: Log) -> Build:
    """The newest passing push build of the fork's main that still has its .deb."""
    runs = json.loads(log.run(
        ["gh", "api", f"repos/{repo}/actions/workflows/{WORKFLOW}/runs"
                      f"?branch={BRANCH}&event=push&status=success&per_page=20"],
        capture=True))["workflow_runs"]
    for run in runs:
        if run["head_repository"]["full_name"] != repo:
            continue
        artifact = _artifact(repo, run["id"], log)
        if artifact:
            return Build(run["id"], run["head_sha"], artifact["id"], artifact["digest"])
    raise NoBuild(f"no passing Linux build of {repo}'s {BRANCH} has a .deb (yet)")


def download(repo: str, build: Build, directory: Path, log: Log) -> Path:
    """Fetch the build's artifact, check its digest, and unpack the one .deb in it."""
    archive = log.run_bytes(["gh", "api", f"repos/{repo}/actions/artifacts/{build.artifact_id}/zip"])
    found = "sha256:" + hashlib.sha256(archive).hexdigest()
    if found != build.digest:
        raise NoBuild(f"artifact {build.artifact_id} is {found}, but GitHub recorded {build.digest}")
    with zipfile.ZipFile(io.BytesIO(archive)) as files:
        debs = [name for name in files.namelist() if name.endswith(".deb") and "/" not in name]
        if len(debs) != 1:
            raise NoBuild(f"artifact {build.artifact_id} holds {len(debs)} .deb files, not one")
        deb = directory / debs[0]
        deb.write_bytes(files.read(debs[0]))
    return deb


def version_of(deb: Path) -> str:
    """`Zephium_1.0.4_amd64.deb` -> `1.0.4`, the name Tauri gives it."""
    parts = deb.name.split("_")
    if len(parts) != 3:
        raise NoBuild(f"unexpected package name {deb.name}")
    return parts[1]


def _artifact(repo: str, run_id: int, log: Log) -> dict | None:
    listed = json.loads(log.run(["gh", "api", f"repos/{repo}/actions/runs/{run_id}/artifacts"], capture=True))
    for artifact in listed["artifacts"]:
        if artifact["name"] == ARTIFACT and not artifact["expired"] and artifact.get("digest"):
            return artifact
    return None
