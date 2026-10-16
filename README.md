<img src="assets/icon.svg" width="64" height="64" alt="">

# pure-decomp

An early-stage decompilation of **Pure (2008), Xbox 360**, with the long-term goal of a playable port.

The immediate goal is to reconstruct and verify the game's behavior. Reproducing the original executable byte for byte is outside the initial scope.

## Current status

There are two tracks:

- **Analysis.** `tools/decompile.sh` turns your own `default.xex` into Ghidra pseudo-C, written locally to `local/decomp/`.
- **Static recompilation.** XenonRecomp translates your `default.xex` into C++, and the Rust runtime in `runtime/`, built on Bevy, provides the console underneath it: memory, threads and synchronization, files from your disc, a stand-in GPU command processor, and audio and system services. The runtime never contains the game's code: the first time you open your copy, it translates and compiles it on your machine into a library it then loads (`runtime/src/builder/`), and later launches load that at once. Every mode of the game plays: the tutorial, World Tour with its garage, Race, Sprint and Freestyle events to their results, Trial, and the menus and saves, at about 30 frames a second on an Apple M4. Its draws run on the GPU through wgpu, and audio plays when built with the `audio` feature, which decodes the game's XMA sound with FFmpeg.

The runtime builds for macOS, Windows and Linux on x86-64 and arm64, and the game has run on Linux x86-64 and on an Apple M4 Mac, where it plays the tutorial at about 18 frames a second in a release build. Without your copy of the game, it builds with no game code at all.

See [the tooling guide](docs/tooling.md) for extraction, executable inspection, and Ghidra setup, and [the analysis log](docs/analysis.md) for the target fingerprint.

## Playing

Download a release for your platform from the repository's Releases page, or build the runtime (`cargo build --release --features audio` in `runtime/`). Start `pure-runtime`, click **Open Pure disc image or default.xex...** and choose your own disc image of Pure (World release, no title update). The first time, the runtime copies the game's files out of the image and builds the game with your C++ compiler, which takes about 10 to 15 minutes. Afterwards the window opens on a home screen offering that disc image with **Play**, which starts at once, and the Open button to point it at a different one. It needs Clang (`xcode-select --install` on macOS, `winget install LLVM.LLVM` on Windows, your distribution's `clang` on Linux). From a terminal, `pure-runtime <disc image or default.xex>` opens a game directly and `--build-game` prepares it with progress printed there. See [the tooling guide](docs/tooling.md) for details and for how releases are made.

## Starting an investigation

1. Place the extracted game files in `originals/`, preserving their directory structure. Start with `default.xex` and include the accompanying game data if available. This directory is ignored by Git.
2. Record the release, region, title update state, file sizes, and SHA-256 hashes in [the analysis log](docs/analysis.md). Keep the original files unchanged; use `local/` for extracted or transformed working copies.
3. Inspect the executable's XEX headers to determine its load address, entry point, compression/encryption state, sections, and imports. Extract a usable image with an Xbox 360 aware tool before treating it as raw PowerPC code.
4. Import the executable into Ghidra using an Xbox 360 XEX loader that supports the observed format. Verify the loader's interpretation of Xenon PowerPC code, endianness, addresses, and section boundaries before trusting its output. Record the exact tools and versions used.
5. Map startup, imported platform calls, and references to diagnostic strings. Select a small function with clear inputs and outputs and few platform dependencies for the first reconstruction.
6. Record the original address and evidence for each recovered function. Verify its behavior against the original code before marking it as recovered.

Keep addresses tied to the exact executable hash: addresses and layouts can change between releases and title updates. Treat decompiler output as a hypothesis to check against disassembly.

## Reconstruction approach

Future recovered game code will use C++ because that keeps native layouts, calling conventions, and eventual platform integration manageable. The current CMake build uses Clang for the inspection tool; the game build will be designed when reconstruction begins.

Use explicit types, named constants, and comments explaining uncertain interpretations. Preserve observable behavior, including integer widths, floating-point behavior, and data layouts. Give inferred symbols descriptive names and label their confidence in the analysis log.

Keep Xbox 360 platform dependencies identifiable during recovery. Design replacement interfaces for the eventual port when their actual usage is understood.

Tests should cover specific recovered behavior, using observations from the original code or execution where possible. A successful compilation alone does not establish correctness.

## What this repository does not contain

This repository and its releases ship no game data: no executables, disc images, audio, textures, models, movies, fonts, or other assets, and no decompiler or recompiler output. You need your own copy of the game: every tool reads it as input. `tools/decompile.sh` and the runtime both refuse to use an executable whose SHA-256 does not match the recorded release.

Original binaries, extracted assets, decompiler exports, and local disassembler projects stay outside version control. Commit analysis notes, reconstruction code, and reproducible tooling as they become available.

`tools/check-no-game-content.sh` fails if a tracked file looks like game data or generated code; CI runs it on every push and pull request. The runtime itself never contains the game's code, so every build of it, and every release, can be published: the game library the builder compiles from your copy stays in your data directory and must never be shared. The release packaging also refuses an archive containing game files or a runtime with recompiled functions in it.

## Notice

This project's code is MIT licensed (`LICENSE`). Releases also contain third-party software under its own licenses, among them FFmpeg under the LGPL as separate, replaceable libraries; see `THIRD_PARTY_NOTICES.md`.

This is an unofficial fan project for research and preservation. It is not affiliated with, endorsed by, or sponsored by Disney, Black Rock Studio, or Microsoft. Pure is a trademark of its owner, used here only to identify the game the tools work with. You must own a legitimate copy of the game; this repository contains none of its content.
