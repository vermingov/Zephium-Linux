"""The upstream checkout a build starts from, with the Linux patches on top."""

import hashlib
import re
import shutil
from dataclasses import dataclass

from .config import CLONE, PATCHES, TREE, UPSTREAM_URL
from .log import CommandFailed, Log


@dataclass(frozen=True)
class Target:
    ref: str  # a release tag, or "main"
    commit: str
    version: str


class PatchConflict(Exception):
    def __init__(self, output: str):
        self.output = output
        super().__init__("the Linux patches no longer apply to upstream")


def patch_files():
    return sorted(PATCHES.glob("*.patch"))


def patch_digest() -> str:
    digest = hashlib.sha256()
    for patch in patch_files():
        digest.update(patch.name.encode() + b"\0" + patch.read_bytes())
    return digest.hexdigest()[:12]


def resolve(ref: str, log: Log) -> Target:
    if not (CLONE / ".git").exists():
        log.run(["git", "clone", "--filter=blob:none", "--no-checkout", UPSTREAM_URL, CLONE])
    if ref == "main":
        log.run(["git", "-C", CLONE, "fetch", "--quiet", "origin", "main"])
    else:
        log.run(["git", "-C", CLONE, "fetch", "--quiet", "origin", "tag", ref, "--no-tags"])
    commit = log.run(["git", "-C", CLONE, "rev-parse", "FETCH_HEAD^{commit}"], capture=True).strip()
    manifest = log.run(["git", "-C", CLONE, "show", f"{commit}:desktop/Cargo.toml"], capture=True)
    version = re.search(r'^version\s*=\s*"([^"]+)"', manifest, re.MULTILINE).group(1)
    return Target(ref=ref, commit=commit, version=version)


def checkout(target: Target, log: Log) -> None:
    """A clean tree at `target`; node_modules survive so installs stay quick."""
    if not (TREE / ".git").exists():
        log.run(["git", "-C", CLONE, "worktree", "prune"])
        log.run(["git", "-C", CLONE, "worktree", "add", "--force", "--detach", TREE, target.commit])
        return
    try:
        log.run(["git", "-C", TREE, "am", "--abort"])
    except CommandFailed:
        pass  # no patch session was open
    log.run(["git", "-C", TREE, "checkout", "--quiet", "--force", "--detach", target.commit])
    log.run(["git", "-C", TREE, "clean", "-q", "-fdx", "-e", "node_modules"])


def apply_patches(log: Log) -> None:
    """Apply the series; a conflict leaves the `git am` session open for repair."""
    try:
        log.run(["git", "-C", TREE, "am", "--3way", "--quiet", *patch_files()])
    except CommandFailed as failure:
        raise PatchConflict(failure.tail) from None


def refresh_patches(target: Target, log: Log) -> bool:
    """Rewrite the series against `target`, so the next upstream starts close."""
    before = patch_digest()
    staged = PATCHES.with_name("patches.new")
    shutil.rmtree(staged, ignore_errors=True)
    log.run(["git", "-C", TREE, "format-patch", "--quiet", "--no-signature", "--zero-commit",
             "-o", staged, f"{target.commit}..HEAD"])
    for old in patch_files():
        old.unlink()
    for new in sorted(staged.glob("*.patch")):
        new.rename(PATCHES / new.name)
    staged.rmdir()
    return patch_digest() != before
