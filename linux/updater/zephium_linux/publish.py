"""Puts the port on its GitHub fork.

The fork has one branch, `main`: upstream's newest main, the Linux patches
replayed onto it, and one commit holding the fork's front page
(`.github/README.md`, which GitHub shows instead of upstream's README), its
Linux-only CI in place of upstream's workflows, and this tooling under
`linux/`. Publishing is git only. The fork's workflow builds what is pushed,
catches main up with upstream every hour, and `zephium-linux update`
installs the builds that passed.
"""

import shutil

from . import source
from .config import CLONE, PROJECT, TREE, Settings
from .log import Log

REMOTE = "fork"
BRANCH = "main"
PAGE = PROJECT / "publish"


def publish_series(settings: Settings, log: Log) -> str:
    """Replay the project's patches onto upstream's newest main and push it.
    Raises `source.PatchConflict` when a patch no longer applies."""
    if not settings.publish_repo:
        return "nothing to publish to: publish_repo is not set"
    target = source.resolve("main", log)
    fork_main = fetch_fork(settings.publish_repo, log)
    source.checkout(target, log)
    source.apply_patches(log)
    _commit_page_and_tooling(log)
    # The lease refuses the push if the hourly catch-up moved main meanwhile.
    log.run(["git", "-C", TREE, "push", "--quiet", f"--force-with-lease={BRANCH}:{fork_main}",
             REMOTE, f"HEAD:refs/heads/{BRANCH}"])
    return f"published upstream {target.commit[:10]} with the patches to {settings.publish_repo}; its workflow builds it"


def publish_check(name: str, settings: Settings, log: Log) -> str:
    """Push `upstream/`'s linux-port branch, with the front page and CI on top,
    as the tag check-NAME. The fork's workflow builds and tests it like main;
    nothing installs it, and main is not touched."""
    if not settings.publish_repo:
        return "nothing to publish to: publish_repo is not set"
    _point_remote_at(settings.publish_repo, log)
    branch = log.run(["git", "-C", CLONE, "rev-parse", "linux-port"], capture=True).strip()
    log.run(["git", "-C", TREE, "checkout", "--quiet", "--detach", branch])
    _commit_page_and_tooling(log)
    tag = f"check-{name}"
    log.run(["git", "-C", TREE, "push", "--quiet", "--force", REMOTE, f"HEAD:refs/tags/{tag}"])
    return f"pushed linux-port ({branch[:10]}) as {tag} to {settings.publish_repo}; its workflow builds and tests it"


def fetch_fork(repo: str, log: Log) -> str:
    """Bring the fork's main into the clone and return its commit."""
    _point_remote_at(repo, log)
    log.run(["git", "-C", CLONE, "fetch", "--quiet", REMOTE, BRANCH])
    return log.run(["git", "-C", CLONE, "rev-parse", "FETCH_HEAD"], capture=True).strip()


def only_linux_workflows(settings: Settings, log: Log) -> None:
    """Switch off every workflow on the fork except the ones publish/workflows
    ships. GitHub keeps upstream's registered from before the fork's main
    carried the port, and a manual "Sync fork" would bring them back."""
    if settings.publish_repo:
        _switch_off_others(settings.publish_repo, log)


def _switch_off_others(repo: str, log: Log) -> None:
    ours = {f".github/workflows/{path.name}" for path in (PAGE / "workflows").iterdir()}
    listed = log.run(["gh", "api", "--paginate", f"repos/{repo}/actions/workflows",
                      "--jq", '.workflows[] | "\\(.id) \\(.state) \\(.path)"'], capture=True)
    for line in listed.splitlines():
        workflow_id, workflow_state, path = line.split(" ", 2)
        if path not in ours and workflow_state == "active":
            log.run(["gh", "api", "--method", "PUT", f"repos/{repo}/actions/workflows/{workflow_id}/disable"])
            log.say(f"switched off upstream's {path} on {repo}")


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
    shutil.rmtree(TREE / ".github" / "workflows", ignore_errors=True)
    shutil.copytree(PAGE / "workflows", TREE / ".github" / "workflows")
    log.run(["git", "-C", TREE, "add", "--all", "linux", ".github/README.md", ".github/linux",
             ".github/workflows"])
    log.run(["git", "-C", TREE, "commit", "--quiet", "--message",
             "linux: the port's front page, its CI, and its build and update tooling\n\n"
             "GitHub shows .github/README.md instead of upstream's README, which stays\n"
             "untouched. Upstream's macOS and Windows workflows are replaced by one\n"
             "Linux build. linux/ is what builds, tests and installs the port."])
