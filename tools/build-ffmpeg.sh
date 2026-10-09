#!/usr/bin/env bash
# Builds the FFmpeg that releases bundle for audio: shared libraries (libavcodec, libavutil)
# with only the WMA Pro decoder, which XMA audio needs, and nothing that would make it GPL.
#
#   tools/build-ffmpeg.sh <install prefix> <work directory>
#
# FFmpeg is LGPL 2.1 or later; the project stays MIT because FFmpeg ships as separate,
# replaceable libraries with its license and exact source (see THIRD_PARTY_NOTICES.md). The
# source tarball is left in the work directory for the release to attach.
set -euo pipefail

readonly FFMPEG_VERSION="9.0.1"
readonly FFMPEG_SHA256="cf38e0e28c7e5605942c4a77755349b0145804a397af37eb1fb4c77cb237f635"
readonly FFMPEG_URL="https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz"
# Never add --enable-gpl, --enable-version3 or --enable-nonfree: the check below fails the
# build unless FFmpeg reports the plain LGPL.
readonly CONFIGURE_FLAGS=(
    --disable-everything
    --enable-decoder=wmapro
    --enable-shared
    --disable-static
    --disable-programs
    --disable-doc
    --disable-network
    --disable-autodetect
    # WMA Pro decoding is light; skipping x86 assembly spares every build machine NASM.
    --disable-x86asm
    --disable-avformat
    --disable-avfilter
    --disable-avdevice
    --disable-swscale
    --disable-swresample
)
readonly EXPECTED_LICENSE="License: LGPL version 2.1 or later"

fail() {
    echo "error: $*" >&2
    exit 1
}

[[ $# -eq 2 ]] || fail "usage: $0 <install prefix> <work directory>"
mkdir -p "$1" "$2"
PREFIX="$(cd "$1" && pwd)"
readonly PREFIX
WORK="$(cd "$2" && pwd)"
readonly WORK
readonly TARBALL="${WORK}/ffmpeg-${FFMPEG_VERSION}.tar.xz"
readonly SOURCE="${WORK}/ffmpeg-${FFMPEG_VERSION}"

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

[[ -f "${TARBALL}" ]] || curl -sSfL -o "${TARBALL}" "${FFMPEG_URL}"
[[ "$(sha256 "${TARBALL}")" == "${FFMPEG_SHA256}" ]] || fail "${TARBALL} does not match the pinned SHA-256"
rm -rf "${SOURCE}"
tar -xJf "${TARBALL}" -C "${WORK}"

platform_flags=()
case "$(uname -s)" in
    # The runtime finds the libraries through its rpath (lib/ beside it), so name them by it.
    Darwin) platform_flags+=(--install-name-dir='@rpath') ;;
    # Under MSYS2 with the MSVC environment loaded: DLLs plus import libraries for the MSVC linker.
    MINGW* | MSYS*) platform_flags+=(--toolchain=msvc) ;;
esac

cd "${SOURCE}"
./configure --prefix="${PREFIX}" "${CONFIGURE_FLAGS[@]}" "${platform_flags[@]}" | tee "${WORK}/configure.log"
grep -qx "${EXPECTED_LICENSE}" "${WORK}/configure.log" \
    || fail "FFmpeg's configure did not report '${EXPECTED_LICENSE}'; refusing to build a non-LGPL FFmpeg"
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 2)"
make install
# The license travels with the libraries (tools/package-release.sh copies it into licenses/).
mkdir -p "${PREFIX}/share/licenses"
cp COPYING.LGPLv2.1 "${PREFIX}/share/licenses/FFmpeg-LGPL-2.1.txt"
echo "FFmpeg ${FFMPEG_VERSION} (LGPL) installed in ${PREFIX}; source: ${TARBALL}"
