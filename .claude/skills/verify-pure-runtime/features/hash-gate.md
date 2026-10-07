# Executable hash gate

The runtime only runs the executable the generated code was produced from. Any other file is rejected before it is loaded into guest memory, with a message naming both hashes, and the window stays open showing nothing.

## Sub-features

- `hash-reject` rejects a file whose SHA-256 differs from the recorded one.
- `hash-missing` reports an unreadable or missing path.

## How to get to it (user POV)

- `pure-runtime <path>` (or `cargo run --features recompiled -- <path>`) with any file other than the user's `default.xex`.

## Driving it with run.sh

Preconditions:

- Doctor reports the binary was built WITH the recompiled feature (the game-free build never reads the file).

- **Wrong file.** Run `bad=$(mktemp /tmp/not-pure.XXXXXX.xex); head -c 4096 /dev/urandom > "$bad"; .claude/skills/verify-pure-runtime/scripts/run.sh hash-gate "$bad"; rm -f "$bad"`. `summary.txt` stop line reads `<path> has SHA-256 <hash>; the generated code was produced from 83c52e15…f33c`.
- **Nothing loaded.** `grep -c 'entry point\|import:' runtime.log` prints `0`.
- **Missing file.** Run `.claude/skills/verify-pure-runtime/scripts/run.sh hash-missing /tmp/does-not-exist.xex`. The stop line names the path and the read error, and no `entry point` line appears.

## Gotchas

- Never prove this with a modified copy of the real executable; random bytes are enough and leave nothing game-derived behind.
- Delete the scratch file afterwards; it lives outside the evidence directory on purpose.
