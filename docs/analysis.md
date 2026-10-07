# Analysis log

## Target

| Field | Value |
| --- | --- |
| Game | Pure (2008) |
| Platform | Xbox 360 |
| Release / region | Archive labeled World (En,Fr,De,Es,It,Nl); region not independently verified |
| Title update state | Base disc executable; no title update applied in this workspace |
| Executable path | `originals/disc/default.xex` |
| Executable SHA-256 | `83c52e15c5c1abbddc5a8e695186200e82e4cbd63f6b679bfc811c62e264f33c` |
| Executable size | 29,315,072 bytes |
| Load address / entry point | `0x82000000` / `0x82254D60` |

## Tools and reproducibility

See [tooling.md](tooling.md) for pinned tool versions and exact setup commands. Local inspection output is in `local/entry-analysis.txt`; its associated mapped image is in `local/entry-analysis/analysis-image.bin`. That image has modified import thunks and is not the original executable.

## Confirmed findings

The archive at `/home/ubuntu/Pure (World) (En,Fr,De,Es,It,Nl).7z` contains a 7,835,492,352-byte disc image. 7-Zip completed extraction successfully, and extract-xiso extracted the game files while skipping `$SystemUpdate`.

The inspection tool loaded the executable and produced a section/import listing and an initial PowerPC disassembly. Its reported image base and entry point agree with the Ghidra XEX loader. Ghidra identifies retail encryption and basic compression (both format values `1`). These are tooling checks, not verified function reconstructions.

Ghidra's headless import completed and saved the project at `/home/ubuntu/pure-decomp-analysis/ghidra-projects/PureTooling.gpr` with automatic analysis disabled. The XEX loader itself populated function metadata and imports. No function bodies were decompiled or reconstructed.

## Full decompilation run (2026-10-05)

`tools/decompile.sh` ran against the executable above in 16 minutes on 8 cores. It imported into the Ghidra project `PureDecomp`, ran default auto-analysis, and exported pseudo-C to `local/decomp/` (172 MB, not committed).

| Measure | Count |
| --- | --- |
| Functions found by analysis | 43,611 |
| Exported without a hard decompiler failure | 43,611 |
| Containing decompiler warnings | 11,421 |
| Truncated by unsupported instructions (`halt_baddata`) | 372 |

The 372 truncated functions hit instructions that the `A2ALT` language could not decode, most likely Xenon VMX128 vector instructions. Check them in the disassembly before relying on them. Treat all output as unverified until a function is hand-reconstructed.

## Static recompilation config (2026-10-06)

Evidence for the addresses in `recomp/pure.toml` and `recomp/pure_switch_tables.toml`. All addresses refer to the executable above.

| Setting | Address | Evidence |
| --- | --- | --- |
| `savegprlr_14` / `restgprlr_14` | `0x82F67EF0` / `0x82F67F40` | Only aligned matches for XenonRecomp's byte patterns `f9 c1 ff 68` / `e9 c1 ff 68` |
| `savefpr_14` / `restfpr_14` | `0x82F696F0` / `0x82F6973C` | Only matches for `d9 cc ff 70` / `c9 cc ff 70` |
| `savevmx_14` / `restvmx_14` | `0x82F69B10` / `0x82F69DA8` | Only matches for the `li r11,-0x120` + `stvx`/`lvx v14` patterns |
| `savevmx_64` / `restvmx_64` | `0x82F69BA4` / `0x82F69E3C` | Only matches for the `li r11,-0x400` + `stvx128`/`lvx128 v64` patterns |
| `longjmp` | `0x82F748B0` | Calls `RtlUnwind` through the stub at `0x83070AA8`; restores v106–v127, LR, CR and r1 from the buffer in r3 and returns r4 |
| `setjmp` | `0x82F750D0` | The only `std r1,144(r3)`; saves into the same buffer offsets longjmp restores (304/308/144) and returns 0 |

The guest `jmp_buf` extends to at least offset 1344, larger than any host `jmp_buf`, so the native replacement fits in it.

**Leaf functions with jump tables.** 802 of the 937 jump tables XenonAnalyse found are stored inline, directly after the `bctr`. 191 jump tables, in 187 functions, sit in leaf functions with no `.pdata` entry. XenonRecomp ends such a function at the `bctr`, which splits its cases into separate functions and, for inline tables, analyses the table words as code. On this executable that made XenonRecomp exhaust memory and crash at `0x826C4158`. `tools/find_switch_functions.py` derives explicit boundaries for these functions by a linear sweep; its output is the `functions` list in `recomp/pure.toml`. With it, XenonRecomp reports no switch case outside its function.

**Switches with interleaved `nop`s.** XenonAnalyse misses the jump tables at `0x829FCEFC` and `0x82BB3570` because `nop`s sit between `mtctr` and `bctr`:

- `0x829FCEFC`: the index is `r10 = r9 & 63`, the byte table is at `0x820BEDE8`, and targets are `0x829FCF00` plus the byte. Only the first nine bytes (0, 12, …, 84, 100) are word-aligned case offsets; the following bytes are unrelated data, so the switch has nine cases.
- `0x82BB3570`: the index is `r28`, a 4-bit field, the halfword table is at `0x8210EB20`, and targets are `0x82BB3048` plus the halfword.

**Unchecked dispatches on a checked index.** Some functions bounds-check a switch index once and then dispatch on it several times, each through its own inline table of absolute case addresses (`lis r12`/`addi r12` to the word after the `bctr`, `rlwinm r0,rN,2,0,29`, `lwzx`, `mtctr`, `bctr`). XenonAnalyse only recognizes the dispatch that follows the bounds check. XenonRecomp turned the others into indirect calls, and the runtime stopped with "indirect call to 0x82DBB764, which has no recompiled function": `0x82DBB764` is a case label in `sub_82DBB4E0`, whose five later dispatches on `r27` were missed. `tools/find_inline_switch_tables.py` finds every such dispatch, sizes each table from the nearest earlier `cmplwi` on the same register, and checks that every case lands inside the function. It found 85 tables in 51 functions between `0x82D4D890` and `0x82DF0340`, all bounded by `cmplwi rN,10`; they are appended to `recomp/pure_switch_tables.toml`.

**Switch index width.** XenonRecomp emitted `switch (rN.u64)`, but the guest bounds-checks the index with `cmplwi` (the low word only) and indexes its table with `rlwinm r0,rN,2,0,29`, which also uses only the low word. At `0x82866B30` the index comes from `rlwinm r11,r11,0,28,26`, whose wrapping mask fills the upper word with a rotated copy of the lower one, so pressing A in a menu made the host switch jump through its table with a huge index and crash (SIGSEGV). The patch dispatches on `rN.u32`.

**Section sizes.** `.xidata`, `.xedata`, `.text` and `BINK` are marked executable and their sizes are not multiples of four. XenonAnalyse's debug build asserts on that, so the release build is used; it truncates the trailing partial word.

**Function count.** XenonRecomp emits 436,390 functions against Ghidra's 43,611. The difference is real: most are one- or two-instruction getters and thunks reached only through vtables, which Ghidra's call-following analysis never discovers.

**Remaining recompiler warnings.** 181 words that do not decode are emitted as comments. Upstream XenonRecomp did not implement 18 instructions this executable uses, at 143 sites: `vadduhs`, `vminub`, `vmaxub`, `vsel128`, `vpkswss128`, `stfsu`, `mulhdu`, `vsrh`, `vpkuhum128`, `vspltish`, `mulhd`, `frsqrte`, `vsrb`, `vslh`, `dcbst`, `vsro128`, `vcfpuxws128` and `mullhwu.`. `tools/patches/xenonrecomp.patch` implements them following upstream's conventions (vectors are stored with their elements reversed, so packs take their operands swapped); the recompiler now reports none unimplemented. See `local/recomp/recomp.log` for the undecodable words.

## Recovered functions

Add a row only after examining a function. Addresses refer to the executable identified above. Inferred names and types must be distinguished from names or types preserved in the binary.

| Address | Name | Evidence and confidence | Reconstruction | Verification |
| --- | --- | --- | --- | --- |

## Next investigation

The raw decompilation is complete. To start reconstruction, use `local/decomp/index.tsv` to pick a small function without warnings, verify the disassembly and select a small function with clear inputs and outputs. Do not infer an engine, middleware version, or asset format solely from the game's title or platform.
