---
name: verify-pure-runtime
description: Drive the Pure (Xbox 360) static-recompilation runtime — the Rust/Bevy desktop window that builds the game library from the user's own disc image or default.xex and runs it — headlessly under Xvfb on Linux or in a native Metal window on macOS, steer it with input scripts, and capture proof (runtime log, frame captures, summary). Also covers the launcher, the game library builder and the no-game-content guard. Use when you need to prove runtime behaviour end to end, e.g. after implementing a kernel import, changing guest loading, or touching the builder or runtime/cpp.
---

# Verify the Pure runtime

The user-facing surface is `pure-runtime [<disc image or default.xex>] [--choose] [--build-game] [--exit-on-stop] [--input-script <file>]`: a Bevy window titled `Pure (static recompilation)` that takes the user's own copy (or asks for it in its launcher), builds the game library from it on this machine the first time (see `features/game-library.md`), and runs the game on guest threads against the runtime's kernel, Xam, GPU-command, audio and file imports. The game boots, renders its menus and the tutorial, and saves; with the `audio` feature it also plays its sound. An unimplemented import still stops the calling guest thread with a stop line, so new work is proven by the main loop surviving (or a stop line moving later). Secondary surfaces are `--build-game` and `tools/check-no-game-content.sh` (plain CLIs, see the feature map).

**Legal boundary, non-negotiable:** evidence goes only to `local/verification/` (ignored by Git). Never commit, upload, or paste into a PR anything from `originals/`, `local/recomp*/`, or the data directory's `games/` (the disc copy, the translated code and the game library). Logs and screenshots may be quoted in conversation; the generated C++ may not.

## Launch

The app is short-lived per run, so there is no server to keep alive: build once, then each drive starts its own runtime (on Linux, on its own private X display; on macOS, as a normal window on the desktop, rendering through Metal).

Build (from `runtime/`):

```sh
cargo build --release --features audio   # seconds to a few minutes; point PURE_RUNTIME_BINARY at target/release/pure-runtime
target/release/pure-runtime ../originals/disc/default.xex --build-game   # the game library: ~10 min cold, instant warm
```

The runtime contains no game code; the game library lives in the data directory (`~/Library/Application Support/pure-runtime/games/<SHA-256>/` on macOS) and is built from `originals/disc/default.xex` (or the disc image) with the XenonRecomp build in `local/tools/XenonRecomp/build-release` and the system's Clang. Use a scratch `PURE_DATA_DIR` to test builds without touching the user's (copy their `content/` in for scripts that expect a save).

Ready signal: by default the harness waits for the log line `guest stopped:` (a refused copy, a failed build, or an unimplemented import). The game itself now keeps running, so drive it with `VERIFY_RUN_SECONDS=<n>`: the harness runs that long, and a stop or exit before then is the outcome (an input script's `exit` counts: `runtime exit: 0`). Teardown is automatic: the harness kills only the Xvfb and runtime it started, on any exit.

## Doctor

Run first, and whenever anything looks off. Read-only.

```sh
.claude/skills/verify-pure-runtime/scripts/doctor.sh
```

It checks: the game executable exists and its hash matches; XenonRecomp's tools are built and `clang++` is available; whether the game library is built (honouring `PURE_DATA_DIR`); the runtime binary exists and is newer than `runtime/src` and `Cargo.toml` (a stale binary proves nothing — rebuild); Xvfb, ImageMagick `import`, `xwininfo` and Mesa lavapipe are installed; and it lists any harness processes still alive from earlier runs. It prints `ready` and exits 0 when the instance is worth driving.

Missing tools on Ubuntu: `sudo apt-get install xvfb x11-utils imagemagick mesa-vulkan-drivers libgl1-mesa-dri libxcursor1 libxrandr2 libxi6 libxkbcommon-x11-0 libx11-xcb1`. On macOS the doctor only needs `swift` (Xcode's command line tools) for its window check.

## Input scripts and frame captures

`--input-script <file>` is how a run reaches a screen and proves it, on any host and with no window system automation: each line is `<frame> down <Key>`, `<frame> up <Key>`, `<frame> screenshot <name>.png`, `<frame> resolves <prefix>` or `<frame> exit`, keyed on the frames the game has presented (its own clock). Keys are the keyboard bindings' names (`Enter`, `Escape`, `KeyW`, `ArrowDown`; see `runtime/src/controller.rs`). A screenshot is a frame capture: the renderer saves the next presented front buffer as the window shows it, at 1280x720. `run.sh` copies the script into the evidence directory and runs it from there, so the captures land beside `runtime.log` and are listed in `summary.txt`. `resolves <prefix>` saves every resolve of the next whole frame, in order, as `<prefix>-<n>-<address>.png` (32-bit float ones as grey, with their range in the log): the render-to-texture passes the picture is built from, for finding which pass goes wrong (look at them as a contact sheet). That frame also logs each draw and resolve under `dump` (shaders, target, blend, alpha test, depth, culling, primitive, program control, surfaces, textures, and full-screen passes' pixel constants), to match a pass to the shaders that drew it. Each step is logged under `input_script`, each saved image as `renderer: saved …`. A malformed script stops the runtime at start-up with exit code 2 and the offending line.

[`inputs/to-tutorial.txt`](inputs/to-tutorial.txt) presses Return through the start-up screens from frame 900, the title screen and the save dialogs (creating a save when there is none), capturing a frame before each press; the tutorial's 3D world loads by about frame 3,600. Its Escape at frame 3,420 now lands in the tutorial and opens the pause menu (`shot-015`), which the next Return resumes. The tutorial then shows instructions that A dismisses and one that waits for a fresh pull of the throttle, at frames that depend on the frame rate, so from frame 3,610 the script holds W and, every 150 frames, eases off it, taps Return and pulls it again, capturing the ride (`ride-NNNN.png`) every 300 frames until it exits at frame 9,150. On an M4 the lap completes (`Complete a lap` gets its green star) and the `Preload` objective appears around frame 5,200.

[`inputs/to-race.txt`](inputs/to-race.txt) goes further: it quits the tutorial from its pause menu (which unloads a level, and needs `MmQueryAllocationSize`), takes Single Event from the main menu, accepts the default race, track, character and ATV, and holds the throttle in the race until it exits at frame 9,178. Run it like the tutorial script with `VERIFY_TIMEOUT_SECONDS=600`. On an M4 release build the main menu is up by `r-00.png` (its 3D background animating), the Alto Vista race is under way by `r-08.png` with the HUD's timer, position and lap counter, and the race runs at about 30 fps. [`inputs/to-world-tour.txt`](inputs/to-world-tour.txt) takes World Tour instead (`VERIFY_TIMEOUT_SECONDS=1000`): character select, the garage's part-by-part build (quick-built with X, then saved, which needs `XamContentGetCreator` and `NtSetInformationFile`), the World Tour map (Mount Garda), the ATV just built, and the race. [`inputs/to-freestyle-results.txt`](inputs/to-freestyle-results.txt) plays a Freestyle event to its end without steering (the fuel runs out), through the results table (`fe-16710.png` on an M4) and back out (`VERIFY_TIMEOUT_SECONDS=1000`). A rider drawn see-through for a second after a crash reset is the game's respawn ghosting, not a rendering fault. [`inputs/to-race-finish.txt`](inputs/to-race-finish.txt) finishes a whole three-lap race with the throttle alone, since the game resets a rider who leaves the track back onto it facing the right way, and shows the race results table (about 15 minutes, `VERIFY_TIMEOUT_SECONDS=1500`).

On macOS, this session's process may lack the screen-recording and accessibility permissions, so OS screenshots and synthetic key presses (the Linux recipes' `import` and `xdotool`) are unavailable there: use input scripts and frame captures. Bevy's own window screenshot API returns black frames on macOS; do not use it as evidence.

## Drive

```sh
.claude/skills/verify-pure-runtime/scripts/run.sh <label> [runtime arguments...]
# e.g.
.claude/skills/verify-pure-runtime/scripts/run.sh boot originals/disc/default.xex
```

Run from the repo root (relative runtime arguments resolve there). It:

1. Copies an `--input-script` argument's file into the evidence directory and passes the copy instead.
2. On Linux, picks the first free display in `:100`–`:199`, starts `Xvfb` at 1280x720, waits for `xdpyinfo`.
3. Starts the binary with `NO_COLOR=1` and `RUST_LOG` from `VERIFY_RUST_LOG` (default `info`), plus on Linux `VK_ICD_FILENAMES` (lavapipe) and `WGPU_BACKEND=vulkan`. On macOS it uses the real GPU through Metal.
4. Waits for `guest stopped:` or the runtime exiting: up to `VERIFY_TIMEOUT_SECONDS` (default 120), or exactly `VERIFY_RUN_SECONDS` when set.
5. Waits 3 s, then checks the window: on Linux by title with `xwininfo`, screenshotting the root window; on macOS by the runtime's process ID among on-screen windows (titles need a permission). macOS runs take no screenshot of their own: use frame captures.
6. Writes `summary.txt` (including the frame captures taken) and prints it, then tears down.

Stable handles to assert on (all in `runtime.log`):

- `pure_runtime::guest: entry point 0x82254d60, stack top …, PCR …` — guest thread started.
- `import: variable <Name> -> 0x… (zero-filled)` — one per kernel data export (13 today).
- `import: <Name>(r3=…, r4=…, r5=…, r6=…) from 0x…` — an unimplemented import call with its arguments and caller, logged just before its stop line.
- `pure_runtime::status: guest stopped: <reason>` (`unimplemented import <Name> called from 0x…` for an import) — the first stop reason; it is also echoed in `summary.txt` as `stop line:`.
- `video: first frame presented (1280x720)` and `video: <N> frames presented` every 600 frames — echoed in `summary.txt` as `frames presented:`.
- `thread: created 0x…`, `thread: 0x… is named "…"`, `audio: client …`, `video: ring buffer …`, `user: profile settings requested: …` — subsystem milestones at info level.

Implemented import calls are logged at debug level under `import` (with the calling guest thread); file opens and reads under `file`, GPU packets under `gpu` (`trace` for every packet and register write). Enable them with `RUST_LOG`, for example `VERIFY_RUST_LOG=info,import=debug,file=debug`, which `run.sh` passes through.
- Audio (`--features audio`): `PURE_AUDIO_CAPTURE=<file.wav>` records what plays (48 kHz stereo, 16-bit). Judge it without listening: RMS per second, and spectral flatness (real music and effects stay under about 0.3; decoding gone wrong is noise near 0.5 to 0.9 with energy around 10 kHz). `VERIFY_RUST_LOG=info,xma=debug` logs `a stream its context calls 1-channel decodes as 2-channel` as stereo streams are found.
- `xma: N passes decoded M frames in the last 10s`: audio decoding progress. Each playing 48 kHz stream needs about 94 frames a second; in the tutorial the total is about 1,100 to 1,400.
- On an Apple silicon Mac, an x86-64 runtime (`cargo build --release --target x86_64-apple-darwin`) builds and loads an x86-64 game library (its own objects, about 10 minutes cold) and runs under Rosetta at about 10 frames a second: a quick way to tell an arm64-specific bug from a general one. A rider drawn see-through for a moment is the game's respawn ghosting after a crash reset, which slow runs hit more often.
- `wait: thread 0x… has waited 5s from 0x… for any of [0x… Semaphore state 0]`: a wait with no timeout lasting five seconds. Idle worker threads log these normally; the main thread logging one (`0x70054000` in Pure) while frames stop is a hang, and the object and caller are where to start. `VERIFY_RUST_LOG=info,semaphore=debug` then shows who creates and releases that semaphore.
- `guest stopped: indirect call to 0x…, which has no recompiled function (return address 0x…)` — the guest called a code address with no recompiled function.

Isolation: concurrent runs are safe. Each gets its own display, process, and evidence directory; guest memory is a private per-process reservation; the game files are only read. Do not drive a runtime you did not start (e.g. one the user has open on their desktop).

`--exit-on-stop` makes the app quit as soon as the guest stops; the harness then usually reports `window: absent` and no useful screenshot. Omit it when you need visual proof; pass it when you are proving the flag itself.

## Evidence

Each run writes `local/verification/<UTC timestamp>-<label>/`:

- `summary.txt` — label, binary, arguments, display, commit (marked dirty if the tree is), outcome, window present/absent, runtime exit status, stop reason.
- `runtime.log` — full stdout/stderr of the runtime.
- `screenshot.png` — the whole display after the stop. With no window manager the window fills the screen; it shows the game's last presented frame (black during the intro video), or the launcher. Check with `convert screenshot.png -format %c histogram:info:-`. It is derived from the game, so never commit it.
- `xvfb.log`.

Proof standards:

- Drive the real binary with the real executable. Do not call `guest::start` from a test, add test-only flags, or patch the guest to reach a state.
- Capture the action and the resulting state. For an import you implemented, prove it ran (its log line), then prove what it changed: the guest gets past it, so the stop line moves to a later import or caller. Quote both the old and new stop line.
- Check side effects too, not just the window. The runtime writes saves (`content/`), the prepared game (`games/`) and `last-game` in the data directory; list any new files a change makes it write.
- `runtime exit: still running (stopped by harness)` is normal: the game runs (or a stopped guest thread parks) until the harness kills it.
- Record the commit from `summary.txt`. A dirty-tree run proves the working tree, not a commit.

## Cleanup

`run.sh` tears down its own Xvfb and runtime through an EXIT trap, by PID, including on Ctrl-C or failure. It removes `harness.pids` when done; a leftover `harness.pids` means teardown did not run, and `doctor.sh` lists those PIDs. Kill those specific PIDs only if they are your own run's. Never `pkill` by name: that can kill the user's display or other agents' runs (and a `pkill -f` pattern can match your own shell).

Evidence is never deleted by cleanup. Remove old run directories under `local/verification/` only when the user asks. Remove any scratch data directories and inputs you created (e.g. a fake `.xex` in `/tmp`); a scratch data directory holds game-derived files, never evidence.

## Feature map

[`features/README.md`](features/README.md) indexes the user-facing features and the recipe for each. A verification is complete only when it covers every entry point that file lists for the feature you changed.
