#!/usr/bin/env python3
"""Finds boundaries for leaf functions that contain jump tables but have no .pdata entry.

XenonRecomp ends a function at the first bctr when .pdata does not give its size, which splits
switch cases into separate functions (and, for tables stored inline after the bctr, analyses the
table words as code). This prints an explicit `functions = [...]` list for recomp/pure.toml.

Usage: find_switch_functions.py <analysis-image.bin> <XenonAnalyse switch_tables.toml>
The image comes from build/inspect_xex; see docs/tooling.md.
"""

import bisect
import re
import struct
import sys
from dataclasses import dataclass

IMAGE_BASE = 0x82000000
PDATA_ADDRESS = 0x8214A600
PDATA_SIZE = 0x476D0
PDATA_ENTRY_SIZE = 8
PDATA_LENGTH_SHIFT = 8
PDATA_LENGTH_MASK = 0x3FFFFF
INSTRUCTION_SIZE = 4

OPCODE_SHIFT = 26
OPCODE_BC = 16
OPCODE_B = 18
LINK_BIT = 1
BC_DISPLACEMENT_MASK = 0xFFFC
BC_SIGN_BIT = 0x8000
B_DISPLACEMENT_MASK = 0x3FFFFFC
B_SIGN_BIT = 0x2000000
BLR = 0x4E800020
BCTR = 0x4E800420
PADDING = 0x00000000

# A word after bctr is an inline table entry if it points a little way forward into the function.
INLINE_TABLE_MAX_DISTANCE = 0x10000
# The furthest an XenonAnalyse switch base can sit before its bctr.
SWITCH_BASE_MAX_DISTANCE = 0x100


@dataclass(frozen=True)
class Switch:
    base: int
    labels: list[int]


class Image:
    def __init__(self, data: bytes) -> None:
        self.data = data

    def word(self, address: int) -> int:
        offset = address - IMAGE_BASE
        return struct.unpack(">I", self.data[offset:offset + INSTRUCTION_SIZE])[0]

    def pdata_functions(self) -> list[tuple[int, int]]:
        functions = []
        for offset in range(PDATA_ADDRESS, PDATA_ADDRESS + PDATA_SIZE, PDATA_ENTRY_SIZE):
            begin, packed = self.word(offset), self.word(offset + INSTRUCTION_SIZE)
            if begin:
                length = ((packed >> PDATA_LENGTH_SHIFT) & PDATA_LENGTH_MASK) * INSTRUCTION_SIZE
                functions.append((begin, length))
        return sorted(functions)

    def next_bctr(self, address: int) -> int:
        while self.word(address) != BCTR:
            address += INSTRUCTION_SIZE
        return address

    def skip_padding(self, address: int) -> int:
        while self.word(address) == PADDING:
            address += INSTRUCTION_SIZE
        return address


def parse_switches(text: str) -> list[Switch]:
    pattern = r"base = (0x[0-9A-F]+)\nr = \d+\ndefault = (0x[0-9A-F]+)\nlabels = \[\n(.*?)\]"
    return [
        Switch(int(base, 16), [int(label, 16) for label in re.findall(r"0x[0-9A-F]+", labels)] + [int(default, 16)])
        for base, default, labels in re.findall(pattern, text, re.S)
    ]


def signed(value: int, sign_bit: int) -> int:
    return value - 2 * sign_bit if value & sign_bit else value


def sweep_function(image: Image, start: int, bctr_labels: dict[int, list[int]],
                   inline_table_sizes: dict[int, int]) -> int:
    """Returns the end of the function at start: the first terminator past every known branch target."""
    address, furthest_target = start, 0
    while True:
        if address in bctr_labels:
            furthest_target = max([furthest_target, *bctr_labels[address]])
            address += INSTRUCTION_SIZE + inline_table_sizes[address]
            continue

        instruction = image.word(address)
        opcode = instruction >> OPCODE_SHIFT
        is_link = instruction & LINK_BIT
        if opcode == OPCODE_BC and not is_link:
            furthest_target = max(furthest_target, address + signed(instruction & BC_DISPLACEMENT_MASK, BC_SIGN_BIT))
        is_branch = opcode == OPCODE_B and not is_link
        if is_branch:
            target = address + signed(instruction & B_DISPLACEMENT_MASK, B_SIGN_BIT)
            # Treat short forward branches as internal; anything else is a tail call.
            if address < target < address + INLINE_TABLE_MAX_DISTANCE:
                furthest_target = max(furthest_target, target)

        address += INSTRUCTION_SIZE
        if (is_branch or instruction in (BLR, BCTR)) and address > furthest_target:
            return address


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    with open(sys.argv[1], "rb") as image_file:
        image = Image(image_file.read())
    with open(sys.argv[2]) as switch_file:
        switches = parse_switches(switch_file.read())

    pdata = image.pdata_functions()
    pdata_starts = [begin for begin, _ in pdata]

    bctr_labels: dict[int, list[int]] = {}
    inline_table_sizes: dict[int, int] = {}
    for switch in switches:
        bctr = image.next_bctr(switch.base)
        assert bctr - switch.base <= SWITCH_BASE_MAX_DISTANCE, f"no bctr near switch {switch.base:#x}"
        bctr_labels[bctr] = switch.labels
        first_word = image.word(bctr + INSTRUCTION_SIZE)
        is_inline = bctr < first_word < bctr + INLINE_TABLE_MAX_DISTANCE
        # XenonAnalyse appends the default label; the table holds only the case labels.
        inline_table_sizes[bctr] = (len(switch.labels) - 1) * INSTRUCTION_SIZE if is_inline else 0

    boundaries = set()
    for bctr in bctr_labels:
        index = bisect.bisect_right(pdata_starts, bctr) - 1
        begin, length = pdata[index]
        if begin <= bctr < begin + length:
            continue

        # Several leaf functions can share one gap between .pdata entries; walk to the one holding the bctr.
        start = image.skip_padding(begin + length)
        while (end := sweep_function(image, start, bctr_labels, inline_table_sizes)) <= bctr:
            start = image.skip_padding(end)
        boundaries.add((start, end - start))

    print("functions = [")
    for start, size in sorted(boundaries):
        print(f"    {{ address = 0x{start:X}, size = 0x{size:X} }},")
    print("]")


if __name__ == "__main__":
    main()
