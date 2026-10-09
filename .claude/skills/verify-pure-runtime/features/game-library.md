# Game library

The runtime never contains the game's code. Given the user's disc image or `default.xex`, it copies the disc's files out of an image once, translates the executable with XenonAnalyse and XenonRecomp, compiles that with the glue in `runtime/cpp/` using the user's Clang, and links a library (`pure_game.dylib`, `.so` or `.dll`) in the data directory under `games/<xex SHA-256>/`, which it then loads. Every step is cached: a warm start takes a fraction of a second, and a change to the glue recompiles only the glue.

## Sub-features

- `library-from-image` builds the library from a disc image in an empty data directory: disc files, translation, compile, link.
- `library-from-xex` builds it from an extracted `default.xex` (no disc copy; the disc is the executable's directory).
- `library-warm` reuses everything: no stage after `Checking your copy of the game`.
- `library-relocatable` reuses a cache copied to another data directory.
- `library-exports` exports exactly the runtime's entry points and needs nothing from the runtime but its host table.
- `library-plays` runs the game from the library, matching earlier evidence.

## How to get to it (user POV)

- `pure-runtime <disc image or default.xex>` (or the launcher's Open dialog; see `launcher.md`).
- `pure-runtime <disc image or default.xex> --build-game`: prepare only, progress on stderr, exit 0 when ready.

## Driving it with the shell

Preconditions:

- Doctor passes: hash, XenonRecomp tools built, `clang++` available, release binary fresh.
- `run=local/verification/$(date -u +%Y%m%dT%H%M%SZ)-library; mkdir -p "$run"` and a scratch data directory, `export PURE_DATA_DIR=$(mktemp -d /tmp/pure-library.XXXXXX)`.

- **From the image (cold).** Run `time "$PURE_RUNTIME_BINARY" "originals/Pure (World) (En,Fr,De,Es,It,Nl).iso" --build-game > "$run/cold.txt" 2>&1`. The stages appear in order (`Reading your disc image`, `Copying the game's files…`, `Translating…`, `Compiling the game (1701 files…)` with percentages, `Linking…`) and it ends `The game library is ready: …/pure_game.<ext>`; about 10 minutes on an Apple M4, plus a minute or two copying 7 GB.
- **Warm.** Run it again into `"$run/warm.txt"`: only `Checking your copy of the game`, then ready, in well under a second.
- **Exports.** `nm -gU "$PURE_DATA_DIR"/games/*/pure_game.dylib | grep -c ' _pure'` prints `11`, and `nm -u` lists only C++ standard library and system symbols (macOS; on Linux `nm -D --defined-only`).
- **Relocatable.** Copy `recomp/`, `pure_game.*` and `build/` of the game's directory into another scratch data directory and run `--build-game` there: ready at once.
- **Plays.** `PURE_DATA_DIR=… .claude/skills/verify-pure-runtime/scripts/run.sh library-movie "originals/Pure (World) (En,Fr,De,Es,It,Nl).iso" --input-script local/verification/scripts/movie.txt`: `title fixes in place`, `entry point 0x82254d60`, and the movie's captures (10k+ colours from `m-0120.png` on). For gameplay use `inputs/to-race.txt` (see the skill); with a fresh data directory there is no save, so copy the user's `content/` into the scratch directory first or the script's presses land on the save dialogs.
- **Proof then cleanup.** Keep the `.txt` files; delete the scratch data directories (7 GB of game files and the compiled game: game-derived, never evidence).

## Gotchas

- Everything under `games/` is game-derived: never quote, copy into evidence, or commit it.
- The user's real data directory holds their saves; use a scratch `PURE_DATA_DIR` for builds you are testing, and copy saves into it rather than pointing at theirs.
- On macOS the compiler is the Command Line Tools' Clang with the SDK from `xcrun --show-sdk-path`; a `'climits' file not found` error means it ran without that SDK.
