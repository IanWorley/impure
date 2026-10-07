#!/usr/bin/env bash
# Read-only: reports whether this checkout is ready to drive the Pure runtime. Changes nothing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
readonly REPO_ROOT
readonly EXPECTED_XEX_SHA256="83c52e15c5c1abbddc5a8e695186200e82e4cbd63f6b679bfc811c62e264f33c"
readonly XEX="${REPO_ROOT}/originals/disc/default.xex"
readonly GENERATED_DIR="${REPO_ROOT}/local/recomp/ppc"
readonly BINARY="${PURE_RUNTIME_BINARY:-${REPO_ROOT}/runtime/target/debug/pure-runtime}"
readonly LAVAPIPE_ICD="/usr/share/vulkan/icd.d/lvp_icd.json"
# Only a binary built with the `recompiled` feature links the Rust import dispatcher.
readonly RECOMPILED_MARKER="pure_import_call"

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
check "generated C++ present (${GENERATED_DIR})" test -f "${GENERATED_DIR}/ppc_func_mapping.cpp"
check "runtime binary present (${BINARY})" test -x "${BINARY}"
if [[ -x "${BINARY}" ]]; then
    if grep -qa "${RECOMPILED_MARKER}" "${BINARY}"; then
        note "binary was built WITH the recompiled feature"
    else
        note "binary was built WITHOUT the recompiled feature (game-free)"
    fi
    newer="$(find "${REPO_ROOT}/runtime/src" "${REPO_ROOT}/runtime/cpp" "${REPO_ROOT}/runtime/build.rs" -newer "${BINARY}" -type f | head -1)"
    if [[ -n "${newer}" ]]; then
        echo "FAIL  binary is stale: ${newer} changed after it was built"
        failures=$((failures + 1))
    else
        echo "ok    binary is newer than runtime sources"
    fi
fi
check "Xvfb installed" command -v Xvfb
check "ImageMagick import installed" command -v import
check "xwininfo installed" command -v xwininfo
check "Mesa lavapipe Vulkan driver present" test -f "${LAVAPIPE_ICD}"

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
