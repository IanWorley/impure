<img src="assets/icon.svg" width="64" height="64" alt="">

# pure-decomp

An early-stage decompilation of **Pure (2008), Xbox 360**, with the long-term goal of a playable port.

The immediate goal is to reconstruct and verify the game's behavior. Reproducing the original executable byte for byte is outside the initial scope.

## Current status

There are two tracks:

- **Analysis.** `tools/decompile.sh` turns your own `default.xex` into Ghidra pseudo-C, written locally to `local/decomp/`.
- **Static recompilation.** `tools/recompile.sh` translates your `default.xex` into C++ with XenonRecomp, written locally to `local/recomp/`. The Rust runtime in `runtime/`, built on Bevy, links that code and provides the console underneath it: memory, threads and synchronization, files from your disc directory, a stand-in GPU command processor, and audio and system services. The game boots, loads its assets, runs its main loop, responds to the keyboard and a gamepad as a controller, and creates and loads its save. Its draws run on the GPU through wgpu, with multisampling, depth and stencil, so its start-up screens, title screen, menus and loading screens appear in the window. Audio is silent.

The runtime builds for macOS, Windows and Linux on x86-64 and arm64. Without your copy of the game, it builds with no game code at all.

See [the tooling guide](docs/tooling.md) for extraction, executable inspection, and Ghidra setup, and [the analysis log](docs/analysis.md) for the target fingerprint.

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

This repository ships no game data: no executables, disc images, audio, textures, models, movies, fonts, or other assets, and no decompiler or recompiler output. You need your own copy of the game: every tool reads it as input. `tools/decompile.sh`, `tools/recompile.sh` and the runtime all refuse to run unless the executable's SHA-256 matches the recorded release.

Original binaries, extracted assets, decompiler exports, and local disassembler projects stay outside version control. Commit analysis notes, reconstruction code, and reproducible tooling as they become available.

`tools/check-no-game-content.sh` fails if a tracked file looks like game data or generated code; CI runs it on every push and pull request. Never publish a runtime built with the `recompiled` feature: it contains the game's translated code. CI builds only the game-free runtime. It lints the feature's Rust code, but whenever `CI` is set `runtime/build.rs` skips the generated C++, so a build with the feature fails to link. That guards against accidents; it cannot stop someone who builds locally.

## Notice

This is an unofficial fan project for research and preservation. It is not affiliated with, endorsed by, or sponsored by Disney, Black Rock Studio, or Microsoft. Pure is a trademark of its owner, used here only to identify the game the tools work with. You must own a legitimate copy of the game; this repository contains none of its content.
