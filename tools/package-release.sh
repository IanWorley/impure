#!/usr/bin/env bash
# Assembles one platform's release archive: the runtime (which never contains game code), the
# LGPL FFmpeg it plays audio with, the SDK it builds the game library from on the player's
# machine (src/builder/), and every license.
#
#   tools/package-release.sh <version> <platform> <output directory>
#
# Needs FFMPEG_PREFIX (an install from tools/build-ffmpeg.sh), XenonRecomp built in Release mode
# at XENON_RECOMP_DIR (default local/tools/XenonRecomp; see docs/tooling.md) with the Ninja or
# Makefile generator, and cargo-about.
set -euo pipefail

fail() {
    echo "error: $*" >&2
    exit 1
}

[[ $# -eq 3 ]] || fail "usage: $0 <version> <platform> <output directory>"
readonly VERSION="$1"
readonly PLATFORM="$2"
mkdir -p "$3"
OUTPUT="$(cd "$3" && pwd)"
readonly OUTPUT
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPO_ROOT
readonly XENON="${XENON_RECOMP_DIR:-${REPO_ROOT}/local/tools/XenonRecomp}"
[[ -n "${FFMPEG_PREFIX:-}" ]] || fail "set FFMPEG_PREFIX to an install from tools/build-ffmpeg.sh"
readonly NAME="pure-runtime-${VERSION}-${PLATFORM}"
readonly STAGE="${OUTPUT}/${NAME}"
readonly FFMPEG_LIBRARIES=(avcodec avutil)

# Where the runtime finds FFmpeg: lib/ beside it through its rpath, or beside it on Windows,
# which searches the executable's directory.
exe=""
library_dir="${STAGE}/lib"
rustflags=""
case "$(uname -s)" in
    Darwin) rustflags="-C link-arg=-Wl,-rpath,@executable_path/lib" ;;
    # DT_RPATH rather than RUNPATH, so libavcodec finds libavutil there too.
    Linux) rustflags="-C link-arg=-Wl,-rpath,\$ORIGIN/lib -C link-arg=-Wl,--disable-new-dtags" ;;
    MINGW* | MSYS* | CYGWIN*)
        exe=".exe"
        library_dir="${STAGE}"
        ;;
    *) fail "unsupported platform $(uname -s)" ;;
esac

# Its own target directory: this build links the bundled FFmpeg through an rpath, which would
# leave a developer's usual build unable to find their system FFmpeg.
readonly TARGET_DIR="${REPO_ROOT}/runtime/target/package"
(
    cd "${REPO_ROOT}/runtime"
    FFMPEG_DIR="${FFMPEG_PREFIX}" RUSTFLAGS="${rustflags}" CARGO_TARGET_DIR="${TARGET_DIR}" \
        cargo build --release --locked --features audio
)

rm -rf "${STAGE}"
mkdir -p "${STAGE}" "${library_dir}" "${STAGE}/licenses"
cp "${TARGET_DIR}/release/pure-runtime${exe}" "${STAGE}/"

# FFmpeg, under the names the runtime was linked against.
for library in "${FFMPEG_LIBRARIES[@]}"; do
    case "$(uname -s)" in
        Darwin) name="$(basename "$(otool -D "${FFMPEG_PREFIX}/lib/lib${library}.dylib" | tail -1)")" ;;
        Linux) name="$(objdump -p "${FFMPEG_PREFIX}/lib/lib${library}.so" | awk '/SONAME/ { print $2 }')" ;;
        *) name="$(basename "$(ls "${FFMPEG_PREFIX}/bin/${library}"-*.dll)")" ;;
    esac
    source_dir="${FFMPEG_PREFIX}/lib"
    [[ -n "${exe}" ]] && source_dir="${FFMPEG_PREFIX}/bin"
    cp -L "${source_dir}/${name}" "${library_dir}/${name}"
done

# The SDK: XenonRecomp's tools and loader, the third-party sources the game library compiles,
# the glue and title fixes, and XenonRecomp's configuration for Pure.
readonly SDK="${STAGE}/sdk"
mkdir -p "${SDK}/bin" "${SDK}/thirdparty/libmspack/libmspack" "${SDK}/recomp"
for tool in XenonAnalyse XenonRecomp; do
    cp "${XENON}/build-release/${tool}/${tool}${exe}" "${SDK}/bin/"
done
cp -R "${XENON}/XenonUtils" "${SDK}/XenonUtils"
cp -R "${XENON}/thirdparty/simde" "${XENON}/thirdparty/tiny-AES-c" "${XENON}/thirdparty/TinySHA1" "${SDK}/thirdparty/"
cp -R "${XENON}/thirdparty/libmspack/libmspack/mspack" "${SDK}/thirdparty/libmspack/libmspack/"
cp -R "${REPO_ROOT}/runtime/cpp" "${SDK}/cpp"
cp "${REPO_ROOT}"/recomp/*.toml "${SDK}/recomp/"

cp "${REPO_ROOT}/LICENSE" "${REPO_ROOT}/THIRD_PARTY_NOTICES.md" "${REPO_ROOT}/tools/release/README.txt" "${STAGE}/"
cp "${FFMPEG_PREFIX}/share/licenses/FFmpeg-LGPL-2.1.txt" "${STAGE}/licenses/"
cp "${XENON}/LICENSE.md" "${STAGE}/licenses/XenonRecomp-MIT.md"
cp "${XENON}/thirdparty/simde/COPYING" "${STAGE}/licenses/SIMDe-MIT.txt"
cp "${XENON}/thirdparty/libmspack/libmspack/COPYING.LIB" "${STAGE}/licenses/libmspack-LGPL-2.1.txt"
cp "${XENON}/thirdparty/tiny-AES-c/unlicense.txt" "${STAGE}/licenses/tiny-AES-c-Unlicense.txt"
cp "${REPO_ROOT}/tools/release/FiraMono-OFL.txt" "${STAGE}/licenses/"
(
    cd "${REPO_ROOT}/runtime"
    cargo about generate --locked -c about.toml --features audio \
        ../tools/release/about.hbs -o "${STAGE}/licenses/rust-crates.html"
)

# Nothing from the game may ever ship: no executables, images or generated code...
game_files="$(find "${STAGE}" -iname '*.xex' -o -iname '*.iso' -o -name 'ppc_recomp*' -o -name 'ppc_func_mapping*' -o -name 'ppc_config.h')"
[[ -z "${game_files}" ]] || fail "game content in the release: ${game_files}"
# ...and the runtime must not contain recompiled functions.
if symbols="$(nm "${STAGE}/pure-runtime${exe}" 2>/dev/null)"; then
    ! grep -qE '__imp__sub_|pure_guest_load' <<< "${symbols}" || fail "the runtime contains game code symbols"
fi

if [[ -n "${exe}" ]]; then
    archive="${OUTPUT}/${NAME}.zip"
    (cd "${OUTPUT}" && 7z a -tzip "${NAME}.zip" "${NAME}" > /dev/null)
else
    archive="${OUTPUT}/${NAME}.tar.gz"
    tar -czf "${archive}" -C "${OUTPUT}" "${NAME}"
fi
echo "${archive}"
