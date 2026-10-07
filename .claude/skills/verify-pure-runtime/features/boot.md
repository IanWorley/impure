# Boot to first import

Given the user's own `default.xex`, the runtime opens the `Pure (static recompilation)` window, loads the image into guest memory, gives each kernel data export a zero-filled block, and runs the game's entry point on a guest thread. Every kernel/Xam import call is logged with r3–r6 and its caller. An unimplemented import stops the guest; implemented imports let it run further. Today it stops at `NtAllocateVirtualMemory` called from `0x829c60a8`.

## Sub-features

- `boot-load` loads the image and resolves 13 variable imports (`import: variable <Name> -> 0x… (zero-filled)`).
- `boot-entry` starts the guest at `0x82254d60` with a stack and PCR (`entry point 0x82254d60, stack top …, PCR …`).
- `boot-import-log` logs each import call with arguments and caller.
- `boot-stop` stops the guest at the first unimplemented import and keeps the window alive.
- `boot-exit-on-stop` closes the app when the guest stops if `--exit-on-stop` is passed.

## How to get to it (user POV)

- `cargo run --features recompiled -- ../originals/disc/default.xex` from `runtime/`.
- The built binary directly: `runtime/target/debug/pure-runtime originals/disc/default.xex [--exit-on-stop]`.

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready` and reports the binary was built WITH the recompiled feature.

- **Boot.** Run `.claude/skills/verify-pure-runtime/scripts/run.sh boot originals/disc/default.xex`. `summary.txt` shows `outcome: guest stopped`, `window: present`, `runtime exit: still running (stopped by harness)`.
- **Load and entry.** `grep -c 'import: variable' runtime.log` prints `13`; `grep 'entry point' runtime.log` shows `0x82254d60`.
- **Import and stop.** `grep -E 'import: [A-Za-z]+\(|guest stopped' runtime.log` shows the call with arguments and the matching `guest stopped: unimplemented import <Name> called from 0x…` line. Today: `NtAllocateVirtualMemory(r3=0x7010cb50, r4=0x7010cc34, r5=0x60002000, r6=0x4) from 0x829c60a8`.
- **Window rendered.** `identify -verbose screenshot.png | grep -A2 Histogram` shows the screen is `#0D0D14`, the runtime's clear colour.
- **Exit on stop.** Run `.claude/skills/verify-pure-runtime/scripts/run.sh exit-on-stop originals/disc/default.xex --exit-on-stop`. `summary.txt` shows `runtime exit: 0` and the same stop line.
- **After implementing an import.** Repeat **Boot** and quote the old and new stop lines; the implemented import's log line must appear before the new stop.

## Gotchas

- A binary older than the source proves nothing; doctor flags it. A relink takes about 2 minutes, a cold build over an hour.
- Building the game-free runtime into the default target directory overwrites this binary. Use a separate `CARGO_TARGET_DIR`.
- With `--exit-on-stop`, the window is usually gone before the screenshot, so `window: absent` is expected there.
- The guest thread parks rather than exiting; the process never exits on its own without `--exit-on-stop`.
- Addresses in the log are tied to this exact executable hash.
