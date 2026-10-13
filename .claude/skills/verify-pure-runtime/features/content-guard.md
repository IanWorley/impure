# Content guard

`tools/check-no-game-content.sh` fails when the Git index contains anything that looks like game data or generated code: forbidden names (disc images, executables, media, objects and libraries, XenonRecomp output names, `originals/` or `local/`), XenonRecomp function bodies or function names in file contents, the `XEX2` magic at the start of any file, or a file over 2 MiB. The hand-written title fixes in `runtime/cpp/patches.cpp` may name the recompiled functions they wrap, so the function-name check skips that file. CI runs it on every push to `main` and every pull request.

## Sub-features

- `guard-clean` prints `No game content tracked.` and exits 0 on the real repo.
- `guard-name` rejects forbidden file names.
- `guard-content` rejects generated-code markers and `XEX2`-magic files whatever their names.
- `guard-title-fixes` lets `runtime/cpp/patches.cpp`, and no other file, name a recompiled function.
- `guard-size` rejects files over 2 MiB.

## How to get to it (user POV)

- `tools/check-no-game-content.sh` before committing (it reads the staged index, so `git add` first).
- The `Repository checks` job in `.github/workflows/ci.yml`.

## Driving it with the shell

Preconditions:

- Probes stage files, so they run only in a throwaway clone: `probe=$(mktemp -d /tmp/guard-probe.XXXXXX); git clone -q . "$probe"`.

- **Clean repo.** Run `tools/check-no-game-content.sh; echo "exit $?"` in the real checkout. Output `No game content tracked.`, exit `0`.
- **Forbidden name.** `touch "$probe/x.o"; git -C "$probe" add x.o; "$probe/tools/check-no-game-content.sh"; echo "exit $?"`. Lists `x.o`, exit `1`.
- **XEX magic.** `printf 'XEX2rest' > "$probe/harmless.txt"; git -C "$probe" add harmless.txt; "$probe/tools/check-no-game-content.sh"`. Reports `harmless.txt starts with the XEX magic`.
- **Generated marker.** Copy one line from any generated `ppc_recomp.*.cpp` in the game library's translation (`<data directory>/games/<SHA-256>/recomp/ppc/`) into `"$probe/marker.txt"` (e.g. `grep -m1 -h '^PPC_FUNC_' "$HOME/Library/Application Support/pure-runtime"/games/*/recomp/ppc/ppc_recomp.0.cpp > "$probe/marker.txt"`), stage it, and run the guard. Reports `tracked files contain XenonRecomp output:` and lists `marker.txt` once for each of the two patterns the line matches (body and name).
- **Title fixes exemption.** Append a function name to the title fixes, built with `printf` so the name never appears literally here: `printf '// __imp__sub_%s\n' 8252A978 >> "$probe/runtime/cpp/patches.cpp"; git -C "$probe" add runtime/cpp/patches.cpp; "$probe/tools/check-no-game-content.sh"; echo "exit $?"`. Output `No game content tracked.`, exit `0`. The same `printf` into `"$probe/other.txt"`, staged, reports `tracked files contain XenonRecomp output:` with `other.txt`, exit `1`.
- **Too large.** `head -c 3000000 /dev/zero > "$probe/big.txt"; git -C "$probe" add big.txt; "$probe/tools/check-no-game-content.sh"`. Reports `large files are not allowed`.
- **Cleanup.** `rm -rf "$probe"` after saving outputs to `local/verification/<ts>-guard/`.

## Gotchas

- Unstaged files are invisible to the guard by design; a probe that forgets `git add` passes falsely.
- Never write a generated-code marker (see `GENERATED_BODY_PATTERN` and `GENERATED_NAME_PATTERN` in the guard) into any tracked file in the real repo, including this skill: the guard would (correctly) fail on it. That is why the probes copy a marker from the data directory's translation or build it with `printf` instead of spelling it out.
