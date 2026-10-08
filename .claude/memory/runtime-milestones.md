---
name: runtime-milestones
description: "Pure runtime milestone status and the order of what comes next, as of 2026-10-08 morning"
metadata:
  node_type: memory
  type: project
  originSessionId: 656d85ae-9ae0-487c-9201-58d7669d1f37
  modified: 2026-10-07T23:45:48.328Z
---

Milestones the user asked to keep advancing ("keep on working on the milestones", 2026-10-07). The repo has no milestone doc; these were defined during that session:

- Done 2026-10-07: boot; instruction coverage; keyboard input; saves; shader translation (Xenos to WGSL); rendering through wgpu with MSAA, depth/stencil, resolves (colour and depth), tiled/DXT/float textures, R32F targets; XMA stream stepping (silent samples); gamepads via gilrs (Linux needs `linux-gamepads` + libudev-dev; Homebrew's pkg-config misses /usr/lib/x86_64-linux-gnu/pkgconfig). Tutorial reaches gameplay since KeSetAffinityThread/ExCreateThread set the PCR processor number.
- Fixed 2026-10-07 late: command-buffer corruption = KTHREAD+0x14C thread ID was never set, so every thread passed D3D's device-owner check and loader threads wrote coherency packets into the command buffer concurrently (guest.rs now stores the thread object address there). Tutorial-gameplay hang (~500-650 frames in) = races in the game's own job system that host preemption exposes: lost activation (completion vs activate, sub_8252A978 / sub_82528108) and lost wakeup in the job-thread step (sub_8252B1C8); fixed with weak-symbol overrides in runtime/cpp/patches.cpp.
- Proven 2026-10-08 with the verify skill: 7,200 s run, 1,084 gameplay frames, no stall (local/verification/*-hangproof3).
- Rendering 2026-10-08: tutorial world, HUD and colours now right (fixes: RRRR/RGGG swizzle replication, sampling top-left parts of resolve destinations for the luminance chain, lower-tile resolves at row offsets, RB_COPY_DEST_INFO red/blue swap plus unswap at present, float-depth clears). Speed on llvmpipe ~2.6 s/frame (batching, indexed draws, batch buffer reuse, 2x MSAA at 1 sample/pixel on CPU adapters). Pure's camera motion blur scales with time between frames, so at this rate the track smears; playing the tutorial lap was stopped by the user's choice ("Stop here", 2026-10-08): leave playing for a machine with a GPU unless asked.
- Dark centre blob in every gameplay frame is unexplained (screen-space; maybe a lens/occlusion effect rendered wrong).
- Later: culling, real XMA (FFmpeg licensing is the user's call), Bink black video.

**Why:** the user wants steady milestone progress without being asked each step.
**How to apply:** prove each step with the verify-pure-runtime skill (`VERIFY_RUN_SECONDS` runs; `PURE_DATA_DIR` under local/verification; features/rendering.md for screenshots; run.sh in background + xdotool keydown/keyup on the run's own display), keep changes uncommitted unless the user asks, never quote or commit game-derived screenshots, WGSL or disassembly, and log to ~/.agents/audit.md. See [[reconstruction-language-cpp]].
