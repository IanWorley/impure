# Pure runtime verification map

This directory is the maintained source for verifying the user-facing behaviour of the Pure static-recompilation project. Read this index, run `scripts/doctor.sh`, then use the matching feature file as the recipe. Paths are relative to the repo root unless noted.

## Baseline preconditions

- `originals/disc/default.xex` is the user's own copy with SHA-256 `83c52e15…f33c` (doctor checks it).
- `local/recomp/ppc` holds generated C++ from `tools/recompile.sh`.
- `runtime/target/debug/pure-runtime` was built with `cargo build --features recompiled` after the last source change (doctor checks freshness).
- `CI` is not set in the shell.
- Never drive a runtime, X display, or `local/recomp` that this verification run did not create.

## Driving conventions

- Run the window app only through `.claude/skills/verify-pure-runtime/scripts/run.sh <label> [args]`, from the repo root.
- Run CLI features directly, capturing the command, stdout, stderr and exit code into `local/verification/<UTC timestamp>-<label>/`.
- Never write to `local/recomp` or `originals/`. Generate into `local/verification/…` instead.
- Mutations of Git state (staging probe files) happen only in a throwaway clone under `/tmp`.

## Proof and skip reporting

- Runtime proof is the run directory: `summary.txt`, `runtime.log`, `screenshot.png`. Quote the outcome and stop line, and the frames presented for main-loop runs.
- CLI proof is the command, its output, and its exit code, plus a read-only check of any file it was meant to write or refuse to write.
- Record the feature ID and the commit (dirty or not) with every artifact.
- If a precondition is unmet (no game copy, no generated C++), report the feature as not verified with the doctor output. Do not substitute the game-free build for the recompiled one.
- Never quote generated C++ or bytes from the executable in a proof.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible behaviour, then exactly four H2 sections in this order: `Sub-features`, `How to get to it (user POV)`, `Driving it with run.sh` (or `with the shell` for CLIs), `Gotchas`.

## Features

- [Boot to the main loop](./boot.md): the recompiled game boots, loads its assets and runs its main loop at 60 frames per second; covers stop lines and `--exit-on-stop`.
- [Keyboard input](./input.md): the host keyboard drives controller 0 through a binding table, and the game reacts.
- [Rendering](./rendering.md): shaders translated to WGSL, draws run through wgpu, and the game's start-up screens shown in the window.
- [Save games](./saves.md): the game creates, writes and reloads its save package in the runtime's data directory.
- [Executable hash gate](./hash-gate.md): the runtime refuses any executable but the recorded one.
- [Game-free build](./game-free-build.md): the runtime without `recompiled` opens the window and loads nothing.
- [Recompile script](./recompile-script.md): `tools/recompile.sh` generates the C++ and refuses bad inputs.
- [Content guard](./content-guard.md): `tools/check-no-game-content.sh` blocks game data and generated code from Git.
