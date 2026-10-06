# Local analysis tools

These commands assume Linux, Git, CMake, Clang 18 or newer, and 7-Zip. Game files and third-party checkouts remain ignored by Git.

Installed for this setup: 7-Zip 26.00, Clang 21.1.8, OpenJDK 21, Ghidra 12.1.3, and XEXLoaderWV 12.1.3. CMake and Git were already available. GCC did not build this XenonRecomp revision; use Clang as shown below.

On Ubuntu, the additional packages can be installed with:

```sh
sudo apt-get install 7zip clang openjdk-21-jdk-headless
```

## Disc extraction

```sh
mkdir -p originals local/tools
7z x -aos -ooriginals '/home/ubuntu/Pure (World) (En,Fr,De,Es,It,Nl).7z' '*.iso'
git clone https://github.com/XboxDev/extract-xiso.git local/tools/extract-xiso
git -C local/tools/extract-xiso checkout 3f5b62cfe68f000b0e3c8a30104973f3a297948e
cmake -S local/tools/extract-xiso -B local/tools/extract-xiso/build
cmake --build local/tools/extract-xiso/build --parallel 2
local/tools/extract-xiso/build/extract-xiso -l 'originals/Pure (World) (En,Fr,De,Es,It,Nl).iso'
local/tools/extract-xiso/build/extract-xiso -x -s -d originals/disc 'originals/Pure (World) (En,Fr,De,Es,It,Nl).iso'
```

Wait for 7-Zip to finish successfully before reading the ISO. `-s` skips the console system update files; it does not apply a game title update. Use a fresh destination for disc extraction.

## Executable inspection

```sh
git clone https://github.com/hedge-dev/XenonRecomp.git local/tools/XenonRecomp
git -C local/tools/XenonRecomp checkout ddd128bcca99fe8bfbb99bea583c972351fa6ace
git -C local/tools/XenonRecomp submodule update --init --recursive
cmake -S . -B build -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ -DCMAKE_BUILD_TYPE=Debug
cmake --build build --target inspect_xex --parallel 2
build/inspect_xex originals/disc/default.xex local/entry-analysis > local/entry-analysis.txt
```

The inspector reports mapped sections, resolved imports, and an initial disassembly at the entry point. Optional arguments select an address and instruction count, using decimal or `0x` hexadecimal notation. Each invocation needs a new output directory so previous analysis is preserved.

`analysis-image.bin` is a **mapped analysis image**, not a reconstructed executable. XenonUtils decrypts/decompresses the original and modifies import thunks for its recompiler. Those modified bytes must not be treated as original instructions or used to claim byte matching. Import names come from upstream's ordinal tables. The input XEX remains unchanged.

Use this tool with the known game executable. The upstream loader is not a hardened parser for arbitrary untrusted or malformed files. Its disassembly is preliminary; verify instruction semantics and function boundaries before reconstructing code, especially Xenon-specific vector instructions.

XenonRecomp produces instruction translations for static recompilation. Running it does not establish a readable decompilation or supply the runtime needed for a playable port. The current project uses its loader and disassembler for investigation.

## Ghidra and XEX loader

The Ghidra and extension versions must match. This setup uses the following official release artifacts:

```sh
curl -fL https://github.com/NationalSecurityAgency/ghidra/releases/download/Ghidra_12.1.3_build/ghidra_12.1.3_PUBLIC_20260817.zip -o local/tools/ghidra_12.1.3_PUBLIC.zip
curl -fL https://github.com/zeroKilo/XEXLoaderWV/releases/download/12.1.3/ghidra_12.1.3_PUBLIC_20260920_XEXLoaderWV.zip -o local/tools/XEXLoaderWV_12.1.3.zip
unzip -q -n local/tools/ghidra_12.1.3_PUBLIC.zip -d local/tools
unzip -q -n local/tools/XEXLoaderWV_12.1.3.zip -d local/tools/ghidra_12.1.3_PUBLIC/Ghidra/Extensions
```

Ghidra rejects project paths containing directory names starting with a dot, including this workspace's `.t3` parent. Store its projects outside that path:

```sh
mkdir -p /home/ubuntu/pure-decomp-analysis/ghidra-projects
local/tools/ghidra_12.1.3_PUBLIC/support/analyzeHeadless \
  /home/ubuntu/pure-decomp-analysis/ghidra-projects PureTooling \
  -import originals/disc/default.xex -noanalysis \
  -log local/ghidra-import-verified.log
```

Use a new project name for a repeat import, or open the existing project. The command disables automatic analysis. The loader still maps sections, names imports, and processes executable metadata. No decompiler export or source reconstruction is part of this setup.

The loader selected `PowerPC:BE:64:A2ALT-32addr:default` for this executable. That selection verifies loading, not complete support for all Xenon instructions; check vector instruction support before relying on future decompiler output.

## Full decompilation

```sh
tools/decompile.sh [path/to/default.xex] [output-directory]
```

The defaults are `originals/disc/default.xex` and `local/decomp`. The script refuses to run unless you supply your own executable and its SHA-256 matches the one recorded in [the analysis log](analysis.md). It then imports the executable into a new Ghidra project, runs auto-analysis, and runs `tools/ghidra/ExportDecompilation.java`. That script writes:

- `functions/<address>.c`: Ghidra's pseudo-C for each function.
- `index.tsv`: every function's address, name, size, and export status.
- `failures.tsv`: functions the decompiler could not finish.
- `ghidra.log` and `script.log`.

Set `GHIDRA_PROJECT_NAME` for a repeat run, because the script will not reuse an existing project. `GHIDRA_HEADLESS_MAXMEM` defaults to `16G`.

This output is derived from the copyrighted executable. It stays in `local/` and must never be committed. It is also not source code: treat each file as a hypothesis to verify against the disassembly before hand-reconstructing that function in C++.

To open the existing project in a desktop session, run `local/tools/ghidra_12.1.3_PUBLIC/ghidraRun` and select `/home/ubuntu/pure-decomp-analysis/ghidra-projects/PureTooling.gpr`.

## Static recompilation

The project has two tracks. The Ghidra output above supports analysis. The static recompilation translates every instruction of your `default.xex` mechanically into C++ with XenonRecomp. That C++ runs on a Rust runtime built on Bevy, which replaces the console: memory, kernel imports, graphics, audio and input.

### Generating the C++

XenonRecomp needs a small patch, `tools/patches/xenonrecomp.patch`. It skips zeroed `.pdata` entries, which otherwise produce duplicate `sub_0` functions. It also stops emitting `__attribute__((alias))`, which Mach-O does not support, so the output links on every target whatever host generated it. Build in Release mode: Pure's code section sizes trip a debug-only assertion in XenonAnalyse.

```sh
git -C local/tools/XenonRecomp apply ../../../tools/patches/xenonrecomp.patch
cmake -S local/tools/XenonRecomp -B local/tools/XenonRecomp/build-release \
  -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ -DCMAKE_BUILD_TYPE=Release
cmake --build local/tools/XenonRecomp/build-release --target XenonAnalyse XenonRecomp --parallel
tools/recompile.sh [path/to/default.xex] [output-directory]
```

The defaults are `originals/disc/default.xex` and `local/recomp`. Like `decompile.sh`, the script refuses to run unless the executable's SHA-256 matches. It runs XenonAnalyse to find jump tables and appends `recomp/pure_switch_tables.toml`. It then writes the XenonRecomp config: local paths followed by `recomp/pure.toml`, which holds only addresses. Finally it generates about 330 MB of C++ into `ppc/`. Warnings go to `recomp.log`. The output takes about a minute and must never be committed.

`recomp/pure.toml`'s `functions` list comes from `tools/find_switch_functions.py`. Regenerate it with the mapped image from `inspect_xex`:

```sh
python3 tools/find_switch_functions.py local/entry-analysis/analysis-image.bin local/recomp/switch_tables.toml
```

[The analysis log](analysis.md) records the evidence for every address.

### Building and running the runtime

The runtime is a Rust crate in `runtime/`. It needs Rust 1.96 or newer (`rustup`). Building with the `recompiled` feature also needs Clang (`clang-cl` for Windows MSVC targets).

```sh
cd runtime
cargo build --features recompiled
cargo run --features recompiled -- ../originals/disc/default.xex
```

With `recompiled`, `build.rs` compiles the generated C++, `cpp/glue.cpp` and XenonUtils' XEX loader with Clang, and links them in. The first build compiles about 1,700 large files at `-O2` and takes a long time. The objects are cached in `local/recomp/objects/<target>/` and rebuilt only when `tools/recompile.sh` regenerates the sources. Set `PURE_RECOMP_DIR` or `XENON_RECOMP_DIR` to use other locations.

At start-up the runtime:

1. Checks the executable's SHA-256.
2. Maps the guest's 4 GiB address space and loads the image there with XenonUtils.
3. Gives each variable import a zero-filled block.
4. Sets up a main-thread stack and processor control region.
5. Runs the entry point on its own host thread.

No kernel or Xam import is implemented yet. Each call is logged with its arguments and caller, and stops the guest thread. `--exit-on-stop` closes the app when that happens, for scripted runs.

Without `recompiled` (`cargo build`, `cargo check`), the runtime contains no game code and only opens the window. That is all CI builds: `.github/workflows/ci.yml` builds and tests it natively on the six targets, lints it, and runs `tools/check-no-game-content.sh`. CI also lints the `recompiled` feature's Rust code. Whenever the `CI` environment variable is set, `build.rs` skips the generated C++, so a build with the feature fails at link time and CI cannot produce a binary with game code by accident. Builds and cross-target checks therefore work without the game, for example in CI:

```sh
rustup target add aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc
cargo check --target aarch64-apple-darwin
```

Platform notes:

- Linux windows use X11; Wayland desktops run the runtime through XWayland. Bevy's Wayland backend needs system libraries at build time, which would break cross-target checks.
- On a machine without a display, run under `Xvfb` with Mesa's software Vulkan driver (`mesa-vulkan-drivers`).
- The generated code itself is portable across x86-64 and arm64: SIMDe maps VMX instructions onto SSE or NEON. It must be compiled by Clang for each target.

## Validation completed

- 7-Zip finished the ISO extraction successfully; extract-xiso listed and extracted the disc.
- Clang built `inspect_xex`, which loaded the real executable and printed section/import information and an entry-point disassembly.
- The inspector rejected an existing output directory, a missing input, and a non-XEX input without creating new output artifacts.
- Ghidra detected the XEX loader, imported the executable with `-noanalysis`, and saved the project successfully.
- `git diff --check` passed. Upstream code emits compiler warnings; the current build succeeds.
- `tools/recompile.sh` rejected a missing input, a non-matching file and an existing output directory. Against the real executable it generated 436,390 functions with no switch case outside its function.
- `cargo build --features recompiled` compiled and linked the generated code on Linux x86-64 in 71 minutes; relinking with cached objects takes about 2 minutes. Under Xvfb with Mesa's lavapipe, the runtime opened its window and ran the entry point (`0x82254D60`) until the first import call, `NtAllocateVirtualMemory` from `0x829C60A8`, where it stopped as designed. It rejected a non-matching executable by hash.
- `cargo check` (without `recompiled`) passed with no warnings for x86-64 and arm64 Linux, macOS and Windows MSVC; `cargo clippy` passed in both modes. Only Linux x86-64 was built and run.
