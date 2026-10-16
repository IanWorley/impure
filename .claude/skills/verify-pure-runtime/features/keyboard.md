# On-screen keyboard

When the game asks the player to type something, as when naming an ATV in the World Tour garage, it calls `XamShowKeyboardUI` and polls the call's overlapped block each frame. The runtime leaves the call pending and the window shows the game's prompt in a dialog over the game: the player types on the host keyboard (or an input script presses keys), Enter accepts the text and Escape cancels. Accepting writes the text into the game's buffer and completes the call with `ERROR_SUCCESS`; cancelling completes it with `ERROR_CANCELLED` and leaves the buffer alone. The game's threads keep running throughout. While the dialog is up the game's controller is at rest, and keys pressed into the dialog stay hidden from the controller until they are released, so the Enter that accepts does not also press A in the game.

## Sub-features

- `keyboard-show` opens the dialog with the game's title, description and default text (`keyboard: shown: "ATV Name" "Choose a Name for Your ATV" (default "Fire Mantis", up to 31 characters)`).
- `keyboard-accept` returns the typed text to the game (`keyboard: accepted "wild"`); the garage's name panel shows it, and the next keyboard opens with it as the default.
- `keyboard-cancel` closes the dialog without changing the name (`keyboard: cancelled`).
- `keyboard-input-isolation` holds the controller at rest while the dialog is up and keeps the keys pressed into it from reaching the game after it closes.
- `keyboard-script-typing` lets an input script type: letter keys type their lowercase letter, Space a space, Backspace erases, Enter accepts and Escape cancels. Scripts can only name bound keys, so they type with the bound letters (W A S D I J K L X Y Q E).

## How to get to it (user POV)

- World Tour with a ride saved: on the event map press Y (Garage), then A (Modify Your Ride), go down to the last part, Name ("Name your ATV."), and press A. Type, then Enter or Escape.
- With no ride saved yet, World Tour opens the garage's build first; save a ride (see the preconditions below) and the route above applies.

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready`. Use a fresh `PURE_DATA_DIR` holding a copy of the built game library (see [saves.md](./saves.md)) and a save with a ride in it. Either copy in a `content/` directory that has one, or make one in two runs: [`inputs/to-tutorial.txt`](../inputs/to-tutorial.txt) creates the save (as in saves.md's "Create and write"), then [`inputs/to-world-tour.txt`](../inputs/to-world-tour.txt) saves a ride. Skip the first run and the save dialogs shift `to-world-tour.txt`'s presses, so it never leaves the tutorial.

- **Name the ATV (any host).** `VERIFY_RUST_LOG=info,controller=debug VERIFY_TIMEOUT_SECONDS=900 .claude/skills/verify-pure-runtime/scripts/run.sh keyboard originals/disc/default.xex --input-script .claude/skills/verify-pure-runtime/inputs/to-garage-name.txt`. The script exits itself at frame 8,422: `summary.txt` shows `runtime exit: 0` and an empty `stop line:`. `runtime.log` shows `keyboard: shown` (default `"Fire Mantis"` for the ride in the user's save), `keyboard: accepted "wild"`, `keyboard: shown` again with default `"wild"`, then `keyboard: cancelled`. `k-name.png` shows the Name part selected with the old name in the panel, and `k-accepted.png` and `k-cancelled.png` show `wild`.
- **Input isolation.** In the same log, from the A press that opens the keyboard (`controller: … buttons: 4096`, just before `keyboard: shown`) to `keyboard: accepted`, the only other `controller:` line is the return to rest (`buttons: 0`): the Backspaces (B) and W I L D (trigger, stick) never reach the game. The same holds from the second press to `keyboard: cancelled`. No `controller:` line with `buttons: 4096` follows the accepting Enter until the script's next Enter.
- **Before the fix.** A runtime without the import stops at the same press with `stop line: unimplemented import XamShowKeyboardUI called from 0x829154bc`.

## Gotchas

- The dialog is drawn by the window, not the game, so frame captures (and `screenshot` steps) show the game's screen under it, not the dialog. Prove it opened and closed from the `keyboard:` lines.
- The garage's part list does not wrap: ArrowUp from Frame stays on Frame. The script presses ArrowDown 40 times, which is more than the list has, to stop on Name.
- Renaming alone does not write the save; the game saves when the ride is saved on leaving the garage.
- The dialog takes host keyboard text only. A physical gamepad cannot type, accept or cancel it.
