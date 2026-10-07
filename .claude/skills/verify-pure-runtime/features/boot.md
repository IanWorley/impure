# Boot to the main loop

Given the user's own `default.xex`, the runtime opens the `Pure (static recompilation)` window, loads the image into guest memory, gives each kernel data export a zero-filled block, and runs the game's entry point on a guest thread. The kernel and Xam imports the game uses are implemented well enough that it boots, loads its assets from the disc directory, creates its worker threads, and settles into its main loop presenting 1280x720 frames, paced by the stand-in GPU's vertical blank interrupts and, once it draws, by the host GPU. What it draws is covered in [rendering.md](./rendering.md); audio is silent. An import the runtime does not implement still stops the calling guest thread with a stop line.

## Sub-features

- `boot-load` loads the image and resolves 13 variable imports (`import: variable <Name> -> 0x… (zero-filled)`).
- `boot-entry` starts the guest at `0x82254d60` with a stack and PCR (`entry point 0x82254d60, stack top …, PCR …`).
- `boot-main-loop` keeps running with no stop line, logging `first frame presented (1280x720)` and then `<N> frames presented` every 600 frames.
- `boot-frame-pacing` presents frames at 60 per second while loading, before the first draw reaches the host GPU.
- `boot-shader-translation` translates every shader the game loads to WGSL that passes naga validation; the progress line counts them (`<S> distinct shaders loaded, <V> valid as WGSL`).
- `boot-disc-files` reads the game's files (fonts, `default.global_regs`, …) from the directory holding `default.xex`.
- `boot-threads` creates the game's worker threads, including one it names `LoaderThread`.
- `boot-stop` stops a guest thread at an unimplemented import (or an indirect call to an address with no recompiled function) and keeps the window alive.
- `boot-exit-on-stop` closes the app when the guest stops if `--exit-on-stop` is passed.

## How to get to it (user POV)

- `cargo run --features recompiled -- ../originals/disc/default.xex` from `runtime/`.
- The built binary directly: `runtime/target/debug/pure-runtime originals/disc/default.xex [--exit-on-stop]`.

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready` and reports the binary was built WITH the recompiled feature.

- **Main loop.** Run `VERIFY_RUN_SECONDS=60 .claude/skills/verify-pure-runtime/scripts/run.sh boot originals/disc/default.xex`. `summary.txt` shows `outcome: still running after 60s`, `window: present` and an empty `stop line:`. Under llvmpipe the frame count depends on how fast it renders (see [rendering.md](./rendering.md)); on a real GPU it is around 3600.
- **Load and entry.** `grep -c 'import: variable' runtime.log` prints `13`; `grep 'entry point' runtime.log` shows `0x82254d60`.
- **Frame pacing.** With `RUST_LOG=info,video=debug`, the `frame <n> presented` lines before `renderer: first guest frame presented` arrive 1/60 s apart; quote two timestamps sixty frames apart. Afterwards, under llvmpipe, they slow to the software GPU's pace.
- **Shader translation.** Add `RUST_LOG=info,shader=debug` to a 30-second run. The two counts in the last `frames presented` line must be equal. Each `shader <hash>: valid WGSL, not translated: {…}` line lists what that shader still lacks; today that is only `texture fetch (returns white)`. A `invalid WGSL` warning carries naga's error. `cargo test translate` in `runtime/` covers every ALU opcode, the vertex formats and the control flow with synthetic shaders.
- **Disc files.** Add `RUST_LOG=info,file=debug` to the run. `grep 'file: read' runtime.log` lists reads such as `"D:\\fonts\\Debug.textures"`.
- **Threads.** `grep 'thread:' runtime.log` shows the created threads and `is named "LoaderThread"`.
- **Window rendered.** `convert screenshot.png -format %c histogram:info:-` shows opaque black: the game's own (black) intro video frame, not the runtime's clear colour `#0D0D14`. [rendering.md](./rendering.md) drives it to visible screens.
- **Stop and exit on stop.** The game no longer stops by itself, so prove both through the hash gate: run `.claude/skills/verify-pure-runtime/scripts/run.sh exit-on-stop /tmp/<wrong file> --exit-on-stop` (see [hash-gate.md](./hash-gate.md)); `summary.txt` shows `outcome: guest stopped` and `runtime exit: 0`.
- **After implementing an import.** If the change was driven by a stop line, quote the old stop line and show the run no longer stops there (a later stop line, or `still running`). For behaviour changes, quote the relevant log lines before and after.

## Gotchas

- A binary older than the source proves nothing; doctor flags it. A relink takes about 2 minutes. Generated objects are cached per file, so regenerating after a recompiler change rebuilds only the changed files.
- Building the game-free runtime into the default target directory overwrites this binary. Use a separate `CARGO_TARGET_DIR`.
- Without `VERIFY_RUN_SECONDS`, a healthy run now ends as `timeout after 120s`: the harness's default mode waits for a stop that never comes.
- Several guest threads run at once, so when something does stop the guest, which import stops first can differ between runs. Run twice before concluding.
- Debug logging (`RUST_LOG=info,import=debug`) logs every import call, including about 100 critical section calls per frame; keep runs short with it.
- With `--exit-on-stop`, the window is usually gone before the screenshot, so `window: absent` is expected there.
- Addresses in the log are tied to this exact executable hash.
- `shader=trace` logs each shader's disassembly and WGSL. Both are derived from the game, so they can be read locally but never quoted or committed.
