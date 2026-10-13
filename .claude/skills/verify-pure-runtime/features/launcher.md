# Launcher

Started without a game on the command line, the runtime opens its window on a home screen, the place a player points it at their copy: the title, an **Open Pure disc image or default.xex...** button that shows the system's Open dialog (filtered to `.iso` and `.xex`), and, when a game was prepared before, its file and folder with a **Play** button. Hidden elements take no space. While a game is prepared it shows each stage, with a progress bar for counted ones (copying the disc, compiling). A failure (wrong copy, no compiler, a build error) is shown in the window with the button to try another file. Once the game runs the launcher hides; if the game stops, the reason is shown over its last frame. Once a game is prepared (by the window or `--build-game`), the runtime remembers what the player chose (the disc image itself, not the copy made from it) by absolute path; the home screen offers it, and starts it only on Play.

## Sub-features

- `launcher-window` opens the `Pure (static recompilation)` window on the home screen when no game is named on the command line; with nothing remembered (or `--choose`) it offers only Open.
- `launcher-dialog` opens the system's Open dialog from the button and prepares the chosen file.
- `launcher-remembered` offers the last game prepared, with Play, without starting it.
- `launcher-failure` shows a preparation failure in the window and logs it as the stop line.
- `launcher-usage` prints the usage line and exits with code 2 for an unknown flag, a second game, or `--build-game` with no game named and none remembered (or with `--choose`); with a remembered game, `--build-game` alone prepares that one.

## How to get to it (user POV)

- `pure-runtime` with no arguments (a release's executable, or `cargo run --release` from `runtime/`).
- `pure-runtime --choose`.

## Driving it with run.sh

Preconditions:

- A release build (`cargo build --release --features audio`) and `PURE_RUNTIME_BINARY` pointing at it.
- A scratch data directory, so the user's remembered game is not used or changed: `export PURE_DATA_DIR=$(mktemp -d /tmp/pure-launcher.XXXXXX)`.

- **Window.** Run `VERIFY_RUN_SECONDS=8 .claude/skills/verify-pure-runtime/scripts/run.sh launcher --choose`. `summary.txt` shows `outcome: still running after 8s` and `window: present`; `runtime.log` has no `ERROR` or `prepare` line.
- **Remembered game.** After any run that prepared the game with this `PURE_DATA_DIR` (`--build-game` is enough), `cat "$PURE_DATA_DIR/last-game"` names what was chosen, as an absolute path (the `.iso`, not the copied `default.xex`). Without the game, write any existing file's absolute path there instead, with no trailing newline (`printf '%s' "$PWD/originals/disc/default.xex" > "$PURE_DATA_DIR/last-game"`; `echo` adds one and the path no longer names a file): the home screen only checks that it is a file. Run the window again with no game argument and `VERIFY_RUN_SECONDS=8`: `still running after 8s` and no `prepare` or `entry point` line, because the home screen waits for Play.
- **Failure.** See the hash gate (`hash-gate.md`): each refusal there is a launcher failure, with `window: present`.
- **Usage.** Run `"$PURE_RUNTIME_BINARY" --bogus; echo "exit $?"`, then the same with `a.xex b.xex`: the usage line (`usage: pure-runtime [<disc image (.iso) or default.xex>] …`) on stderr, exit `2`. With `--build-game` alone and an empty data directory it first prints `--build-game needs a disc image or default.xex`.
- **Dialog.** The click itself needs a person (or accessibility permission this session lacks): ask the user to click the button and choose their disc image, and confirm from `runtime.log` that `prepare: Reading your disc image` follows (`prepare: Checking your copy of the game` for a `default.xex`). Everything after the click is the same path `pure-runtime <image>` takes, which `game-library.md` proves.

## Gotchas

- Bevy's window screenshots are black on macOS and OS screenshots need a permission, and frame captures show only the game's front buffer. To see the launcher, patch it temporarily to render offscreen: spawn a second `Camera2d` with `RenderTarget::from(image)` (a 1280x720 `Image::new_target_texture` with `COPY_SRC` added), insert `UiTargetCamera(camera)` on the `Screen` root, and after a couple of seconds spawn `Screenshot::image(image)` observed by `save_to_disk`. That captures the real layout and font; remove the patch afterwards.
- Remove the scratch `PURE_DATA_DIR` afterwards (it is not evidence), but only one you created.
