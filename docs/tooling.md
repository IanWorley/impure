# Local analysis tools

These commands assume Linux or macOS, Git, CMake, Clang 18 or newer, and 7-Zip. Game files and third-party checkouts remain ignored by Git.

Installed for this setup: 7-Zip 26.00, Clang 21.1.8, OpenJDK 21, Ghidra 12.1.3, and XEXLoaderWV 12.1.3. CMake and Git were already available. GCC did not build this XenonRecomp revision; use Clang as shown below.

On Ubuntu, the additional packages can be installed with:

```sh
sudo apt-get install 7zip clang openjdk-21-jdk-headless
```

On macOS, Xcode's command line tools provide Clang and Git; Homebrew provides the rest (`brew install cmake sevenzip ninja`). Homebrew's 7-Zip is `7zz`, not `7z`. The static recompilation (below) was also set up this way on an Apple M4 (macOS 26, Apple Clang 21, CMake 4.4, 7-Zip 26.00); Ghidra was not.

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

### XenonRecomp

XenonRecomp needs a small patch, `tools/patches/xenonrecomp.patch`. It skips zeroed `.pdata` entries, which otherwise produce duplicate `sub_0` functions. It also stops emitting `__attribute__((alias))`, which Mach-O does not support, so the output links on every target whatever host generated it. Integer division yields 0 for a zero divisor or signed overflow: the Xenon CPU does not trap there, but C++ division traps on x86-64 (SIGFPE). Switches dispatch on the index register's low word, as the guest's bounds check and table lookup do; the upper word can hold anything, and with upstream's 64-bit index an out-of-range value jumped to a wild address. It routes every generated memory access through `PPC_MEMORY_ADDRESS`, which redirects the 4 KiB-page physical memory view (see Physical memory below). Finally, it implements the 18 instructions this executable uses that upstream does not (see [the analysis log](analysis.md)). Build in Release mode: Pure's code section sizes trip a debug-only assertion in XenonAnalyse.

```sh
git -C local/tools/XenonRecomp apply ../../../tools/patches/xenonrecomp.patch
cmake -S local/tools/XenonRecomp -B local/tools/XenonRecomp/build-release \
  -DCMAKE_C_COMPILER=clang -DCMAKE_CXX_COMPILER=clang++ -DCMAKE_BUILD_TYPE=Release
cmake --build local/tools/XenonRecomp/build-release --target XenonAnalyse XenonRecomp --parallel
```

The runtime's builder runs these tools itself (below); a source checkout uses this build, and releases ship their own in `sdk/bin/`. It runs XenonAnalyse to find jump tables and appends `recomp/pure_switch_tables.toml`, then writes the XenonRecomp config: the executable's path followed by `recomp/pure.toml`, which holds only addresses. XenonRecomp generates about 330 MB of C++ in about a minute, with its warnings in `recomp.log` beside it. That output lives only in the data directory and must never be committed or shared.

`recomp/pure.toml`'s `functions` list comes from `tools/find_switch_functions.py`. Regenerate it with the mapped image from `inspect_xex`:

```sh
python3 tools/find_switch_functions.py local/entry-analysis/analysis-image.bin <data directory>/games/<SHA-256>/recomp/switch_tables.toml
```

The inline jump tables after unchecked dispatches come from `tools/find_inline_switch_tables.py`, given the same image and XenonAnalyse's tables; its output is appended to `recomp/pure_switch_tables.toml`:

```sh
python3 tools/find_inline_switch_tables.py local/entry-analysis/analysis-image.bin <data directory>/games/<SHA-256>/recomp/switch_tables.toml
```

[The analysis log](analysis.md) records the evidence for every address.

### Building and running the runtime

The runtime is a Rust crate in `runtime/`. It needs Rust 1.96 or newer (`rustup`), and building the game needs Clang.

```sh
cd runtime
cargo run --release -- ../originals/disc/default.xex     # or your disc image (.iso)
cargo run --release -- ../originals/disc/default.xex --build-game   # build only, progress in the terminal
cargo run --release                                     # the launcher: the last game, or an Open dialog
```

To play, build with `--release`: the debug build also runs wgpu's and the Metal bindings' debug assertions on every draw. On an Apple M4 the tutorial's 3D runs at about 19 frames a second in a release build while the camera holds still, and 25 to 27 while riding, against 2 to 3 in a debug one. The command processor (walking packets and recording draws on one thread) is the bottleneck, not the GPU.

`--input-script <file>` plays back keys and takes frame captures on chosen frames, for unattended runs (see `runtime/src/input_script.rs` for the format). A capture saves the front buffer, as the window shows it, to a PNG file; it reads the display texture itself, so it needs no screen-recording permission and works with the window hidden. Bevy's own window screenshots come out black on macOS.

The runtime never contains the game's code. Without a game on the command line, the window opens on a home screen: an Open button for the system's Open dialog, and **Play** for the disc image or `default.xex` chosen last time (remembered by its absolute path in the data directory's `last-game`; `--choose` leaves it out). Given a disc image or `default.xex`, from either, it prepares the game (`runtime/src/prepare.rs`):

1. From a disc image, it reads the Xbox file system (XDVDFS, `runtime/src/xiso.rs`), checks `default.xex`'s SHA-256, and copies the disc's files once to `games/<SHA-256>/disc/` in the data directory (about 7 GB).
2. The builder (`runtime/src/builder/`) translates the executable with XenonAnalyse and XenonRecomp into `games/<SHA-256>/recomp/`, then compiles that, `cpp/glue.cpp`, `cpp/patches.cpp`, the import forwarders and XenonUtils' XEX loader with the player's Clang, in parallel, and links them into `games/<SHA-256>/pure_game.{dylib,so,dll}`: about 1,700 large files at `-O2`, 10 minutes on an Apple M4. Each object is cached under a key of its source, the compiler, the flags and the shared headers, so a runtime update that changes the glue recompiles only the glue, and a recompiler fix only the files it changed; a stamp beside the library makes a warm start a fraction of a second.
3. It loads the library (`runtime/src/game_module.rs`). The library calls the runtime only through a table of callbacks it is handed (`cpp/host.h`), so it has no undefined symbols, and it exports only the entry points the runtime uses. A library built for a different interface version is refused.

The builder finds its tools and sources in `PURE_SDK_DIR`, or `sdk/` beside the executable (releases), or else this checkout (`XENON_RECOMP_DIR`, default `local/tools/XenonRecomp`), and the compiler as `PURE_CXX`, `xcrun --find clang++` on macOS (with the SDK `xcrun --show-sdk-path` names), or `clang++` on PATH.

Then the runtime:

1. Checks the executable's SHA-256.
2. Maps the guest's 4 GiB address space and loads the image there with XenonUtils, and copies the XEX header into guest memory.
3. Gives each variable import a zero-filled block (and keeps `KeTimeStampBundle` current every millisecond, as the console's clock interrupt does: the SDK's GetTickCount and the middleware read it directly, and while it stood still the logo movie never advanced, its sound looped its first 0.6 s, and the tutorial's lap never counted), and mounts the directory holding `default.xex` read-only as the disc (`game:`, `d:`, `\Device\Cdrom0`).
4. Runs the entry point on its own host thread, with a stack and processor control region.

Each import is a typed handler in `runtime/src/kernel/`, grouped by subsystem: memory (virtual and physical, with the console's three physical views), threads, synchronization objects and waits, timers, files, video, audio, input, users and system settings. Their logic lives in plain modules beside `main.rs` with focused tests that need no game. A few pieces stand in for hardware:

- **GPU.** A command processor thread walks the PM4 packets the game submits, applies register writes, performs the fence writes and interrupts the game waits on, stalls on `WAIT_REG_MEM` as the real GPU does, and honours predicated tiling (packets that only run for some screen tiles). A graphics interrupt thread delivers 60 Hz vertical blanks into the game's interrupt handler, which paces its frames. Each shader the game loads is decoded from Xenos microcode (`runtime/src/shader.rs`), translated to WGSL (`runtime/src/translate.rs`) and checked with naga, the validator wgpu uses. Draws run on the host GPU through wgpu, on Bevy's device (`runtime/src/renderer.rs`). The shaders read constants, vertices, indices and textures from guest data themselves. Each EDRAM surface is a host texture at sample resolution (`runtime/src/edram.rs`): a 4x multisampled surface stores 2x2 samples per pixel, so surfaces that alias in EDRAM alias on the host too, and a resolve averages the samples of its rectangle into a host texture named by its destination address. Depth and stencil surfaces work the same way. The swap packet that VdSwap queues copies the presented front buffer into the window (`runtime/src/display.rs`). Textures are untiled and decoded on the CPU (`runtime/src/texture.rs`: 8-, 16- and 32-bit formats, DXT1-5, and 32-bit and 4x16-bit float), uploaded at most once a frame, and only again when their guest bytes change. Colour surfaces are 8-bit RGBA or 32-bit float, and depth resolves copy depth into a float texture; read as an 8888 texture, a depth resolve is the packed words EDRAM held, and colour drawn where a depth surface lies (with its 40-sample tile halves exchanged, as EDRAM keeps depth) becomes that surface's depth before the next draw tests it, which is how the game carries depth from its tiled 2x pass into its 1x pass; depth written over a colour surface likewise becomes its colour (the game clears colour that way). Draws with clipping off have their depth clamped instead, and pixel shaders get the pixel parameters (`ps_param_gen`). Draws honour the alpha test and alpha to coverage (`RB_COLORCONTROL`) and cull faces as `PA_SU_SC_MODE_CNTL` asks; shaders sample 2D and cube textures at their base level, with fetch offsets and `setTextureLOD`. Not reproduced yet: other render target formats, mip levels, 1D and 3D textures, and writing resolved data back to guest memory. The renderer reports each of these once, as it meets it. Draws and resolves are recorded into one command buffer per frame and submitted at the swap by a separate thread (`runtime/src/submitter.rs`), which takes every upload and submission in order, so the command processor records the next frame while the previous one is finished, uploaded and presented, with each draw's constants and guest memory in shared batch buffers (each range of guest memory once a batch, and ranges inside it sharing it until the title submits more commands: titles draw from one vertex buffer through ranges that start further in each draw) and indexed lists and strips drawn from a host index buffer, where the strip reset index the title sets (`VGT_MULTI_PRIM_IB_RESET_INDX`, when `PA_SU_SC_MODE_CNTL` enables it) becomes the host's restart index: drawn through, each cut joined two strips with long triangles through one vertex, which covered the tutorial in spikes and giant flat shapes; a submission per draw cost more than most draws on llvmpipe. On a software GPU, 2x multisampled surfaces are drawn at one sample per pixel.
- **Audio.** The audio driver calls the game's mixer at the console's frame rate. Each time the game enables an XMA context, the decoder runs one pass as the hardware would (`runtime/src/xma.rs`): it steps through the compressed stream frame by frame (packets, frame lengths, loops), swaps and releases input buffers, fills the output ring, and clears the output buffer's valid bit when the ring is full. The game's audio streaming waits on these details: with a cruder decoder it froze while loading the tutorial. Without the `audio` feature, each frame decodes to silence. With it (`cargo build --release --features audio`), FFmpeg decodes the samples (`runtime/src/xma_ffmpeg.rs`): every XMA frame is a WMA Pro frame, so each goes to FFmpeg's WMA Pro decoder in a packet of its own, which on real sounds matches FFmpeg's XMA2 decoder sample for sample without the 4,096 samples that decoder holds back. Pure's FMOD plays every sound through contexts marked mono, stereo music and effects included, so a stream's channel count is judged from its first frames (decoded both ways; the way FFmpeg parses without errors wins), and a stereo stream fills 8 output blocks a frame. The frames the game's mixer submits (six channels of 256 big-endian float samples each) are downmixed to stereo and played through the default output device with cpal; `PURE_AUDIO_CAPTURE=<file.wav>` records them too. The feature links FFmpeg dynamically, so FFmpeg's LGPL code stays a separate library and this repository stays MIT: from a system install for local builds (`brew install ffmpeg pkgconf`, or `libavcodec-dev pkg-config libasound2-dev` on Debian and Ubuntu), and in releases from a minimal build bundled beside the runtime (see Releases). Builds without the feature, CI's included, need neither.

- **Title fixes.** `runtime/cpp/patches.cpp` replaces a few recompiled functions (the generated definitions are weak symbols) where the game's own code races in ways the console's timing hides and a host's does not. Today these are its job system's lost activation and lost wakeup, which left the main thread waiting forever a few hundred frames into the tutorial. The runtime logs `3 title fixes in place` at start-up from the library's `pure_title_fix_count`. (When the game was linked into the runtime from a static library, Apple's linker, which loads archive members only for undefined symbols, left this file out without a word until something referenced it; the library now links every object directly.) Any wait with no timeout that lasts five seconds is logged (`wait: thread … has waited 5s from … for any of [object Type state N]`), and `RUST_LOG=semaphore=debug` logs every semaphore's creation and releases, with the thread and caller.
- **Physical memory.** The console's three physical memory views (0xA0000000, 0xC0000000 and 0xE0000000, the last offset by one page) all show the same memory, so titles can read through one view what they wrote through another; D3D relies on it. The first two are mapped onto one shared block. The 4 KiB-page view's one-page offset is finer than Apple silicon (16 KiB pages) or Windows (64 KiB units) can map, so instead every access to it is redirected to the 0xA0000000 view's address for the same physical byte: the patched recompiler wraps each generated address in `PPC_MEMORY_ADDRESS`, and the runtime's `memory::host_address` does the same. So the GPU's physical addresses are all read through the 0xA0000000 view (`physical_memory::guest_address`), including memory the title has just freed while a draw still uses it, as on the console; the runtime refuses to start on a host that cannot share the first two views. Before the redirect, on Apple silicon the title's writes through that view never reached the GPU's reads through the others: the intro movie's vertex shader, written inline into a command buffer, arrived without its control-flow words, and the movie played black.
- **Input.** The keyboard and the first connected gamepad together drive the first controller (`runtime/src/controller.rs`). Gamepads use Bevy's gilrs backend: built in on Windows and macOS, and on Linux behind the `linux-gamepads` feature, which needs libudev's development files (`libudev-dev`).
- **Saves.** Content packages are directories under the runtime's data directory: `$XDG_DATA_HOME/pure-runtime` (or `~/.local/share/pure-runtime`) on Linux, `~/Library/Application Support/pure-runtime` on macOS and `%APPDATA%\pure-runtime` on Windows. `PURE_DATA_DIR` overrides it, for example to keep test runs away from real saves. Every save there was made by the one local profile, which is what `XamContentGetCreator` reports for it; the game asks before it overwrites a save.

An import the runtime does not implement is logged with its arguments and caller, and stops the calling guest thread. `--exit-on-stop` closes the app when a guest thread stops, for scripted runs. On Linux and macOS a host crash (segmentation fault or bus error) is reported in the log before the process exits, with the faulting address and a backtrace naming the generated guest functions.

Every build of the runtime is game-free, so CI builds the whole runtime: `.github/workflows/ci.yml` builds and tests it natively on the six targets, lints it and its workflows, and runs `tools/check-no-game-content.sh`. Builds and cross-target checks therefore work without the game:

```sh
rustup target add aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc
cargo check --target aarch64-apple-darwin
```

Platform notes:

- Linux windows use X11; Wayland desktops run the runtime through XWayland. Bevy's Wayland backend needs system libraries at build time, which would break cross-target checks.
- On a machine without a display, run under `Xvfb` with Mesa's software Vulkan driver (`mesa-vulkan-drivers`).
- macOS renders through Metal.
- The runtime logs at `info` unless `RUST_LOG` says otherwise. A `RUST_LOG` set in a shell profile (to `warn`, say) hides its progress lines.
- The generated code itself is portable across x86-64 and arm64: SIMDe maps VMX instructions onto SSE or NEON. It must be compiled by Clang for each target.

## Releases

Releases are built by GitHub Actions from a tag and contain no part of the game: the runtime, the tools and sources its builder needs (`sdk/`), a minimal FFmpeg, and every license (`tools/package-release.sh`).

- **Full release:** push a tag `vMAJOR.MINOR.PATCH` (for example `git tag v0.1.0 && git push origin v0.1.0`).
- **Pre-release (release candidate):** push `vMAJOR.MINOR.PATCH-rc.N` (also `-beta.N`, `-alpha.N`). It is published as a GitHub pre-release.
- The tag's `MAJOR.MINOR.PATCH` must match `runtime/Cargo.toml`'s version, so bump that first; release candidates need no separate bump (`tools/release-tag.sh` checks this).
- **Undo:** delete the tag (`git push --delete origin v0.1.0`) and `.github/workflows/retract-release.yml` deletes its release; or run that workflow from the Actions tab with the tag, which deletes both. If the tag goes while the release is still building, nothing is published (`gh release create --verify-tag`). GitHub only runs the tag-deletion trigger from the default branch's copy of the workflow.

`.github/workflows/release.yml` checks the tag, builds one archive per platform with `.github/workflows/build-release.yml` (macOS, Linux and Windows on x86-64 and arm64; `.tar.gz`, or `.zip` on Windows), and publishes them with `SHA256SUMS` and FFmpeg's source. Each build compiles the pinned XenonRecomp with its patch and builds FFmpeg with `tools/build-ffmpeg.sh`, both cached.

**FFmpeg and the LGPL.** The bundled FFmpeg 9.0.1 is built from its pinned, checksummed source with only the WMA Pro decoder, as shared libraries, and never with `--enable-gpl`, `--enable-version3` or `--enable-nonfree`; the build fails unless FFmpeg's configure reports "LGPL version 2.1 or later". The libraries ship as separate files the runtime loads at start-up (`lib/` beside it, found through its rpath; beside the executable on Windows), so a player can replace them. Each archive carries the LGPL text and `THIRD_PARTY_NOTICES.md` with the version and build flags, and the release attaches the exact source tarball. The project's own code stays MIT.

Each archive also lists every Rust crate in the runtime with its license (`licenses/rust-crates.html`, from `cargo about` with `runtime/about.toml`), and carries the licenses of XenonRecomp, SIMDe, libmspack, tiny-AES-c and the launcher's font. Packaging fails if the staged tree holds a game file or the runtime has recompiled functions in it.

Builds are not signed: on macOS a player opens the app the first time with right-click, Open; on Windows, SmartScreen's "Run anyway". Signing needs paid certificates and is not set up.

## Validation completed

- On an Apple M4 (2026-10-08, evening): the game moved out of the runtime into a library built on the player's machine. From the World disc image in a fresh data directory, `--build-game` copied the disc's files, translated the executable and compiled the library in 10 minutes 24 seconds; it exports exactly the 11 entry points and needs nothing from the runtime but its host table. The intro movie's frames matched the statically linked build's exactly, the Single Event race script played identically at 30 frames a second, a warm start took 0.14 seconds, a relocated cache was reused whole, and launching with no argument reopened the remembered game. `cargo check` and clippy passed for all six targets; the release scripts passed shellcheck and the workflows actionlint; FFmpeg 9.0.1 built LGPL-only with the WMA Pro decoder in 1.1 MB.
- On an Apple M4 (macOS 26, 2026-10-08): the pinned XenonRecomp and extract-xiso built with Apple Clang 21; `tools/recompile.sh` regenerated the C++ in 18 seconds; `cargo build --features recompiled` compiled it for arm64 in 14 minutes. The game ran natively through Metal at a steady 60 frames a second up to its menus, and an input script drove it through the start-up screens, the save dialog and into the tutorial lap, with frame captures of each. With the batching, bind group and constants changes and the submit thread of that day, a release build runs the tutorial's 3D at 19 to 27 frames a second (2 to 3 before). A 1,200-second release run presented 18,600 frames, about 15,000 of them in the tutorial's 3D, with no stall, crash or stop line: the job-system fixes hold on arm64's weaker memory ordering. The game-free build's tests, clippy in both modes, and `cargo check` for the other five targets passed there too.
- 7-Zip finished the ISO extraction successfully; extract-xiso listed and extracted the disc.
- Clang built `inspect_xex`, which loaded the real executable and printed section/import information and an entry-point disassembly.
- The inspector rejected an existing output directory, a missing input, and a non-XEX input without creating new output artifacts.
- Ghidra detected the XEX loader, imported the executable with `-noanalysis`, and saved the project successfully.
- `git diff --check` passed. Upstream code emits compiler warnings; the current build succeeds.
- `tools/recompile.sh` rejected a missing input, a non-matching file and an existing output directory. Against the real executable it generated 436,390 functions with no switch case outside its function.
- `cargo build --features recompiled` compiled and linked the generated code on Linux x86-64 in 71 minutes; relinking with cached objects takes about 2 minutes. Under Xvfb with Mesa's lavapipe, the runtime opened its window and ran the entry point (`0x82254D60`) until the first import call, `NtAllocateVirtualMemory` from `0x829C60A8`, where it stopped as designed. It rejected a non-matching executable by hash.
- `cargo check` (without `recompiled`) passed with no warnings for x86-64 and arm64 Linux, macOS and Windows MSVC; `cargo clippy` passed in both modes. Only Linux x86-64 was built and run.
- Once thread affinity set the hardware thread each worker sees (2026-10-07), the tutorial finished loading and its first 3D frames rendered, at about 1,600 draws a frame.
- With thread IDs in thread objects and the job-system fixes (2026-10-08), the tutorial plays on without the command-buffer corruption or the hang: a 7,200-second run under Xvfb passed 1,084 gameplay frames with no stall, where runs had stopped 450 to 650 frames in. Batching draws took a tutorial frame on llvmpipe from about 20 seconds to 4 or 5, and the luminance, swizzle and resolve fixes made the world visible behind the HUD.
- With rendering in place (2026-10-07), the game's start-up screens appear in the window under Xvfb: a black intro video frame, then the safety warning, the ESRB rating and the autosave notice as Return is pressed, and later the loading screen, the title screen, the save dialog and the tutorial's loading screen. On llvmpipe the game then runs at one or two frames per second, since multisampled surfaces render at four times the pixels. On llvmpipe the translated shaders keep all eight raster threads busy, so the game then presents only a few frames per second; the command processor itself takes under 2% of the time.
- With the kernel, GPU stand-in and audio imports in place (2026-10-07), the game boots under Xvfb, reads its assets from the disc directory, creates its worker threads and runs its main loop: a 60-second run presented frames at a steady 60.0 per second with no stop. Fixing XenonRecomp's division and the missed inline jump tables rebuilt 155 and 8 generated files respectively, about 9 minutes and under a minute.
