# Zephium on Linux

[Zephium](https://github.com/zephium-browser/Zephium) ships for macOS and
Windows. Upstream already has most of a WebKitGTK port in the tree, switched
off. This project switches it on with a short patch series and keeps it on
upstream's newest code. The machines it runs on never compile it: the
patched tree lives on a GitHub fork, whose workflow catches up with upstream
every hour and builds the `.deb`; every hour `zephium-linux` installs the
newest build that passed, after it starts on a headless desktop here. Once
upstream publishes its own Linux build, it installs that instead.

## Use

```sh
./install.sh            # zephium-linux on PATH, the timer, then the first install
zephium                 # the browser (also in the app launcher as Zephium)
zephium-linux status    # what is installed, what the last run did
zephium-linux update    # check now (--force reinstalls anyway)
zephium-linux rollback  # back to the previous install; that build is not reinstalled
zephium-linux publish   # push changed patches to the fork; its workflow builds them
zephium-linux doctor    # missing tools, or a WebKitGTK Zephium will refuse
zephium-linux log       # the last run, in full
```

Zephium refuses to start on a WebKitGTK older than the security floor in its
source (2.54.0 as of 1.0.3). Keep the system updated (`sudo pacman -Syu`);
`doctor` says when it is behind.

To make it the default browser: `xdg-settings set default-web-browser app.zephium.desktop`.

## Settings

`~/.config/zephium-linux/config.toml`, all optional except `publish_repo`:

```toml
publish_repo = "owner/Zephium-Linux"   # the GitHub fork that builds the port
keep_versions = 3                      # installs kept for rollback
```

## Other machines

```toml
peers = ["friend@laptop"]   # tailscale ssh targets
upload_kbps = 300           # every upload is paced, so a small home uplink stays usable
```

A peer never builds or downloads. After each run, any peer not on this
machine's current version gets that exact install (the files that passed the
smoke test here, about 19 MB compressed), plus the `zephium-linux` tool
itself, and installs them with `zephium-linux receive`. That refuses a build
whose libraries do not resolve there. On the peer, `zephium-linux status`,
`rollback` and `doctor` work as here. A peer that was offline catches up on
the next run; updates reach it only while this machine runs its timer.

## The fork

The fork has one branch, `main`: upstream's newest main, the patches replayed
onto it, and one commit with the fork's front page (`publish/README.md`
becomes `.github/README.md`, which GitHub shows instead of upstream's README),
its CI (`publish/workflows/`, replacing upstream's macOS and Windows
workflows) and this directory as `linux/`. It is force-pushed whenever it is
replayed.

- The fork's Linux workflow builds every push to `main` in an Arch container
  (upstream's guard tests, then the release `.deb`) and keeps the `.deb` as
  the run's artifact.
- Every hour the same workflow replays the patches onto upstream's newest
  main (`tools/ci/catch-up.sh`), builds that, and pushes it only if the build
  passed. Pushing workflow files needs a `SYNC_TOKEN` secret: a fine-grained
  token for the fork with Contents and Workflows read/write.
- When a patch stops applying, that run fails and `main` stays where it was.
  Carry the patch over in `upstream/` (branch `linux-port`), export the
  series, and `zephium-linux publish` pushes it; nothing is compiled here.
- `zephium-linux` switches off every other workflow on the fork on each run.

Pushes use `gh`'s login, and commits the repository's configured git identity.

## What a run does

1. Asks GitHub for upstream's newest release. If it carries a Linux build
   (`linux-x86_64*` in its `latest.json`, or a `.deb`/`.AppImage` asset), that
   is downloaded, checked against upstream's updater key (minisign, the key
   from the build installed here, as Tauri's own updater does) or
   `SHA256SUMS`, and installed. From then on only official builds are
   installed.
2. Otherwise it finds the newest passing push build of the fork's `main`,
   downloads its artifact and checks it against the SHA-256 digest GitHub
   recorded for it. Only push runs count: they build exactly the commit they
   name.
3. Unpacks the `.deb` to `~/.local/opt/zephium/<version>/` and starts it on a
   private headless sway (`tools/rig`) with a throwaway profile. It must reach
   "main document initialized" and draw a real window.
4. Switches `~/.local/opt/zephium/current` to it. A running browser keeps its
   files; restart it to get the new one.

The timer runs only on AC power, at idle CPU and IO priority, capped at 14 GB
of memory. Logs: `~/.local/state/zephium-linux/logs/`.

## The patches

| Patch | Why |
| --- | --- |
| 0001 | Upstream's build script and `run()` both exit on Linux. |
| 0002 | `launcher_trigger` names the global-shortcut plugin's key types, and Ask needs the Work crate, on every platform; Linux linked neither. |
| 0003 | The agent-browser port had Linux fallbacks for all but a few native calls. |
| 0004 | The chrome view stayed 1×1 when the window mapped at exactly its requested size (sway, Hyprland), so the window drew nothing. |

Seen working on Linux: onboarding, the new-tab page, browsing, tabs and tab
switching, links, back and forward, keyboard shortcuts, URLs handed over by a
second launch (how other apps open links), and ad and tracker blocking (the
new-tab "blocked" counter stays at 0, though).

Not there yet, because upstream has not written the Linux side: Work runs and
Ask's page reading (no WebKitGTK agent browser or page reader), Chrome
extensions, and a keychain. A Secret Service backend for the keychain is on the
`linux-credentials` branch in `upstream/`: it compiles but is not in the
series, since on Linux nothing that would use it works yet.

`updater/tests/test_offline.py` checks release detection, signatures and the
smoke test's screen reading without network or display.

To change the port: work on the `linux-port` branch in `upstream/`, then
`git -C upstream format-patch --no-signature --zero-commit -o ../patches origin/main..linux-port`
(clear `patches/` first), commit here, and run `zephium-linux publish`. The
fork's workflow builds and tests it; the next `zephium-linux update` installs
it once it passed.

## Testing by hand

`tools/rig` is a headless sway with a scratch profile and its own session
bus. `rig zephium [binary]`, `rig open URL`, `rig shot file.png`,
`rig move X Y` / `click` / `key ctrl+t`, `rig down`. Nothing in it reaches the
real desktop, profile or keyring. `tools/test-only/webkit-version-shim.so`
reports WebKitGTK 2.54.0 so startup can be tested on an older system; it is
only ever used there, never for browsing.
