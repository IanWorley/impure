#!/usr/bin/env bash
# Runs the Pure runtime once and captures evidence: the runtime log, screenshots, and a summary.
# On Linux it runs on a private headless X display with Mesa's software Vulkan driver and
# screenshots the display. On macOS it opens an ordinary window on the GPU through Metal;
# screenshots there come from an input script, since capturing the screen needs a permission.
#
#   run.sh <label> [runtime arguments...]
#   run.sh boot originals/disc/default.xex
#   VERIFY_RUN_SECONDS=60 run.sh boot originals/disc/default.xex
#   run.sh title originals/disc/default.xex --input-script local/verification/scripts/title.txt
#
# An input script is copied into the evidence directory and run from there, so the screenshots
# it takes land beside the log.
#
# By default the run ends when the guest stops (or VERIFY_TIMEOUT_SECONDS, default 120, pass).
# With VERIFY_RUN_SECONDS the guest is expected to keep running: the run lasts that long, and a
# stop or exit before then is the outcome. VERIFY_RUST_LOG sets the log filter (default info; an
# ambient RUST_LOG is ignored). PURE_RUNTIME_BINARY overrides the binary (default
# runtime/target/debug/pure-runtime). Evidence goes to local/verification/<timestamp>-<label>/.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
readonly REPO_ROOT
readonly BINARY="${PURE_RUNTIME_BINARY:-${REPO_ROOT}/runtime/target/debug/pure-runtime}"
readonly TIMEOUT_SECONDS="${VERIFY_TIMEOUT_SECONDS:-120}"
readonly RUN_SECONDS="${VERIFY_RUN_SECONDS:-}"
# The stable handles are info lines, so an ambient RUST_LOG (warn, say) must not hide them.
readonly LOG_FILTER="${VERIFY_RUST_LOG:-info}"
readonly FIRST_DISPLAY=100
readonly LAST_DISPLAY=199
readonly SCREEN_GEOMETRY="1280x720x24"
readonly WINDOW_TITLE="Pure (static recompilation)"
readonly STOP_LINE="guest stopped:"
# The runtime logs "first frame presented" once and "<N> frames presented" every 600 frames.
readonly FRAMES_LINE="frames presented"
readonly FIRST_FRAME_LINE="first frame presented"
readonly LAVAPIPE_ICD="/usr/share/vulkan/icd.d/lvp_icd.json"
readonly INPUT_SCRIPT_FLAG="--input-script"
HOST_OS="$(uname -s)"
readonly HOST_OS
# Bevy renders a few frames after the stop is logged; give the clear colour time to reach the screen.
readonly SETTLE_SECONDS=3
readonly POLL_SECONDS=1

[[ $# -ge 1 ]] || { echo "usage: $0 <label> [runtime arguments...]" >&2; exit 2; }
readonly LABEL="$1"
shift
[[ -x "${BINARY}" ]] || { echo "error: ${BINARY} not built; see the Launch section" >&2; exit 1; }

EVIDENCE_DIR="${REPO_ROOT}/local/verification/$(date -u +%Y%m%dT%H%M%SZ)-${LABEL}"
readonly EVIDENCE_DIR
mkdir -p "${EVIDENCE_DIR}"
readonly PID_FILE="${EVIDENCE_DIR}/harness.pids"

runtime_arguments=()
while [[ $# -gt 0 ]]; do
    if [[ "$1" == "${INPUT_SCRIPT_FLAG}" && $# -ge 2 ]]; then
        cp "$2" "${EVIDENCE_DIR}/input-script.txt"
        runtime_arguments+=("$1" "${EVIDENCE_DIR}/input-script.txt")
        shift 2
    else
        runtime_arguments+=("$1")
        shift
    fi
done

display=""
if [[ "${HOST_OS}" == Linux ]]; then
    for ((number = FIRST_DISPLAY; number <= LAST_DISPLAY; number++)); do
        if [[ ! -e "/tmp/.X${number}-lock" && ! -e "/tmp/.X11-unix/X${number}" ]]; then
            display=":${number}"
            break
        fi
    done
    [[ -n "${display}" ]] || { echo "error: no free X display in ${FIRST_DISPLAY}-${LAST_DISPLAY}" >&2; exit 1; }
fi

xvfb_pid=""
runtime_pid=""
teardown() {
    # Only the processes this run started; evidence stays in place.
    [[ -z "${runtime_pid}" ]] || kill "${runtime_pid}" 2>/dev/null || true
    [[ -z "${xvfb_pid}" ]] || kill "${xvfb_pid}" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -f "${PID_FILE}"
}
trap teardown EXIT

if [[ -n "${display}" ]]; then
    Xvfb "${display}" -screen 0 "${SCREEN_GEOMETRY}" -nolisten tcp >"${EVIDENCE_DIR}/xvfb.log" 2>&1 &
    xvfb_pid=$!
    echo "${xvfb_pid}" >>"${PID_FILE}"
    for ((waited = 0; waited < TIMEOUT_SECONDS; waited++)); do
        DISPLAY="${display}" xdpyinfo >/dev/null 2>&1 && break
        sleep "${POLL_SECONDS}"
    done
fi

{
    echo "label: ${LABEL}"
    echo "binary: ${BINARY}"
    echo "arguments: ${runtime_arguments[*]+${runtime_arguments[*]}}"
    echo "display: ${display:-native window (${HOST_OS})}"
    echo "commit: $(git -C "${REPO_ROOT}" rev-parse --short HEAD)$(git -C "${REPO_ROOT}" diff --quiet || echo ' (dirty)')"
} >"${EVIDENCE_DIR}/summary.txt"

# Create the log first so the wait loop never greps a file that does not exist yet.
: >"${EVIDENCE_DIR}/runtime.log"
# NO_COLOR keeps ANSI escapes out of the log so it can be grepped and quoted.
if [[ -n "${display}" ]]; then
    DISPLAY="${display}" VK_ICD_FILENAMES="${LAVAPIPE_ICD}" WGPU_BACKEND=vulkan NO_COLOR=1 RUST_LOG="${LOG_FILTER}" \
        "${BINARY}" ${runtime_arguments[@]+"${runtime_arguments[@]}"} >"${EVIDENCE_DIR}/runtime.log" 2>&1 &
else
    NO_COLOR=1 RUST_LOG="${LOG_FILTER}" "${BINARY}" ${runtime_arguments[@]+"${runtime_arguments[@]}"} >"${EVIDENCE_DIR}/runtime.log" 2>&1 &
fi
runtime_pid=$!
echo "${runtime_pid}" >>"${PID_FILE}"

if [[ -n "${RUN_SECONDS}" ]]; then
    wait_seconds="${RUN_SECONDS}"
    outcome="still running after ${RUN_SECONDS}s"
else
    wait_seconds="${TIMEOUT_SECONDS}"
    outcome="timeout after ${TIMEOUT_SECONDS}s"
fi
for ((waited = 0; waited < wait_seconds; waited++)); do
    if grep -q "${STOP_LINE}" "${EVIDENCE_DIR}/runtime.log"; then
        outcome="guest stopped"
        break
    fi
    if ! kill -0 "${runtime_pid}" 2>/dev/null; then
        outcome="runtime exited before the guest stopped"
        break
    fi
    sleep "${POLL_SECONDS}"
done

sleep "${SETTLE_SECONDS}"
window="absent"
if [[ -n "${display}" ]]; then
    if DISPLAY="${display}" xwininfo -root -tree 2>/dev/null | grep -qF "\"${WINDOW_TITLE}\""; then
        window="present"
    fi
    DISPLAY="${display}" import -window root "${EVIDENCE_DIR}/screenshot.png" 2>>"${EVIDENCE_DIR}/xvfb.log" || true
elif kill -0 "${runtime_pid}" 2>/dev/null; then
    # Window titles need the screen recording permission on macOS; owners do not.
    window="$(swift -e "import CoreGraphics
let windows = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) as! [[String: Any]]
print(windows.contains { (\$0[kCGWindowOwnerPID as String] as? Int) == ${runtime_pid} } ? \"present\" : \"absent\")" 2>/dev/null || echo unknown)"
fi

exit_status="still running (stopped by harness)"
if ! kill -0 "${runtime_pid}" 2>/dev/null; then
    wait "${runtime_pid}" && exit_status=0 || exit_status=$?
    runtime_pid=""
fi

{
    echo "outcome: ${outcome}"
    echo "window: ${window}"
    echo "runtime exit: ${exit_status}"
    echo "stop line: $(grep -m1 "${STOP_LINE}" "${EVIDENCE_DIR}/runtime.log" | sed 's/.*guest stopped: //' || true)"
    frames="$(grep "${FRAMES_LINE}" "${EVIDENCE_DIR}/runtime.log" | grep -v "${FIRST_FRAME_LINE}" | tail -1 | grep -oE '[0-9]+ frames presented' | cut -d' ' -f1 || true)"
    if [[ -z "${frames}" ]] && grep -q "${FIRST_FRAME_LINE}" "${EVIDENCE_DIR}/runtime.log"; then
        frames="at least 1"
    fi
    echo "frames presented: ${frames:-0}"
    echo "screenshots: $(find "${EVIDENCE_DIR}" -maxdepth 1 -name '*.png' -exec basename {} \; | sort | tr '\n' ' ')"
} >>"${EVIDENCE_DIR}/summary.txt"

cat "${EVIDENCE_DIR}/summary.txt"
echo "evidence: ${EVIDENCE_DIR}"
