#!/usr/bin/env bash
# Fails if any tracked file looks like game data, decompiler output, or recompiler output.
# CI runs this on every push; run it locally before committing. Contents and sizes are read from
# the index, which is what a commit records, so a tracked file deleted locally is still checked.
set -euo pipefail

# Nothing in this repository should come close to this size; game files and generated code do.
readonly MAX_TRACKED_FILE_BYTES=$((2 * 1024 * 1024))
# Disc images, archives, executables, Bink video, XMA/XACT audio, raw dumps, compiled objects and
# libraries, and XenonRecomp's output names.
readonly FORBIDDEN_PATTERN='\.(iso|7z|xex|xexp|bik|xma|xwb|xsb|xgs|bin|o|obj|a|lib)$|(^|/)ppc_recomp\.[0-9]+\.cpp$|(^|/)ppc_func_mapping\.cpp$|(^|/)(originals|local)/'
# Every recompiled function is emitted as PPC_FUNC_IMPL(__imp__sub_<8 hex digits>). Committed
# glue and build code name the PPC_FUNC macro and the bare `__imp__sub_` prefix, never these.
readonly GENERATED_CODE_PATTERN='PPC_FUNC_IMPL\(|__imp__sub_[0-9A-F]{8}'
# This script spells out the patterns above, so it is the one file the content search skips.
readonly SELF_PATHSPEC=':(exclude)tools/check-no-game-content.sh'
# Every Xbox 360 executable starts with this magic, whatever its file name.
readonly XEX_MAGIC='XEX2'

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
cd "${REPO_ROOT}"

status=0

forbidden="$(git ls-files | grep -iE "${FORBIDDEN_PATTERN}" || true)"
if [[ -n "${forbidden}" ]]; then
    echo "error: tracked files look like game data or generated code:" >&2
    echo "${forbidden}" >&2
    status=1
fi

generated="$(git grep --cached -lE "${GENERATED_CODE_PATTERN}" -- . "${SELF_PATHSPEC}" || true)"
if [[ -n "${generated}" ]]; then
    echo "error: tracked files contain XenonRecomp output:" >&2
    echo "${generated}" >&2
    status=1
fi

while IFS= read -r -d '' file; do
    size="$(git cat-file -s ":${file}")"
    if (( size > MAX_TRACKED_FILE_BYTES )); then
        echo "error: ${file} is ${size} bytes; large files are not allowed in this repository." >&2
        status=1
    fi
    if cmp -s -n "${#XEX_MAGIC}" <(git cat-file blob ":${file}") <(printf '%s' "${XEX_MAGIC}"); then
        echo "error: ${file} starts with the XEX magic; game executables are not allowed." >&2
        status=1
    fi
done < <(git ls-files -z)

if (( status == 0 )); then
    echo "No game content tracked."
fi
exit "${status}"
