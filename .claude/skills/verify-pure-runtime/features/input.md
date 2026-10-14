# Keyboard input as controller 0

The host keyboard and the first connected physical gamepad together drive the game's first controller. Each frame the runtime maps held keys to an Xbox 360 gamepad state through one binding table (`runtime/src/controller.rs`): W and S are the right and left triggers (accelerate and brake), A and D steer the left stick, I/J/K/L the right stick, the arrow keys are the D-pad, Enter or Space is A, Backspace B, X and Y their buttons, Q and E the bumpers, Escape Start and Tab Back. The game reads it through `XamInputGetState`, whose packet number changes whenever the state does. A physical gamepad's buttons map by position (south is A, east B, and so on), its analog triggers and sticks pass through, and the keyboard's input is added to it. Gamepad support comes from Bevy's gilrs backend: always on Windows and macOS, and on Linux with `--features linux-gamepads` (needs `libudev-dev`). Controllers 1-3 report not connected.

## Sub-features

- `input-bindings` maps held keys to buttons, triggers and stick axes; opposite directions cancel.
- `input-packet-number` advances the XInput packet number only when the state changes.
- `input-guest-reaction` the game acts on the input (pressing A in the start menu moves it on).
- `input-gamepad` maps a physical gamepad to controller 0 and combines it with the keyboard.
- `input-script` plays back `--input-script` steps through the same bindings, keyed on presented frames, holding a press for at least one poll; its `screenshot` steps capture frames and its `exit` step closes the app.

## How to get to it (user POV)

- Focus the `Pure (static recompilation)` window and press the bound keys while the game runs.
- Or pass `--input-script <file>` (format in `runtime/src/input_script.rs` and the skill's "Input scripts and frame captures" section).

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready`. Only the by-hand key recipe needs `xdotool` (Linux; `sudo apt-get install xdotool`).

- **Bindings.** `cargo test controller` in `runtime/` covers the keyboard table, the gamepad mapping, combining the two, and the packet number.
- **Script parsing.** `cargo test input_script` covers frame order, steps coming due once, a late poll still holding a press, and rejected lines.
- **Scripted input reaches the guest (any host).** `VERIFY_RUST_LOG=info,controller=debug,input_script=info VERIFY_RUN_SECONDS=120 .claude/skills/verify-pure-runtime/scripts/run.sh input-script originals/disc/default.xex --input-script .claude/skills/verify-pure-runtime/inputs/to-tutorial.txt`. `runtime.log` shows `input_script: frame N: down Enter`, then `controller: Gamepad { buttons: 4096, … }`, then the release; the frame captures listed in `summary.txt` show the start-up screens moving on (warning, rating, loading screen with the logo, save dialog). A script naming an unbound key (`1 down F13`) makes the runtime exit 2 at start-up, naming the line.
- **Gamepad.** Xvfb has no gamepads, so a run can only show the mapping's unit coverage. On a desktop with a pad, `VERIFY_RUST_LOG=info,controller=debug` logs `controller: Gamepad { … }` as its buttons change.
- **Key reaches the guest.** Start a run in the background with controller logging: `VERIFY_RUST_LOG=info,controller=debug VERIFY_RUN_SECONDS=40 .claude/skills/verify-pure-runtime/scripts/run.sh input originals/disc/default.xex &`. After about 15 seconds, read the display from the run's `summary.txt` (`display: :1NN`), then: `W=$(DISPLAY=:1NN xdotool search --name 'Pure \(static' | head -1); DISPLAY=:1NN xdotool windowfocus --sync $W; DISPLAY=:1NN xdotool keydown Return; sleep 0.5; DISPLAY=:1NN xdotool keyup Return`. `runtime.log` shows `controller: Gamepad { buttons: 4096, … }` (4096 is A) and then the state back at rest.
- **Game reaction.** The same run must end `still running after 40s` with no stop line: the game takes a new path when A is pressed (it crashed there before the switch-index fix in `tools/patches/xenonrecomp.patch`). The longer sequence in [saves.md](./saves.md) drives it on into its save code.

## Gotchas

- Without a window manager, `xdotool windowactivate` fails; `windowfocus --sync` works.
- `xdotool key` sends press and release back to back, which a 60 Hz game can miss; hold the key with `keydown`, a short sleep, and `keyup`.
- While the game's on-screen keyboard is up, keys type into its dialog instead of driving the controller (see [keyboard.md](./keyboard.md)).
- The run's display is only known once `summary.txt` exists; never send keys to a display this run did not create.
