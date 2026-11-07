"""zephium-linux: keeps a Linux build of Zephium installed and current."""

import argparse
import shutil
import subprocess
import time

from . import install, peers, publish, source, state, system, update
from .config import CONFIG_FILE, CURRENT, TREE, load_settings
from .log import CommandFailed, Log
from .smoke import SmokeFailed


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="zephium-linux", description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("update", help="install the newest build: upstream's own, or the fork's CI build")
    run.add_argument("--force", action="store_true", help="reinstall even when nothing changed")
    run.add_argument("--quiet", action="store_true", help="notify only when something changed or failed")
    published = commands.add_parser("publish", help="replay the patches onto upstream's main and push them to the fork")
    published.add_argument("--check", metavar="NAME",
                           help="push upstream/'s linux-port branch as the tag check-NAME instead; main is not touched")
    commands.add_parser("status", help="what is installed and what the last run did")
    commands.add_parser("rollback", help="go back to the previous installed version")
    commands.add_parser("doctor", help="check what this machine needs to build and run Zephium")
    commands.add_parser("log", help="print the last run's log")
    receive = commands.add_parser("receive", help="install a version another machine sent here")
    for field in ("id", "kind", "version", "upstream_commit", "patch_digest"):
        receive.add_argument(field)
    arguments = parser.parse_args(argv)
    return {"update": _update, "publish": _publish, "status": _status, "rollback": _rollback,
            "doctor": _doctor, "log": _log, "receive": _receive}[arguments.command](arguments)


def _update(arguments) -> int:
    settings = load_settings()
    with state.exclusive_run():
        log = Log(state.new_log())
        started = time.time()
        try:
            outcome = update.run(settings, log, force=arguments.force)
            ok = True
            if sent := peers.push(state.load().current, settings, log):
                outcome += "; " + "; ".join(sent)
            try:
                publish.only_linux_workflows(settings, log)
            except CommandFailed as failure:
                log.say(f"upstream's workflows on the fork were not switched off: {failure}")
        except (update.UpdateFailed, SmokeFailed, CommandFailed, RuntimeError, OSError) as failure:
            outcome, ok = f"update failed: {failure}", False
        log.say(outcome)
        recorded = state.load()
        recorded.last_run = {"at": started, "ok": ok, "outcome": outcome, "log": str(log.path)}
        state.save(recorded)
    changed = outcome.startswith(("installed", "switched")) or "now on" in outcome
    if changed or not ok or not arguments.quiet:
        _notify("Zephium updated" if changed else "Zephium update" if ok else "Zephium update failed",
                outcome + ("\nRestart Zephium to use it." if changed else ""))
    return 0 if ok else 1


def _publish(arguments) -> int:
    settings = load_settings()
    with state.exclusive_run():
        log = Log(state.new_log())
        try:
            if arguments.check:
                outcome = publish.publish_check(arguments.check, settings, log)
            else:
                outcome = publish.publish_series(settings, log)
        except source.PatchConflict as conflict:
            print(f"a patch no longer applies to upstream's main; nothing was pushed:\n{conflict.output}")
            return 1
    print(outcome)
    return 0


def _status(_arguments) -> int:
    recorded = state.load()
    current = recorded.current
    if current:
        origin = "upstream's Linux build" if current.kind == "official" else "the fork's CI build of upstream + patches"
        print(f"installed: {current.id} ({origin})")
        print(f"  program: {CURRENT}/{install.ENTRY_POINT}")
        for older in reversed(recorded.installed[:-1]):
            print(f"  kept for rollback: {older.id}")
    else:
        print("installed: nothing yet (run `zephium-linux update`)")
    settings = load_settings()
    print(f"channel: {settings.channel}  (config: {CONFIG_FILE})")
    if settings.peers:
        print(f"sends each install to: {', '.join(settings.peers)}")
    if recorded.hold:
        print(f"holding: not reinstalling {recorded.hold['commit'][:10]} after a rollback")
    if run := recorded.last_run:
        when = time.strftime("%Y-%m-%d %H:%M", time.localtime(run["at"]))
        print(f"last run: {when}, {'ok' if run['ok'] else 'FAILED'}: {run['outcome']}")
        print(f"  log: {run['log']}")
    return 0


def _rollback(_arguments) -> int:
    with state.exclusive_run():
        recorded = state.load()
        if len(recorded.installed) < 2:
            print("nothing older is installed")
            return 1
        abandoned = recorded.installed.pop()
        recorded.hold = {"commit": abandoned.upstream_commit, "patch_digest": abandoned.patch_digest}
        install.activate(recorded.current.id, Log(state.new_log()))
        install.remove([abandoned])
        state.save(recorded)
    print(f"rolled back from {abandoned.id} to {recorded.current.id}")
    return 0


def _receive(arguments) -> int:
    with state.exclusive_run():
        install.accept_incoming(arguments.id)
        install.activate(arguments.id, Log(state.new_log()))
        entry = state.Installed(id=arguments.id, kind=arguments.kind, version=arguments.version,
                                upstream_commit=arguments.upstream_commit.strip("-"),
                                patch_digest=arguments.patch_digest.strip("-"))
        update.remember(state.load(), entry, load_settings())
    print(f"installed {arguments.id}")
    _doctor(arguments)  # what this machine still lacks, for the sender's log; not a failure to receive
    return 0


def _doctor(_arguments) -> int:
    problems = []
    if missing := system.missing_tools():
        problems.append(f"missing tools: {', '.join(missing)}")
    if missing := system.missing_libraries():
        problems.append(f"missing libraries (pkg-config): {', '.join(missing)}")
    found = system.webkit_version()
    floor = install.current_webkit_floor() or system.webkit_floor(TREE)
    print(f"WebKitGTK: {system.dotted(found)}, Zephium needs at least {system.dotted(floor)}")
    if found and floor and found < floor:
        problems.append("WebKitGTK is older than Zephium accepts and it will refuse to start: "
                        "update the system (sudo pacman -Syu)")
    for problem in problems:
        print(f"problem: {problem}")
    if not problems:
        print("ok")
    return 1 if problems else 0


def _log(_arguments) -> int:
    run = state.load().last_run
    if not run:
        print("no run yet")
        return 1
    print(open(run["log"]).read(), end="")
    return 0


def _notify(title: str, body: str) -> None:
    if shutil.which("notify-send"):
        subprocess.run(["notify-send", "--app-name=Zephium", f"--icon={install.APP_ID}", title, body],
                       capture_output=True)
