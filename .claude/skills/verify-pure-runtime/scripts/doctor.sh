#!/usr/bin/env bash
# Read-only: reports whether this checkout is ready to drive the Pure runtime. Changes nothing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
readonly REPO_ROOT
readonly EXPECTED_XEX_SHA256="83c52e15c5c1abbddc5a8e695186200e82e4cbd63f6b679bfc811c62e264f33c"
readonly XEX="${REPO_ROOT}/originals/disc/default.xex"
readonly XENON_BUILD="${XENON_RECOMP_DIR:-${REPO_ROOT}/local/tools/XenonRecomp}/build-release"
readonly BINARY="${PURE_RUNTIME_BINARY:-${REPO_ROOT}/runtime/target/debug/pure-runtime}"
readonly LAVAPIPE_ICD="/usr/share/vulkan/icd.d/lvp_icd.json"
# Where the runtime keeps the game library it builds (runtime/src/data_dir.rs).
if [[ -n "${PURE_DATA_DIR:-}" ]]; then
    readonly DATA_DIR="${PURE_DATA_DIR}"
elif [[ "$(uname -s)" == Darwin ]]; then
    readonly DATA_DIR="${HOME}/Library/Application Support/pure-runtime"
else
    readonly DATA_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/pure-runtime"
fi
readonly GAME_LIBRARY_GLOB="${DATA_DIR}/games/${EXPECTED_XEX_SHA256}/pure_game."

failures=0
check() {
    local label="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        echo "ok    ${label}"
    else
        echo "FAIL  ${label}"
        failures=$((failures + 1))
    fi
}
note() { echo "info  $*"; }

check "game executable present (${XEX})" test -f "${XEX}"
if [[ -f "${XEX}" ]]; then
    check "game executable hash matches" test "$(sha256sum "${XEX}" | cut -d' ' -f1)" = "${EXPECTED_XEX_SHA256}"
fi
# The runtime's builder translates and compiles the game with these (docs/tooling.md).
check "XenonAnalyse and XenonRecomp built (${XENON_BUILD})" \
    test -x "${XENON_BUILD}/XenonAnalyse/XenonAnalyse" -a -x "${XENON_BUILD}/XenonRecomp/XenonRecomp"
check "clang++ available" command -v clang++
if compgen -G "${GAME_LIBRARY_GLOB}*" >/dev/null; then
    note "game library built: $(compgen -G "${GAME_LIBRARY_GLOB}*" | grep -v stamp)"
else
    note "no game library yet: the first run builds it (about 10 to 15 minutes), or use --build-game"
fi
check "runtime binary present (${BINARY})" test -x "${BINARY}"
if [[ -x "${BINARY}" ]]; then
    # Changes to runtime/cpp need no runtime rebuild: the builder recompiles the glue itself.
    newer="$(find "${REPO_ROOT}/runtime/src" "${REPO_ROOT}/runtime/Cargo.toml" \
        -newer "${BINARY}" -type f 2>/dev/null | head -1)"
    if [[ -n "${newer}" ]]; then
        echo "FAIL  binary is stale: ${newer} changed after it was built"
        failures=$((failures + 1))
    else
        echo "ok    binary is newer than runtime sources"
    fi
fi
if [[ "$(uname -s)" == Linux ]]; then
    check "Xvfb installed" command -v Xvfb
    check "ImageMagick import installed" command -v import
    check "xwininfo installed" command -v xwininfo
    check "Mesa lavapipe Vulkan driver present" test -f "${LAVAPIPE_ICD}"
    # Input scripts need no tools; xdotool only drives a run by hand.
    if command -v xdotool >/dev/null; then
        echo "ok    xdotool installed (keys by hand)"
    else
        note "xdotool missing; input scripts still work (sudo apt-get install xdotool to send keys by hand)"
    fi
else
    # macOS: the runtime opens a real window on the GPU; screenshots come from input scripts.
    check "swift installed (window check)" command -v swift
    note "runs open a window on this desktop; screenshots come from --input-script"
fi

# run.sh records the processes it started and removes the record when it tears them down.
for pid_file in "${REPO_ROOT}"/local/verification/*/harness.pids; do
    [[ -f "${pid_file}" ]] || continue
    while read -r pid; do
        if kill -0 "${pid}" 2>/dev/null; then
            note "harness process ${pid} from ${pid_file%/harness.pids} is still running"
        fi
    done <"${pid_file}"
done

if ((failures > 0)); then
    echo "${failures} check(s) failed"
    exit 1
fi
echo "ready"
