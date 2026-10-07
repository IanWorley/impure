# Rendering the game's frames

The window shows what the game draws. Each shader the game loads is translated from Xenos microcode to WGSL and checked with naga. Each draw then runs on the host GPU through wgpu, on the device Bevy created. An EDRAM surface is a host texture at sample resolution (2x2 samples per pixel for 4x multisampling, with depth and stencil surfaces alike), a resolve averages the samples of its rectangle into a host texture named by its destination address, and the swap packet VdSwap queues copies the presented front buffer into the window, ignoring alpha as the console's scan-out does. Textures are untiled and decoded on the CPU (8-, 16- and 32-bit formats, DXT1-5). The game's 2D screens appear this way: a black video frame (its Bink movies decode to black), then, as keys move it on, the safety warning, the ESRB rating, the autosave notice, the loading screen, the title screen (`Press START`), the save dialog and the tutorial's loading screen.

Not reproduced yet, each reported once in the log as `renderer: not reproduced: …`:

- depth resolves;
- render target formats other than 8-bit RGBA;
- writing resolved textures back to guest memory.

## Sub-features

- `rendering-shaders` translates every loaded shader to valid WGSL (see `boot-shader-translation` in [boot.md](./boot.md)).
- `rendering-present` presents guest frames: `renderer: first guest frame presented from physical 0x01220000`.
- `rendering-screens` shows the start-up screens in the window as the game reaches them.
- `rendering-gaps` reports each unreproduced feature once.

## How to get to it (user POV)

- Run the game and press Return (A) a few times once the window has been black for about 15 seconds.

## Driving it with run.sh

Preconditions:

- `scripts/doctor.sh` prints `ready`; `xdotool` and ImageMagick (`import`, `convert`) are installed. Set `PURE_DATA_DIR` to a fresh directory under `local/verification/`, since moving on through the screens reaches the save code (see [saves.md](./saves.md)).

- **Unit coverage.** `cargo test renderer translate` in `runtime/` checks the constants layout against the WGSL struct (with naga's layouter), the viewport and pixel-centre transform, primitive expansion and blend mapping, and that synthetic shaders translate to valid WGSL.
- **First frame.** Run 25 seconds: `runtime.log` has `renderer: rendering guest draws with wgpu` and `renderer: first guest frame presented from physical 0x01220000`, with no `ERROR bevy_render` line. `screenshot.png` is opaque black (`convert screenshot.png -format %c histogram:info:-` shows one near-black colour, not `#0D0D14`): the game's intro video frame is black (its Y plane is 16 throughout; `RUST_LOG=info,renderer=debug` logs each texture's mean byte).
- **Screens.** Start a 75-second run in the background and, once its `summary.txt` shows the display, wait 15 seconds, focus the window (see [input.md](./input.md)) and, 3 seconds apart, take a screenshot of that display with `DISPLAY=:1NN import -window root shot-<n>.png` before pressing Return ×4, Down, Return ×2, Escape, Return ×3. The screenshots after the first two presses show the safety warning, then the ESRB rating, then the autosave notice: hundreds of colours each (`convert shot-<n>.png -format '%k colors' info:`). Look at them locally; they are the game's own images, so never quote or commit them.
- **Title screen.** Drive a 180-second run the same way, pressing Return ×4, Down, Return ×2, Escape and then Return ×9. Around the twelfth screenshot the loading screen (tens of thousands of colours) and then the title screen appear; `cargo test texture` covers the tiling and DXT decoding.
- **Tutorial loading screen.** Drive a 330-second run pressing Return ×14, Escape (Start, at the title screen), then Return about every 3 seconds. From about the 37th screenshot the tutorial's loading screen shows (over 100,000 colours). `cargo test edram` covers the sample layout, window offsets and resolve rectangles.
- **Gaps.** `grep 'not reproduced' runtime.log` lists each missing feature once; after the long runs it shows only `depth resolves`.

## Gotchas

- Under Xvfb the GPU is llvmpipe, a CPU rasterizer. The translated shaders are heavy for it, so once the game draws, it presents only a few frames per second there (each swap waits for the previous frame's GPU work). Frame-rate claims belong to the boot recipe's pre-rendering phase or to real GPUs.
- A frame that comes out as the window's clear colour `#0D0D14` means the display sprite is transparent or missing, not that the game drew that colour.
- Screenshots and `shader=trace` output are derived from the game. Keep them in `local/verification/`.
