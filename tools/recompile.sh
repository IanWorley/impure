#!/usr/bin/env bash
# Statically recompiles the user's own copy of Pure's default.xex into C++ with XenonRecomp.
# Nothing from the game ships with this repository: the executable must be supplied locally,
# and all generated code is written to the ignored local/ tree.
set -euo pipefail

readonly EXPECTED_XEX_SHA256="83c52e15c5c1abbddc5a8e695186200e82e4cbd63f6b679bfc811c62e264f33c"
# Must match docs/tooling.md, which explains how to check out, patch, and build it.
readonly XENON_RECOMP_COMMIT="ddd128bcca99fe8bfbb99bea583c972351fa6ace"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
readonly XEX_INPUT="${1:-${REPO_ROOT}/originals/disc/default.xex}"
output_argument="${2:-${REPO_ROOT}/local/recomp}"
# The temporary sibling directory is named after this path, so drop any trailing slash.
readonly OUTPUT_DIR="${output_argument%/}"
readonly XENON_RECOMP_DIR="${XENON_RECOMP_DIR:-${REPO_ROOT}/local/tools/XenonRecomp}"
readonly XENON_RECOMP_PATCH="${REPO_ROOT}/tools/patches/xenonrecomp.patch"
# Release build: Pure's code section sizes are not word multiples, which trips a debug-only assert.
readonly XENON_BUILD_DIR="${XENON_BUILD_DIR:-${XENON_RECOMP_DIR}/build-release}"
readonly PURE_CONFIG="${REPO_ROOT}/recomp/pure.toml"
readonly PURE_SWITCH_TABLES="${REPO_ROOT}/recomp/pure_switch_tables.toml"
readonly SWITCH_TABLES_FILE="switch_tables.toml"
readonly RECOMP_CONFIG_FILE="config.toml"
readonly GENERATED_DIR_NAME="ppc"

fail() {
    echo "error: $*" >&2
    exit 1
}

[[ -f "${XEX_INPUT}" ]] || fail "game executable not found at ${XEX_INPUT}. Extract your own copy of the disc first (see docs/tooling.md)."

# XenonRecomp runs from the output directory, so every path it sees must be absolute.
XEX_PATH="$(realpath "${XEX_INPUT}")"
readonly XEX_PATH
actual_sha256="$(sha256sum "${XEX_PATH}" | cut -d' ' -f1)"
[[ "${actual_sha256}" == "${EXPECTED_XEX_SHA256}" ]] \
    || fail "${XEX_PATH} has SHA-256 ${actual_sha256}; expected ${EXPECTED_XEX_SHA256} (base World disc, no title update)."

actual_commit="$(git -C "${XENON_RECOMP_DIR}" rev-parse HEAD 2>/dev/null)" \
    || fail "no XenonRecomp checkout at ${XENON_RECOMP_DIR}; see docs/tooling.md."
[[ "${actual_commit}" == "${XENON_RECOMP_COMMIT}" ]] \
    || fail "XenonRecomp at ${XENON_RECOMP_DIR} is at ${actual_commit}; expected ${XENON_RECOMP_COMMIT} (see docs/tooling.md)."
git -C "${XENON_RECOMP_DIR}" apply --reverse --check "${XENON_RECOMP_PATCH}" 2>/dev/null \
    || fail "tools/patches/xenonrecomp.patch is not applied to ${XENON_RECOMP_DIR}; see docs/tooling.md."

[[ -x "${XENON_BUILD_DIR}/XenonAnalyse/XenonAnalyse" && -x "${XENON_BUILD_DIR}/XenonRecomp/XenonRecomp" ]] \
    || fail "XenonAnalyse/XenonRecomp not built in ${XENON_BUILD_DIR}; see docs/tooling.md."
ANALYSE="$(realpath "${XENON_BUILD_DIR}/XenonAnalyse/XenonAnalyse")"
readonly ANALYSE
RECOMP="$(realpath "${XENON_BUILD_DIR}/XenonRecomp/XenonRecomp")"
readonly RECOMP
[[ ! -e "${OUTPUT_DIR}" ]] || fail "${OUTPUT_DIR} already exists; choose a new output directory to preserve earlier results."

# Generate beside the destination and move it into place only once both tools succeed, so an
# interrupted or failed run never leaves a directory that looks complete.
mkdir -p "$(dirname "${OUTPUT_DIR}")"
PARTIAL_DIR="$(mktemp -d "${OUTPUT_DIR}.partial.XXXXXX")"
readonly PARTIAL_DIR
trap 'rm -rf "${PARTIAL_DIR}"' EXIT
mkdir "${PARTIAL_DIR}/${GENERATED_DIR_NAME}"

"${ANALYSE}" "${XEX_PATH}" "${PARTIAL_DIR}/${SWITCH_TABLES_FILE}" > "${PARTIAL_DIR}/analyse.log"
cat "${PURE_SWITCH_TABLES}" >> "${PARTIAL_DIR}/${SWITCH_TABLES_FILE}"

# The generated paths are relative to the config, so the directory can move once it is complete.
{
    echo "[main]"
    echo "file_path = \"${XEX_PATH}\""
    echo "out_directory_path = \"${GENERATED_DIR_NAME}\""
    echo "switch_table_file_path = \"${SWITCH_TABLES_FILE}\""
    cat "${PURE_CONFIG}"
} > "${PARTIAL_DIR}/${RECOMP_CONFIG_FILE}"

# XenonRecomp prefixes file_path with the config's directory even when it is absolute,
# so pass the config by bare name from inside the output directory.
CONTEXT_HEADER="$(realpath "${XENON_RECOMP_DIR}/XenonUtils/ppc_context.h")"
readonly CONTEXT_HEADER
(cd "${PARTIAL_DIR}" && "${RECOMP}" "${RECOMP_CONFIG_FILE}" "${CONTEXT_HEADER}" > recomp.log)

mv -T "${PARTIAL_DIR}" "${OUTPUT_DIR}"
trap - EXIT

echo "Generated C++ written to ${OUTPUT_DIR}/${GENERATED_DIR_NAME} (warnings in ${OUTPUT_DIR}/recomp.log)"
