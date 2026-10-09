#!/usr/bin/env bash
# Classifies a release tag for .github/workflows/release.yml.
#
#   tools/release-tag.sh <tag>
#
# vMAJOR.MINOR.PATCH is a full release; vMAJOR.MINOR.PATCH-rc.N (or -beta.N, -alpha.N) is a
# pre-release. Prints `prerelease=true` or `prerelease=false` (GitHub Actions output syntax).
# Fails for any other tag, or one whose MAJOR.MINOR.PATCH differs from runtime/Cargo.toml's, so a
# release always says which runtime version it is; release candidates need no Cargo.toml edit.
set -euo pipefail

readonly FULL_RELEASE='^v([0-9]+\.[0-9]+\.[0-9]+)$'
readonly PRE_RELEASE='^v([0-9]+\.[0-9]+\.[0-9]+)-(rc|beta|alpha)\.[0-9]+$'

fail() {
    echo "error: $*" >&2
    exit 1
}

[[ $# -eq 1 ]] || fail "usage: $0 <tag>"
readonly TAG="$1"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT

if [[ "${TAG}" =~ ${FULL_RELEASE} ]]; then
    prerelease=false
elif [[ "${TAG}" =~ ${PRE_RELEASE} ]]; then
    prerelease=true
else
    fail "${TAG} is not a release tag: use vMAJOR.MINOR.PATCH, or vMAJOR.MINOR.PATCH-rc.N for a release candidate"
fi
readonly TAG_VERSION="${BASH_REMATCH[1]}"

# The package's own version, the first `version =` line of runtime/Cargo.toml.
CARGO_VERSION="$(sed -n 's/^version = "\([^"-]*\).*"$/\1/p' "${REPO_ROOT}/runtime/Cargo.toml" | head -1)"
readonly CARGO_VERSION
[[ "${TAG_VERSION}" == "${CARGO_VERSION}" ]] \
    || fail "${TAG} does not match runtime/Cargo.toml's version ${CARGO_VERSION}; bump it first"

echo "prerelease=${prerelease}"
