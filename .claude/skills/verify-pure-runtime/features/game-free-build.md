# Game-free build

Without the `recompiled` feature the runtime contains no game code: it opens the window, reports that it did not load the executable, and does nothing else. This is the only build CI produces, on six targets.

## Sub-features

- `free-window` opens the `Pure (static recompilation)` window and renders the clear colour.
- `free-notice` reports `built without the \`recompiled\` feature, so <path> was not loaded`.
- `free-usage` prints the usage line and exits with code 2 when called without an executable.

## How to get to it (user POV)

- `cargo run -- <any path>` from `runtime/`.
- `cargo build` / `cargo test` (CI does this).

## Driving it with run.sh

Preconditions:

- Build into a separate directory so the recompiled binary survives: from `runtime/`, `CARGO_TARGET_DIR=../local/verification/target-gamefree cargo build --locked`.

- **Window and notice.** Run `PURE_RUNTIME_BINARY=local/verification/target-gamefree/debug/pure-runtime .claude/skills/verify-pure-runtime/scripts/run.sh game-free originals/disc/default.xex`. `summary.txt` shows `window: present` and the `built without the \`recompiled\` feature` stop line.
- **No game code linked.** `grep -ca pure_import_call local/verification/target-gamefree/debug/pure-runtime` prints `0`.
- **Usage.** Run `local/verification/target-gamefree/debug/pure-runtime; echo "exit $?"`. stderr is `usage: pure-runtime <path/to/default.xex> [--exit-on-stop]` and the exit code is `2`.

## Gotchas

- The first build of this target directory compiles all of Bevy, so it takes several minutes.
- Delete `local/verification/target-gamefree` when finished; it is a cache, not evidence.
- Cross-target behaviour (macOS, Windows, arm64) is proven by CI, not here; locally only `cargo check --target <triple>` is possible.
