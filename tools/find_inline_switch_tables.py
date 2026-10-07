#!/usr/bin/env python3
"""Finds inline jump tables XenonAnalyse misses because their dispatch has no bounds check.

A function can bounds-check a switch index once and then dispatch on it several times, each time
through its own table of absolute case addresses stored right after the bctr:

    lis r12,hi ; addi r12,r12,lo ; rlwinm r0,rN,2,0,29 ; lwzx r0,r12,r0 ; mtctr r0 ; bctr

XenonAnalyse only recognizes the dispatch that follows the bounds check, so XenonRecomp turns the
others into indirect calls to addresses that are not functions. This prints a [[switch]] entry for
each missed table, sized by the nearest earlier `cmplwi rN,K` in the same function, in the format
of recomp/pure_switch_tables.toml. Tables it cannot size, or whose entries leave the function, are
reported on stderr and skipped.

Usage: find_inline_switch_tables.py <analysis-image.bin> <XenonAnalyse switch_tables.toml>
The image comes from build/inspect_xex; see docs/tooling.md.
"""

import sys

from find_switch_functions import (
    BCTR,
    INSTRUCTION_SIZE,
    Image,
    parse_switches,
    signed,
)

# The dispatch sequence, as instruction words. The register fields that vary are masked out.
LIS_R12 = 0x3D800000
ADDI_R12_R12 = 0x398C0000
IMMEDIATE_MASK = 0xFFFF
IMMEDIATE_SIGN_BIT = 0x8000
HIGH_HALF_SHIFT = 16
# rlwinm r0,rN,2,0,29 with rN cleared: scales the index to a word offset.
RLWINM_R0_INDEX_TIMES_4 = 0x5400103A
SOURCE_REGISTER_SHIFT = 21
REGISTER_MASK = 0x1F
LWZX_R0_R12_R0 = 0x7C0C002E
MTCTR_R0 = 0x7C0903A6
DISPATCH_LENGTH = 6
ADDRESS_MASK = 0xFFFFFFFF

# cmplwi crF,rA,K: opcode 10 with the 32-bit (L = 0) form.
OPCODE_SHIFT = 26
OPCODE_CMPLI = 10
CMPLI_LONG_BIT = 1 << 21
CMPLI_REGISTER_SHIFT = 16


def dispatch_at(image: Image, address: int) -> tuple[int, int] | None:
    """Returns (index register, table address) if the dispatch sequence starts at address."""
    lis, addi, rlwinm, lwzx, mtctr, bctr = (
        image.word(address + offset * INSTRUCTION_SIZE) for offset in range(DISPATCH_LENGTH)
    )
    if (lis & ~IMMEDIATE_MASK) != LIS_R12 or (addi & ~IMMEDIATE_MASK) != ADDI_R12_R12:
        return None
    index_register = (rlwinm >> SOURCE_REGISTER_SHIFT) & REGISTER_MASK
    if rlwinm & ~(REGISTER_MASK << SOURCE_REGISTER_SHIFT) != RLWINM_R0_INDEX_TIMES_4:
        return None
    if (lwzx, mtctr, bctr) != (LWZX_R0_R12_R0, MTCTR_R0, BCTR):
        return None
    table = ((lis & IMMEDIATE_MASK) << HIGH_HALF_SHIFT) + signed(addi & IMMEDIATE_MASK, IMMEDIATE_SIGN_BIT)
    return index_register, table & ADDRESS_MASK


def bound_before(image: Image, function_start: int, address: int, register: int) -> int | None:
    """The K of the last `cmplwi rN,K` before address in the function, if any."""
    bound = None
    for candidate in range(function_start, address, INSTRUCTION_SIZE):
        instruction = image.word(candidate)
        is_cmplwi = instruction >> OPCODE_SHIFT == OPCODE_CMPLI and not instruction & CMPLI_LONG_BIT
        if is_cmplwi and (instruction >> CMPLI_REGISTER_SHIFT) & REGISTER_MASK == register:
            bound = instruction & IMMEDIATE_MASK
    return bound


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    with open(sys.argv[1], "rb") as image_file:
        image = Image(image_file.read())
    with open(sys.argv[2]) as switch_file:
        known_bctrs = {image.next_bctr(switch.base) for switch in parse_switches(switch_file.read())}

    for begin, length in image.pdata_functions():
        end = begin + length
        for address in range(begin, end - DISPATCH_LENGTH * INSTRUCTION_SIZE, INSTRUCTION_SIZE):
            dispatch = dispatch_at(image, address)
            if dispatch is None:
                continue
            register, table = dispatch
            bctr = address + (DISPATCH_LENGTH - 1) * INSTRUCTION_SIZE
            if bctr in known_bctrs or table != bctr + INSTRUCTION_SIZE:
                continue
            bound = bound_before(image, begin, address, register)
            if bound is None:
                print(f"skipped {bctr:#x}: no bounds check on r{register} before it", file=sys.stderr)
                continue
            labels = [image.word(table + index * INSTRUCTION_SIZE) for index in range(bound + 1)]
            if not all(begin <= label < end for label in labels):
                print(f"skipped {bctr:#x}: a table entry leaves the function", file=sys.stderr)
                continue

            print(f"\n# Inline table after the bctr at 0x{bctr:X}, indexed by r{register} "
                  f"(bounded by an earlier cmplwi r{register},{bound}).")
            print("[[switch]]")
            print(f"base = 0x{bctr:X}")
            print(f"r = {register}")
            print(f"default = 0x{labels[0]:X}")
            print("labels = [")
            for label in labels:
                print(f"    0x{label:X},")
            print("]")


if __name__ == "__main__":
    main()
