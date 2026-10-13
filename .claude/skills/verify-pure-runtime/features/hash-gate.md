# Executable hash gate

The runtime only runs the release of Pure it supports. Any other executable, or a file that is not an Xbox disc image, is rejected before anything is built or loaded, with a message in the window (and as the stop line) naming the file and, for an executable, both hashes; the window stays open on the launcher.

## Sub-features

- `hash-reject` rejects an executable whose SHA-256 differs from the recorded one.
- `hash-not-image` rejects a `.iso` that is not an Xbox disc image.
- `hash-missing` reports an unreadable or missing path.

## How to get to it (user POV)

- `pure-runtime <path>` (or the launcher's Open dialog) with any file other than the user's `default.xex` or disc image.

## Driving it with run.sh

Preconditions:

- A scratch data directory, so nothing is remembered in the user's: `export PURE_DATA_DIR=$(mktemp -d /tmp/pure-hash.XXXXXX)`.

- **Wrong file.** Run `probe=$(mktemp -d /tmp/not-pure.XXXXXX); bad="$probe/not-pure.xex"; head -c 4096 /dev/urandom > "$bad"; .claude/skills/verify-pure-runtime/scripts/run.sh hash-gate "$bad"` (a private directory, since macOS's `mktemp` only randomises trailing `X`s and a fixed name in `/tmp` could be pre-created by someone else). `summary.txt` stop line reads `<path> is not the copy of Pure this runtime supports: its executable's SHA-256 is <hash>, and the runtime needs the World release with no title update (<expected hash>).`, both hashes in full (64 hex digits), with `window: present`.
- **Nothing loaded.** `grep -c 'entry point\|import:' runtime.log` prints `0`.
- **Not a disc image.** In the same directory, `head -c 200000 /dev/urandom > "$probe/not-pure.iso"` and run it: the stop line is `<probe>/not-pure.iso is not an Xbox disc image`. Then `rm -rf "$probe"`.
- **Missing file.** Run `.claude/skills/verify-pure-runtime/scripts/run.sh hash-missing /tmp/does-not-exist.xex`. The stop line is `/tmp/does-not-exist.xex: No such file or directory (os error 2)`, and no `entry point` line appears.
- **Nothing remembered.** `ls -A "$PURE_DATA_DIR"` is empty after the refusals: `last-game` is written only once a game is prepared.

## Gotchas

- Never prove this with a modified copy of the real executable; random bytes are enough and leave nothing game-derived behind.
- Delete the scratch files and data directory afterwards; they live outside the evidence directory on purpose.
