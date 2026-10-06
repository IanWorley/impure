#!/usr/bin/env bash
# Decompiles the user's own copy of Pure's default.xex with Ghidra.
# Nothing from the game ships with this repository: the executable must be supplied locally,
# and all output is written to the ignored local/ tree.
set -euo pipefail

readonly EXPECTED_XEX_SHA256="83c52e15c5c1abbddc5a8e695186200e82e4cbd63f6b679bfc811c62e264f33c"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
readonly XEX_PATH="${1:-${REPO_ROOT}/originals/disc/default.xex}"
readonly OUTPUT_DIR="${2:-${REPO_ROOT}/local/decomp}"
readonly GHIDRA_HOME="${GHIDRA_HOME:-${REPO_ROOT}/local/tools/ghidra_12.1.3_PUBLIC}"
# Ghidra rejects project paths containing dot-prefixed directories such as this workspace's parent.
readonly GHIDRA_PROJECT_DIR="${GHIDRA_PROJECT_DIR:-${HOME}/pure-decomp-analysis/ghidra-projects}"
readonly GHIDRA_PROJECT_NAME="${GHIDRA_PROJECT_NAME:-PureDecomp}"
export GHIDRA_HEADLESS_MAXMEM="${GHIDRA_HEADLESS_MAXMEM:-16G}"

fail() {
    echo "error: $*" >&2
    exit 1
}

[[ -f "${XEX_PATH}" ]] || fail "game executable not found at ${XEX_PATH}. Extract your own copy of the disc first (see docs/tooling.md)."

actual_sha256="$(sha256sum "${XEX_PATH}" | cut -d' ' -f1)"
[[ "${actual_sha256}" == "${EXPECTED_XEX_SHA256}" ]] \
    || fail "${XEX_PATH} has SHA-256 ${actual_sha256}; expected ${EXPECTED_XEX_SHA256} (base World disc, no title update)."

[[ -x "${GHIDRA_HOME}/support/analyzeHeadless" ]] || fail "Ghidra not found at ${GHIDRA_HOME}; see docs/tooling.md."
[[ -d "${GHIDRA_HOME}/Ghidra/Extensions/XEXLoaderWV" ]] || fail "XEXLoaderWV is not installed in ${GHIDRA_HOME}."
[[ ! -e "${OUTPUT_DIR}" ]] || fail "${OUTPUT_DIR} already exists; choose a new output directory to preserve earlier results."
[[ ! -e "${GHIDRA_PROJECT_DIR}/${GHIDRA_PROJECT_NAME}.gpr" ]] \
    || fail "Ghidra project ${GHIDRA_PROJECT_NAME} already exists; set GHIDRA_PROJECT_NAME to a new name."

mkdir -p "${OUTPUT_DIR}" "${GHIDRA_PROJECT_DIR}"

"${GHIDRA_HOME}/support/analyzeHeadless" \
    "${GHIDRA_PROJECT_DIR}" "${GHIDRA_PROJECT_NAME}" \
    -import "${XEX_PATH}" \
    -scriptPath "${REPO_ROOT}/tools/ghidra" \
    -postScript ExportDecompilation.java "${OUTPUT_DIR}" \
    -log "${OUTPUT_DIR}/ghidra.log" \
    -scriptlog "${OUTPUT_DIR}/script.log"

echo "Decompilation written to ${OUTPUT_DIR}"
