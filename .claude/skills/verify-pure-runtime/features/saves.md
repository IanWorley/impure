# Save games

The game keeps its progress in a content package on the console's hard drive. The runtime offers that drive and stores each package as a directory under its data directory (`PURE_DATA_DIR`, or the platform default: `~/.local/share/pure-runtime` on Linux): `content/<XUID>/<title ID>/<content type>/<name>/`, with the package's display name in `<name>.display_name` beside it. When the player goes through the start menus, Pure looks for its save (`PureSave`), creates it if missing, writes `puresave.dat` (about 20 KB), and on a later launch opens and reads it back.

## Sub-features

- `saves-enumerate` lists existing packages of the requested type (`content: N packages of type 0x1 in …`).
- `saves-create` creates a package and mounts it writable (`content: save: "PureSave" (…) mode 1 -> Ok(1)`).
- `saves-write` writes files in the mounted package (`file: write "save:\\puresave.dat" …`).
- `saves-load` opens an existing package and reads it back on a later run (`mode 3 -> Ok(2)`, `file: read "save:\\puresave.dat" … 20225 of 20225 bytes`).

## How to get to it (user POV)

- Start the game and press A (Enter) through the start menus; the game saves on its own.

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready`; `xdotool` is installed. Always set `PURE_DATA_DIR` to a fresh directory under `local/verification/`, so runs never touch real saves.

- **Create and write.** `export PURE_DATA_DIR=$PWD/local/verification/saves-$(date +%s); mkdir -p $PURE_DATA_DIR`, then drive a 70-second run with `VERIFY_RUST_LOG=info,file=debug` and press, 3 s apart after the window appears: Return ×4, Down, Return ×2, Escape, Return ×3 (see [input.md](./input.md) for sending keys). `runtime.log` shows `0 packages`, the `mode 1 -> Ok(1)` create line, a `file: write "save:\\puresave.dat"` line and `save: closed -> 0`; `find $PURE_DATA_DIR -type f` lists `PureSave.display_name` and `PureSave/puresave.dat`.
- **Load.** Repeat with the same `PURE_DATA_DIR`. The log shows `1 packages`, `mode 3 -> Ok(2)` and a read of the whole file.

## Gotchas

- Without the full key sequence the game never reaches its save code.
- A package left empty by an earlier crashed run is reopened as is; start from a fresh `PURE_DATA_DIR` to prove creation.
- Never point `PURE_DATA_DIR` at a directory the run did not create.
