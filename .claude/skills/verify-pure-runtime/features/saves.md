# Save games

The game keeps its progress in a content package on the console's hard drive. The runtime offers that drive and stores each package as a directory under its data directory (`PURE_DATA_DIR`, or the platform default: `~/Library/Application Support/pure-runtime` on macOS, `$XDG_DATA_HOME/pure-runtime` or `~/.local/share/pure-runtime` on Linux, `%APPDATA%\pure-runtime` on Windows): `content/<XUID>/<title ID>/<content type>/<name>/`, with the package's display name in `<name>.display_name` beside it. When the player goes through the start menus, Pure looks for its save (`PureSave`), creates it if missing, writes `puresave.dat` (about 20 KB), and on a later launch opens and reads it back.

## Sub-features

- `saves-enumerate` lists existing packages of the requested type (`content: N packages of type 0x1 in …`).
- `saves-create` creates a package and mounts it writable (`content: save: "PureSave" (…) mode 1 -> Ok(1)`).
- `saves-write` writes files in the mounted package (`file: write "save:\\puresave.dat" at None: 20225 bytes -> 0x0`).
- `saves-load` opens an existing package and reads it back on a later run (`mode 3 -> Ok(2)`, `file: read "save:\\puresave.dat" at Some(0): 20225 of 20225 bytes -> 0x0`).

## How to get to it (user POV)

- Start the game and press A (Enter) through the start menus; the game saves on its own.

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready`. Always set `PURE_DATA_DIR` to a fresh directory, so runs never touch real saves. The data directory also holds the game library, so give the fresh one a copy of the built library first (`mkdir -p "$PURE_DATA_DIR/games"; cp -Rc "<data directory>/games/<SHA-256>" "$PURE_DATA_DIR/games/"`, see [game-library.md](./game-library.md)) or its first run spends about 10 minutes building one.

- **Create and write (any host).** With that fresh `PURE_DATA_DIR`, drive [`inputs/to-tutorial.txt`](../inputs/to-tutorial.txt) with `VERIFY_RUST_LOG=info,file=debug` (`VERIFY_RUN_SECONDS=600`; it exits itself at frame 9,150). With no save the title screen leads to `No Save File Found`, which the script's next Return answers with Create Save File: `runtime.log` shows `content: 0 packages of type 0x1 in …/content/E000000000000001/425607D9/00000001`, `content: save: "PureSave" ("Pure - <date>") mode 1 -> Ok(1)` around frame 2,020 on an M4, the `file: write` line and `save: closed -> 0`; `find "$PURE_DATA_DIR/content" -type f` lists `PureSave.display_name` and `PureSave/puresave.dat`.
- **Load (any host).** With the same `PURE_DATA_DIR`, drive the script's steps before frame 2,400 and then `2400 exit` (`grep -v '^#' inputs/to-tutorial.txt | awk '$1 < 2400'; echo '2400 exit'`): `1 packages`, `mode 3 -> Ok(2)` and the read of the whole file.

- **Create and write (Linux, by hand).** `export PURE_DATA_DIR=$PWD/local/verification/saves-$(date +%s); mkdir -p $PURE_DATA_DIR`, then drive a 70-second run with `VERIFY_RUST_LOG=info,file=debug` and press, 3 s apart after the window appears: Return ×4, Down, Return ×2, Escape, Return ×3 (see [input.md](./input.md) for sending keys). `runtime.log` shows `0 packages`, the `mode 1 -> Ok(1)` create line, a `file: write "save:\\puresave.dat"` line and `save: closed -> 0`; `find $PURE_DATA_DIR -type f` lists `PureSave.display_name` and `PureSave/puresave.dat`.
- **Load (Linux, by hand).** Repeat with the same `PURE_DATA_DIR`. The log shows `1 packages`, `mode 3 -> Ok(2)` and a read of the whole file.

## Gotchas

- Without the full key sequence the game never reaches its save code.
- A package left empty by an earlier crashed run is reopened as is; start from a fresh `PURE_DATA_DIR` to prove creation.
- Never point `PURE_DATA_DIR` at a directory the run did not create.
