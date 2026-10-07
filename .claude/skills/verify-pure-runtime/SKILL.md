---
name: verify-pure-runtime
description: Drive the Pure (Xbox 360) static-recompilation runtime — the Rust/Bevy desktop window that runs the recompiled default.xex — headlessly under Xvfb, and capture proof (runtime log, screenshot, summary). Also covers the hash-gated tools/recompile.sh and the no-game-content guard. Use when you need to prove runtime behaviour end to end, e.g. after implementing a kernel import, changing guest loading, or touching build.rs.
---

# Verify the Pure runtime

The user-facing surface is `pure-runtime <path/to/default.xex> [--exit-on-stop]`: a Bevy window titled `Pure (static recompilation)` that loads the user's own executable and runs the game on guest threads against the runtime's kernel, Xam, GPU-command, audio and file imports. Today the game boots into its main loop and presents frames at 60 per second; nothing is drawn yet and audio is silent. An unimplemented import still stops the calling guest thread with a stop line, so new work is proven by the main loop surviving (or a stop line moving later). Secondary surfaces are `tools/recompile.sh` and `tools/check-no-game-content.sh` (plain CLIs, see the feature map).

**Legal boundary, non-negotiable:** evidence goes only to `local/verification/` (ignored by Git). Never commit, upload, or paste into a PR anything from `originals/`, `local/recomp/`, or a binary built with `recompiled`. Logs and screenshots may be quoted in conversation; the generated C++ may not.

## Launch

The app is short-lived per run, so there is no server to keep alive: build once, then each drive starts its own private X display and runtime.

Build (from `runtime/`):

```sh
cargo build --features recompiled        # the real thing; ~2 min relink with cached objects, ~70 min cold
```

Prerequisites the build needs: `originals/disc/default.xex` with the recorded SHA-256 and generated C++ in `local/recomp/ppc` (from `tools/recompile.sh`). Never set `CI` locally; with `CI` set, the build skips the C++ and fails to link on purpose.

To verify the game-free build, use a separate target directory so it does not overwrite the recompiled binary (which would cost a relink): `CARGO_TARGET_DIR=../local/verification/target-gamefree cargo build`, then point the harness at it with `PURE_RUNTIME_BINARY`.

Ready signal: by default the harness waits for the log line `guest stopped:` (a hash rejection, the game-free notice, or an unimplemented import). The game itself now keeps running, so drive it with `VERIFY_RUN_SECONDS=<n>`: the harness runs that long, and a stop or exit before then is the outcome. Teardown is automatic: the harness kills only the Xvfb and runtime it started, on any exit.

## Doctor

Run first, and whenever anything looks off. Read-only.

```sh
.claude/skills/verify-pure-runtime/scripts/doctor.sh
```

It checks: the game executable exists and its hash matches; generated C++ exists; the runtime binary exists, whether it has the `recompiled` feature, and that it is newer than `runtime/src`, `runtime/cpp` and `build.rs` (a stale binary proves nothing — rebuild); Xvfb, ImageMagick `import`, `xwininfo` and Mesa lavapipe are installed; and it lists any harness processes still alive from earlier runs. It prints `ready` and exits 0 when the instance is worth driving.

Missing tools on Ubuntu: `sudo apt-get install xvfb x11-utils imagemagick mesa-vulkan-drivers libgl1-mesa-dri libxcursor1 libxrandr2 libxi6 libxkbcommon-x11-0 libx11-xcb1`.

## Drive

```sh
.claude/skills/verify-pure-runtime/scripts/run.sh <label> [runtime arguments...]
# e.g.
.claude/skills/verify-pure-runtime/scripts/run.sh boot originals/disc/default.xex
```

Run from the repo root (relative runtime arguments resolve there). It:

1. Picks the first free display in `:100`–`:199`, starts `Xvfb` at 1280x720, waits for `xdpyinfo`.
2. Starts the binary with `VK_ICD_FILENAMES` (lavapipe), `WGPU_BACKEND=vulkan` and `NO_COLOR=1`.
3. Waits for `guest stopped:` or the runtime exiting: up to `VERIFY_TIMEOUT_SECONDS` (default 120), or exactly `VERIFY_RUN_SECONDS` when set.
4. Waits 3 s, checks the window by title with `xwininfo`, screenshots the root window.
5. Writes `summary.txt` and prints it, then tears down.

Stable handles to assert on (all in `runtime.log`):

- `pure_runtime::guest: entry point 0x82254d60, stack top …, PCR …` — guest thread started.
- `import: variable <Name> -> 0x…(zero-filled)` — one per kernel data export (13 today).
- `import: <Name>(r3=…, r4=…, r5=…, r6=…) from 0x…` — an unimplemented import call with its arguments and caller, logged just before its stop line.
- `pure_runtime::status: guest stopped: <reason>` (`unimplemented import <Name> called from 0x…` for an import) — the first stop reason; it is also echoed in `summary.txt` as `stop line:`.
- `video: first frame presented (1280x720)` and `video: <N> frames presented` every 600 frames — echoed in `summary.txt` as `frames presented:`.
- `thread: created 0x…`, `thread: 0x… is named "…"`, `audio: client …`, `video: ring buffer …`, `user: profile settings requested: …` — subsystem milestones at info level.

Implemented import calls are logged at debug level under `import` (with the calling guest thread); file opens and reads under `file`, GPU packets under `gpu` (`trace` for every packet and register write). Enable them with `RUST_LOG`, for example `RUST_LOG=info,import=debug,file=debug`, which `run.sh` passes through.
- `guest stopped: indirect call to 0x…, which has no recompiled function (return address 0x…)` — the guest called a code address with no recompiled function.

Isolation: concurrent runs are safe. Each gets its own display, process, and evidence directory; guest memory is a private per-process reservation; the game files are only read. Do not drive a runtime you did not start (e.g. one the user has open on their desktop).

`--exit-on-stop` makes the app quit as soon as the guest stops; the harness then usually reports `window: absent` and no useful screenshot. Omit it when you need visual proof; pass it when you are proving the flag itself.

## Evidence

Each run writes `local/verification/<UTC timestamp>-<label>/`:

- `summary.txt` — label, binary, arguments, display, commit (marked dirty if the tree is), outcome, window present/absent, runtime exit status, stop reason.
- `runtime.log` — full stdout/stderr of the runtime.
- `screenshot.png` — the whole display after the stop. With no window manager the window fills the screen; for the recompiled game it shows the game's last presented frame (black during the intro video), and for the game-free build the runtime's clear colour `#0D0D14`. Check with `convert screenshot.png -format %c histogram:info:-`. It is derived from the game, so never commit it.
- `xvfb.log`.

Proof standards:

- Drive the real binary with the real executable. Do not call `guest::start` from a test, add test-only flags, or patch the guest to reach a state.
- Capture the action and the resulting state. For an import you implemented, prove it ran (its log line), then prove what it changed: the guest gets past it, so the stop line moves to a later import or caller. Quote both the old and new stop line.
- Check side effects too, not just the window. The runtime currently writes no files; if a change makes it write any (saves, caches), list them in the proof.
- `runtime exit: still running (stopped by harness)` is normal: the game runs (or a stopped guest thread parks) until the harness kills it.
- Record the commit from `summary.txt`. A dirty-tree run proves the working tree, not a commit.

## Cleanup

`run.sh` tears down its own Xvfb and runtime through an EXIT trap, by PID, including on Ctrl-C or failure. It removes `harness.pids` when done; a leftover `harness.pids` means teardown did not run, and `doctor.sh` lists those PIDs. Kill those specific PIDs only if they are your own run's. Never `pkill` by name: that can kill the user's display or other agents' runs (and a `pkill -f` pattern can match your own shell).

Evidence is never deleted by cleanup. Remove old run directories under `local/verification/` only when the user asks. If you built a game-free binary in `local/verification/target-gamefree`, delete that directory when finished (it is a build cache, not evidence). Remove any scratch inputs you created (e.g. a fake `.xex` in `/tmp`).

## Feature map

[`features/README.md`](features/README.md) indexes the user-facing features and the recipe for each. A verification is complete only when it covers every entry point that file lists for the feature you changed.
