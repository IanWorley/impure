# Recompile script

`tools/recompile.sh [xex] [output-dir]` turns the user's own `default.xex` into about 330 MB of C++ with XenonRecomp. It refuses a missing or wrong-hash executable, an unpinned or unpatched XenonRecomp, and an existing output directory, and it never leaves a half-written output behind.

## Sub-features

- `recomp-generate` writes `ppc/`, `config.toml`, `switch_tables.toml`, `analyse.log` and `recomp.log` atomically.
- `recomp-deterministic` produces output identical to the existing `local/recomp`.
- `recomp-reject-input` refuses a missing or wrong-hash executable.
- `recomp-reject-existing` refuses an existing output directory without touching it.

## How to get to it (user POV)

- `tools/recompile.sh` (defaults: `originals/disc/default.xex`, `local/recomp`).
- `tools/recompile.sh <xex> <output-dir>`.

## Driving it with the shell

Preconditions:

- Doctor passes the hash check; XenonRecomp is checked out, patched, and built in `local/tools/XenonRecomp/build-release`.
- `run=local/verification/$(date -u +%Y%m%dT%H%M%SZ)-recompile; mkdir -p "$run"`.

- **Reject existing output.** Run `tools/recompile.sh originals/disc/default.xex local/recomp 2>&1 | tee "$run/existing.txt"; echo "exit ${PIPESTATUS[0]}" >> "$run/existing.txt"`. Exit `1`, message `already exists`, and `stat -c %Y local/recomp` is unchanged.
- **Reject wrong input.** Run `tools/recompile.sh /etc/hostname "$run/never" 2>&1 | tee "$run/wrong.txt"`. Exit `1`, message names both hashes, and `"$run/never"` and any `"$run/never.partial.*"` do not exist.
- **Generate.** Run `tools/recompile.sh originals/disc/default.xex "$run/recomp" > "$run/generate.txt" 2>&1` (about a minute). Exit `0`; `"$run/recomp/ppc"` exists and no `.partial.*` sibling remains.
- **Deterministic.** Run `diff -r --exclude=objects --exclude='*.log' local/recomp "$run/recomp" > "$run/diff.txt"; echo "exit $?" >> "$run/diff.txt"`. `diff.txt` ends with `exit 0`.
- **Proof then cleanup.** Keep the `.txt` files; delete `"$run/recomp"` (330 MB of generated code, scratch, not evidence).

## Gotchas

- Never pass `local/recomp` as a fresh target or delete it: its `objects/` cache saves an hour-long rebuild.
- The output contains the game's translated code. It stays in `local/` and is never quoted in proof.
