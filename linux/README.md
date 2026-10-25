# Zephium on Linux

[Zephium](https://github.com/zephium-browser/Zephium) ships for macOS and
Windows. Upstream already has most of a WebKitGTK port in the tree, switched
off. This project switches it on with a short patch series, builds it, and
keeps it current: every six hours it checks upstream, rebuilds when upstream
moved, and installs the result only after it starts on a headless desktop.
Once upstream publishes its own Linux build, it installs that instead and
stops building.

## Use

```sh
./install.sh            # zephium-linux on PATH, the timer, then the first build
zephium                 # the browser (also in the app launcher as Zephium)
zephium-linux status    # what is installed, what the last run did
zephium-linux update    # check now (--force rebuilds anyway)
zephium-linux rollback  # back to the previous build; that one is not rebuilt
zephium-linux doctor    # missing tools, or a WebKitGTK Zephium will refuse
zephium-linux log       # the last run, in full
```

Zephium refuses to start on a WebKitGTK older than the security floor in its
source (2.54.0 as of 1.0.3). Keep the system updated (`sudo pacman -Syu`);
`doctor` says when it is behind.

To make it the default browser: `xdg-settings set default-web-browser app.zephium.desktop`.

## Settings

`~/.config/zephium-linux/config.toml`, all optional:

```toml
channel = "release"   # or "main": every upstream commit, not just releases
ai_repair = true      # let Claude Code adapt the patches when upstream moves
ai_repair_model = "opus"
keep_versions = 3     # installs kept for rollback
build_jobs = 8
```

## Other machines

```toml
peers = ["friend@laptop"]   # tailscale ssh targets
upload_kbps = 300           # every upload is paced, so a small home uplink stays usable
```

A peer never builds. After each run, any peer not on this machine's current
version gets that exact install (the files that passed the smoke test here,
about 19 MB compressed), plus the `zephium-linux` tool itself, and installs
them with `zephium-linux receive`. That refuses a build whose libraries do not
resolve there. On the peer, `zephium-linux status`, `rollback` and `doctor`
work as here. A peer that was offline catches up on the next run; updates
reach it only while this machine runs its timer.

## Publishing

```toml
publish_repo = "owner/Zephium-Linux"   # a GitHub fork of zephium-browser/Zephium
```

After each new build, the fork's `linux` branch is set to the patched
upstream tree exactly as built here, plus one commit with the fork's front
page (`publish/README.md` becomes `.github/README.md`, which GitHub shows
instead of upstream's README) and this directory as `linux/`. The branch is
replayed onto every upstream release, so it is force-pushed. Each published
build is tagged `v<version>-linux.<n>`, and for releases a GitHub release
carries the smoke-tested install as a `.tar.gz` (uploaded at `upload_kbps`).
Pushes use `gh`'s login. Commits are made under the repository's configured
git identity, and the repair step strips any co-author trailers it adds.

## What a run does

1. Asks GitHub for upstream's newest release. If it carries a Linux build
   (`linux-x86_64*` in its `latest.json`, or a `.deb`/`.AppImage` asset), that
   is downloaded, checked against upstream's updater key (minisign, the key
   from the build installed here, as Tauri's own updater does) or
   `SHA256SUMS`, and installed. From then on only official builds are
   installed.
2. Otherwise it checks out the release (or `main`) in `build/tree` and applies
   `patches/*.patch` with `git am --3way`.
3. If a patch no longer applies, or the build fails, and `claude` is
   installed, Claude Code gets one attempt to carry the patches over, limited
   to the build tree and a short list of git, cargo and file tools.
4. Builds the release `.deb` with upstream's own settings (`tauri build`,
   pinned Node and pnpm in `toolchain/`, Rust from `rust-toolchain.toml`).
5. Unpacks it to `~/.local/opt/zephium/<version>/` and starts it on a
   private headless sway (`tools/rig`) with a throwaway profile. It must reach
   "main document initialized" and draw a real window.
6. Switches `~/.local/opt/zephium/current` to it. A running browser keeps its
   files; restart it to get the new one. The patches are rewritten against
   the new upstream and committed here, so the next update starts close.

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
`updater/tests/drill_repair.py` (by hand; it spends Claude usage) makes
upstream conflict with patch 0004 and checks that the repair carries it over.

To change the port: work on the `linux-port` branch in `upstream/`, then
`git -C upstream format-patch --no-signature --zero-commit -o ../patches main..linux-port`
(clear `patches/` first) and run `zephium-linux update --force`.

## Testing by hand

`tools/rig` is a headless sway with a scratch profile and its own session
bus. `rig zephium [binary]`, `rig open URL`, `rig shot file.png`,
`rig move X Y` / `click` / `key ctrl+t`, `rig down`. Nothing in it reaches the
real desktop, profile or keyring. `tools/test-only/webkit-version-shim.so`
reports WebKitGTK 2.54.0 so startup can be tested on an older system; it is
only ever used there, never for browsing.
