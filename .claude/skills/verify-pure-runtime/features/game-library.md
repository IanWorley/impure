# Game library

The runtime never contains the game's code. Given the user's disc image or `default.xex`, it copies the disc's files out of an image once, translates the executable with XenonAnalyse and XenonRecomp, compiles that with the glue in `runtime/cpp/` using the user's Clang, and links a library (`pure_game.dylib`, `.so` or `.dll`) in the data directory under `games/<xex SHA-256>/`, which it then loads. Every step is cached: a warm start takes a fraction of a second, and a change to the glue recompiles only the glue.

## Sub-features

- `library-from-image` builds the library from a disc image in an empty data directory: disc files, translation, compile, link.
- `library-from-xex` builds it from an extracted `default.xex` (no disc copy; the disc is the executable's directory).
- `library-warm` reuses everything: no stage after the first (`Checking your copy of the game` for a `default.xex`, `Reading your disc image` for an image).
- `library-relocatable` reuses a cache copied to another data directory.
- `library-incremental` rebuilds only what is missing: a removed library is relinked, removed objects are recompiled.
- `library-exports` exports exactly the runtime's entry points and needs nothing from the runtime but its host table.
- `library-plays` runs the game from the library, matching earlier evidence.

## How to get to it (user POV)

- `pure-runtime <disc image or default.xex>` (or the launcher's Open dialog; see `launcher.md`).
- `pure-runtime <disc image or default.xex> --build-game`: prepare only, progress on stderr, exit 0 when ready.

## Driving it with the shell

Preconditions:

- Doctor passes: hash, XenonRecomp tools built, `clang++` available, release binary fresh.
- A cold build from the image needs about 8 GB free: 7 GB for the disc's files, plus the translation, objects and library (about 0.6 GB). The warm, relocatable, incremental and exports recipes need no extra space on APFS when the cache is copied with `cp -Rc` (clones).
- `run=local/verification/$(date -u +%Y%m%dT%H%M%SZ)-library; mkdir -p "$run"` and a scratch data directory, `export PURE_DATA_DIR=$(mktemp -d /tmp/pure-library.XXXXXX)`.

- **From the image (cold).** Run `time "$PURE_RUNTIME_BINARY" "originals/Pure (World) (En,Fr,De,Es,It,Nl).iso" --build-game > "$run/cold.txt" 2>&1`. The stages appear in order (`Reading your disc image`, `Copying the game's files from your disc image (a few minutes)`, `Translating the game's code (a minute or two)`, `Checking which parts of the game need compiling`, `Compiling the game (1701 files; about 10 to 15 minutes the first time)` with percentages, `Linking the game library`) and it ends `The game library is ready: …/pure_game.<ext>`; about 10 minutes on an Apple M4, plus a minute or two copying 7 GB.
- **Warm.** Run it again into `"$run/warm.txt"`: only the first stage (`Reading your disc image` here), then ready, in well under a second.
- **Exports.** `nm -gU "$PURE_DATA_DIR"/games/*/pure_game.dylib | grep -c ' _pure'` prints `11`, and `nm -u` lists only C++ standard library and system symbols (macOS; on Linux `nm -D --defined-only`).
- **Relocatable.** Copy the game's directory (`games/<SHA-256>/`: `recomp/`, `build/`, `pure_game.*`, and `disc/` if it was built from an image) into another scratch data directory, e.g. `mkdir -p "$PURE_DATA_DIR/games"; cp -Rc "<data directory>/games/<SHA-256>" "$PURE_DATA_DIR/games/"`, and run `--build-game` there with the same source: `Checking your copy of the game` (or `Reading your disc image`) and ready at once. Without `disc/`, an image source copies the disc's files again.
- **Incremental.** In that copy, `rm` the `pure_game.dylib` and run `--build-game`: `Checking which parts of the game need compiling`, `Linking the game library`, ready in about 2 seconds. Remove it again together with a few `build/objects/ppc_recomp.*.o`: `Compiling the game (3 files; …)` with `30%`, `60%`, `100%`, then linking. Objects alone are not rechecked while the library and its stamp are current.
- **Plays.** In the scratch data directory, `PURE_DATA_DIR=… VERIFY_RUN_SECONDS=600 .claude/skills/verify-pure-runtime/scripts/run.sh library-tutorial originals/disc/default.xex --input-script .claude/skills/verify-pure-runtime/inputs/to-tutorial.txt`: `3 title fixes in place (cpp/patches.cpp)`, `entry point 0x82254d60`, and the tutorial's captures (see [rendering.md](./rendering.md)), ending `runtime exit: 0`. With no save the tutorial script creates one (see [saves.md](./saves.md)). The other scripts (`inputs/to-race.txt` and friends) were written against an existing save, so copy the user's `content/` into the scratch directory first for them.
- **Proof then cleanup.** Keep the `.txt` files; delete the scratch data directories (7 GB of game files and the compiled game: game-derived, never evidence).

## Gotchas

- Everything under `games/` is game-derived: never quote, copy into evidence, or commit it.
- The user's real data directory holds their saves; use a scratch `PURE_DATA_DIR` for builds you are testing, and copy saves into it rather than pointing at theirs.
- On macOS the compiler is the Command Line Tools' Clang with the SDK from `xcrun --show-sdk-path`; a `'climits' file not found` error means it ran without that SDK.
