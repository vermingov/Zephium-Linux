"""Asks Claude Code to carry the Linux patches over an upstream change.

It works only inside the build tree, with a short list of tools, and its
result is not trusted: the caller checks the tree, then builds and smoke-tests
it exactly as it would an unrepaired one.
"""

import json
import re
import shutil
import subprocess

from . import build
from .config import TREE, Settings
from .log import CommandFailed, Log
from .source import Target

TIMEOUT_SECONDS = 75 * 60
ALLOWED_TOOLS = [
    "Read", "Edit", "Write", "Glob", "Grep",
    "Bash(git status:*)", "Bash(git diff:*)", "Bash(git log:*)", "Bash(git show:*)",
    "Bash(git add:*)", "Bash(git am:*)", "Bash(git commit:*)", "Bash(git apply:*)",
    "Bash(git checkout:*)", "Bash(git restore:*)", "Bash(git rebase:*)", "Bash(git reset:*)",
    "Bash(git -C:*)", "Bash(cargo check:*)", "Bash(cargo build:*)", "Bash(pnpm:*)",
    "Bash(rg:*)", "Bash(ls:*)", "Bash(sed -n:*)", "Bash(head:*)", "Bash(tail:*)", "Bash(wc:*)",
]
DISALLOWED_TOOLS = ["Bash(git push:*)", "Bash(git remote:*)", "Bash(git fetch:*)", "WebFetch", "WebSearch"]
# The series is published under its owner's name alone: no co-author lines.
NO_ATTRIBUTION = '{"includeCoAuthoredBy": false, "attribution": {"commit": "", "pr": ""}}'
ATTRIBUTION_LINE = re.compile(r"^(co-authored-by|claude-session|generated-by):.*$", re.IGNORECASE | re.MULTILINE)

BRIEF = """\
You maintain a small patch series that ports the Zephium browser (Tauri,
Rust, WebKitGTK) to Linux. The working directory is upstream Zephium at
{ref} ({commit}), version {version}. The series ports it by enabling code
upstream already gates for Linux; it never changes macOS or Windows behaviour.

What went wrong:
{problem}

{task}

Rules:
- Keep every change Linux-only: inside cfg(target_os = "linux") or
  cfg(not(any(target_os = "macos", target_os = "windows"))) branches, Linux
  files, or Cargo target sections for Linux. Prefer upstream's own Linux code
  and patterns over new code; if upstream now does what a patch did, drop the
  patch's change.
- Never weaken a security check: the WebKitGTK version floor, sandboxing,
  CSP, permission handlers, signature checks and the like stay as they are.
- Do not touch the network, other directories, or git remotes, and do not
  record notes or memories: this run is unattended.
- frame/dist is already built and the build environment is set, so
  `cargo check` works as it is.
- Finish with `cargo check -p zephium-desktop --features tauri/custom-protocol`
  passing, every change committed on top of {commit}, and a clean `git status`.
  Commit messages follow upstream's style: conventional, lower case, saying why,
  with no Co-Authored-By or other trailers.
"""

CONFLICT_TASK = """\
A `git am --3way` of the series stopped on a conflict and is still open.
Resolve the conflict so the patch does what it did before on the new code,
then `git am --continue`, and repeat until the whole series is applied."""

BUILD_TASK = """\
The series applied, but the build fails (output above). Fix it with the
smallest Linux-only change: amend the patch commit that the error belongs to,
or add a commit "fix(linux): ..." on top."""


class RepairFailed(Exception):
    pass


def available(settings: Settings) -> bool:
    return settings.ai_repair and shutil.which("claude") is not None


def repair(target: Target, problem: str, *, conflict: bool, settings: Settings, log: Log) -> None:
    brief = BRIEF.format(ref=target.ref, commit=target.commit, version=target.version,
                         problem=problem[-6000:], task=CONFLICT_TASK if conflict else BUILD_TASK)
    environment = build.frontend(settings, log)
    log.say("asking Claude Code to adapt the patches")
    try:
        reply = subprocess.run(
            ["claude", "--print", "--output-format", "json", "--no-session-persistence",
             "--model", settings.ai_repair_model, "--permission-mode", "dontAsk", "--strict-mcp-config",
             "--settings", NO_ATTRIBUTION,
             "--allowedTools", *ALLOWED_TOOLS, "--disallowedTools", *DISALLOWED_TOOLS],
            input=brief, cwd=TREE, env=environment, capture_output=True, text=True, timeout=TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        raise RepairFailed(f"Claude Code did not finish within {TIMEOUT_SECONDS // 60} minutes") from None
    log.file.write(reply.stdout + reply.stderr)
    if reply.returncode != 0:
        raise RepairFailed(f"Claude Code exited with {reply.returncode}")
    log.say(f"Claude Code: {_summary(reply.stdout)}")
    _require_finished(target, log)
    _own_the_commits(target, log)


def _summary(output: str) -> str:
    try:
        return json.loads(output).get("result", "").strip().splitlines()[-1][:300]
    except (json.JSONDecodeError, IndexError, AttributeError):
        return "(no summary)"


def _require_finished(target: Target, log: Log) -> None:
    session = log.run(["git", "-C", TREE, "rev-parse", "--git-path", "rebase-apply"], capture=True).strip()
    if (TREE / session).exists():
        raise RepairFailed("the patch session is still open")
    if log.run(["git", "-C", TREE, "status", "--porcelain"], capture=True).strip():
        raise RepairFailed("the tree has uncommitted changes")
    try:
        log.run(["git", "-C", TREE, "merge-base", "--is-ancestor", target.commit, "HEAD"])
    except CommandFailed:
        raise RepairFailed("the repaired commits are not on top of upstream") from None


def _own_the_commits(target: Target, log: Log) -> None:
    """Every commit above upstream is authored by this repository's owner and
    carries no attribution trailer, whatever the repair wrote."""
    name, email = (log.run(["git", "-C", TREE, "config", key], capture=True).strip()
                   for key in ("user.name", "user.email"))
    commits = log.run(["git", "-C", TREE, "rev-list", "--reverse", f"{target.commit}..HEAD"],
                      capture=True).split()
    log.run(["git", "-C", TREE, "reset", "--quiet", "--hard", target.commit])
    for commit in commits:
        message = log.run(["git", "-C", TREE, "log", "-1", "--format=%B", commit], capture=True)
        log.run(["git", "-C", TREE, "cherry-pick", "--allow-empty", "--no-commit", commit])
        log.run(["git", "-C", TREE, "commit", "--quiet", "--allow-empty", f"--author={name} <{email}>",
                 "-m", ATTRIBUTION_LINE.sub("", message).rstrip() + "\n"])
