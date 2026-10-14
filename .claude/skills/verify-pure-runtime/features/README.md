# Pure runtime verification map

This directory is the maintained source for verifying the user-facing behaviour of the Pure static-recompilation project. Read this index, run `scripts/doctor.sh`, then use the matching feature file as the recipe. Paths are relative to the repo root unless noted.

## Baseline preconditions

- `originals/disc/default.xex` is the user's own copy with SHA-256 `83c52e15…f33c` (doctor checks it).
- XenonRecomp is built in `local/tools/XenonRecomp/build-release` and `clang++` is available, so the runtime can build the game library (doctor checks both and reports whether a library is built).
- The runtime binary was built after the last source change (doctor checks freshness); point `PURE_RUNTIME_BINARY` at a release build for anything past the start-up screens.
- Never drive a runtime or X display that this verification run did not create, and never write into the user's data directory's `games/` or saves except through the runtime itself.

## Driving conventions

- Run the window app only through `.claude/skills/verify-pure-runtime/scripts/run.sh <label> [args]`, from the repo root.
- Run CLI features directly, capturing the command, stdout, stderr and exit code into `local/verification/<UTC timestamp>-<label>/`.
- Never write to `originals/`. Use a scratch `PURE_DATA_DIR` for anything that builds or saves, and keep evidence in `local/verification/…`.
- Mutations of Git state (staging probe files) happen only in a throwaway clone under `/tmp`.

## Proof and skip reporting

- Runtime proof is the run directory: `summary.txt`, `runtime.log`, and `screenshot.png` (Linux) or frame captures from an input script (any host). Quote the outcome and stop line, and the frames presented for main-loop runs.
- CLI proof is the command, its output, and its exit code, plus a read-only check of any file it was meant to write or refuse to write.
- Record the feature ID and the commit (dirty or not) with every artifact.
- If a precondition is unmet (no game copy, no XenonRecomp build, no Clang), report the feature as not verified with the doctor output.
- Never quote generated C++, anything under the data directory's `games/`, or bytes from the executable in a proof.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible behaviour, then exactly four H2 sections in this order: `Sub-features`, `How to get to it (user POV)`, `Driving it with run.sh` (or `with the shell` for CLIs), `Gotchas`.

## Features

- [Boot to the main loop](./boot.md): the game boots, loads its assets and runs its main loop at 60 frames per second; covers stop lines and `--exit-on-stop`.
- [Keyboard input](./input.md): the host keyboard (or an `--input-script`) drives controller 0 through a binding table, and the game reacts.
- [Rendering](./rendering.md): shaders translated to WGSL, draws run through wgpu, and the game's start-up screens shown in the window.
- [Audio](./audio.md): with the `audio` feature, the game's XMA streams are decoded with FFmpeg and its mixed frames play on the default output device, and can be captured to a WAV file.
- [Save games](./saves.md): the game creates, writes and reloads its save package in the runtime's data directory.
- [Executable hash gate](./hash-gate.md): the runtime refuses any executable but the recorded one.
- [Launcher](./launcher.md): with no game, the window asks for one in the system's Open dialog, shows preparation and failures, and remembers the last game.
- [Game library](./game-library.md): the runtime copies the disc out of an image and builds the game's code into a cached library on the user's machine, then loads it.
- [Content guard](./content-guard.md): `tools/check-no-game-content.sh` blocks game data and generated code from Git.
- [Release archive](./release-archive.md): `tools/package-release.sh` packages the runtime with its SDK, bundled FFmpeg and README, and the unpacked archive builds the game library and plays sound with no checkout in reach.
