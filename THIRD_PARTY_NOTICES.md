# Third-party software in releases

This project's own code is MIT licensed (`LICENSE`). A release also contains the software below,
each under its own license; the full texts are in `licenses/`. Releases contain no part of the
game: the player supplies their own copy, and the game's code is built from it on their machine.

## FFmpeg (audio decoding)

- **Files:** `lib/libavcodec*`, `lib/libavutil*` (macOS, Linux); `avcodec-*.dll`, `avutil-*.dll`
  (Windows).
- **License:** GNU Lesser General Public License, version 2.1 or later —
  `licenses/FFmpeg-LGPL-2.1.txt`.
- **Version and source:** FFmpeg 9.0.1, unmodified. The exact source tarball is attached to the
  same GitHub release (`ffmpeg-9.0.1.tar.xz`, SHA-256
  `cf38e0e28c7e5605942c4a77755349b0145804a397af37eb1fb4c77cb237f635`), and is also available
  from <https://ffmpeg.org/releases/>.
- **How it was built:** `tools/build-ffmpeg.sh` in this repository, with only the WMA Pro
  decoder enabled, as shared libraries, and never with `--enable-gpl`, `--enable-version3` or
  `--enable-nonfree`.
- **Replacing it:** the libraries are separate files that the runtime loads at start-up. You may
  replace them with your own build of FFmpeg 9.0 (same major library versions); the runtime
  finds them in `lib/` beside it (beside `pure-runtime.exe` on Windows).

## XenonRecomp, XenonAnalyse and XenonUtils

- **Files:** `sdk/bin/`, `sdk/XenonUtils/`.
- **License:** MIT — `licenses/XenonRecomp-MIT.md`. Built from
  <https://github.com/hedge-dev/XenonRecomp> at commit
  `ddd128bcca99fe8bfbb99bea583c972351fa6ace` with `tools/patches/xenonrecomp.patch`.

## Sources compiled into the game library on the player's machine

Shipped as source in `sdk/thirdparty/`:

- **SIMDe** — MIT, `licenses/SIMDe-MIT.txt`.
- **libmspack** (`lzxd.c`, LZX decompression) — LGPL 2.1, `licenses/libmspack-LGPL-2.1.txt`.
- **tiny-AES-c** — The Unlicense (public domain), `licenses/tiny-AES-c-Unlicense.txt`.
- **TinySHA1** — its permission notice is at the top of `sdk/thirdparty/TinySHA1/TinySHA1.hpp`.

## Rust crates in the runtime

The runtime is built from many Rust crates (Bevy, wgpu and others), under MIT, Apache-2.0 and
similar permissive licenses. Each crate, its license and its notice are listed in
`licenses/rust-crates.html`, generated with `cargo about`.

## Font

The launcher's text uses Fira Mono (a subset embedded by Bevy), © The Mozilla Foundation and
Telefonica S.A., under the SIL Open Font License 1.1 — `licenses/FiraMono-OFL.txt`.
