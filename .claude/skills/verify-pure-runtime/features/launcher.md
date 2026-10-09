# Launcher

Started without a game (and with none remembered, or with `--choose`), the runtime opens its window on a launcher: the title, a short explanation, and an **Open Pure disc image or default.xex…** button that shows the system's Open dialog (filtered to `.iso` and `.xex`). While a game is prepared it shows each stage, with a progress bar for counted ones (copying the disc, compiling). A failure (wrong copy, no compiler, a build error) is shown in the window with the button to try another file. Once the game runs the launcher hides; if the game stops, the reason is shown over its last frame. The runtime remembers the last game that started and opens it directly next time.

## Sub-features

- `launcher-window` opens the `Pure (static recompilation)` window on the launcher, with no game named or remembered, or with `--choose`.
- `launcher-dialog` opens the system's Open dialog from the button and prepares the chosen file.
- `launcher-remembered` opens the last game that started when run with no argument.
- `launcher-failure` shows a preparation failure in the window and logs it as the stop line.
- `launcher-usage` prints the usage line and exits with code 2 for an unknown flag or a second game.

## How to get to it (user POV)

- `pure-runtime` with no arguments (a release's executable, or `cargo run --release` from `runtime/`).
- `pure-runtime --choose`.

## Driving it with run.sh

Preconditions:

- A release build (`cargo build --release --features audio`) and `PURE_RUNTIME_BINARY` pointing at it.
- A scratch data directory, so the user's remembered game is not used or changed: `export PURE_DATA_DIR=$(mktemp -d /tmp/pure-launcher.XXXXXX)`.

- **Window.** Run `VERIFY_RUN_SECONDS=8 .claude/skills/verify-pure-runtime/scripts/run.sh launcher --choose`. `summary.txt` shows `outcome: still running after 8s` and `window: present`; `runtime.log` has no `ERROR` or `prepare` line.
- **Remembered game.** After any run that started the game with this `PURE_DATA_DIR`, `cat "$PURE_DATA_DIR/last-game"` names its `default.xex`. Run `.claude/skills/verify-pure-runtime/scripts/run.sh remembered --input-script local/verification/scripts/movie.txt` with no game argument: the log shows `prepare: Checking your copy of the game` and then `entry point 0x82254d60`.
- **Failure.** See the hash gate (`hash-gate.md`): each refusal there is a launcher failure, with `window: present`.
- **Usage.** Run `"$PURE_RUNTIME_BINARY" --bogus; echo "exit $?"`: the usage line on stderr, exit `2`.
- **Dialog.** The click itself needs a person (or accessibility permission this session lacks): ask the user to click the button and choose their disc image, and confirm from `runtime.log` that `prepare: Reading your disc image` follows. Everything after the click is the same path `pure-runtime <image>` takes, which `game-library.md` proves.

## Gotchas

- Bevy's window screenshots are black on macOS and OS screenshots need a permission, so the launcher's look cannot be captured here; frame captures show only the game's front buffer.
- Remove the scratch `PURE_DATA_DIR` afterwards (it is not evidence), but only one you created.
