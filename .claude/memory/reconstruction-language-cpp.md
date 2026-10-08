---
name: reconstruction-language-cpp
description: Pure languages - hand decomp in C++; recomp = XenonRecomp C++ codegen + Rust/Bevy runtime; full Rust recompiler deferred
metadata:
  node_type: memory
  type: project
  originSessionId: 656d85ae-9ae0-487c-9201-58d7669d1f37
  modified: 2026-10-06T00:58:53.604Z
---

2026-10-05: Ian chose C++ (not Rust) for hand-reconstructed decomp source.
2026-10-06: Ian asked for a static recompile. Decision: keep XenonRecomp's generated C++ for codegen, write the runtime (kernel, GPU, audio, input, window) in Rust on Bevy. A full PPC→Rust recompiler is explicitly deferred ("let's not go full rust yet"), possibly done incrementally later.

**Why:** Reuses proven codegen; hand-written runtime is where Ian wants Rust. Keeps scope small.
**How to apply:** Generated C++ links into the Rust runtime via extern "C". Don't start a Rust codegen unless Ian asks. Generated code and game data stay in local/, hash-gated, never committed.
