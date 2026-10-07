//! Xenos shader microcode: decoding and disassembly.
//!
//! A shader starts with control-flow instructions, 48 bits each and packed two per three words,
//! up to the first instruction an `exec` refers to. `exec` instructions run a few ALU or fetch
//! instructions (96 bits each, addressed in three-word units from the start of the shader),
//! with two "sequence" bits per instruction saying which kind it is. Bit layouts follow Xenia's
//! `ucode.h`.

use std::fmt::Write;

const WORDS_PER_INSTRUCTION: u32 = 3;

/// Extracts `width` bits of `word` starting at `shift`.
const fn bits(word: u32, shift: u32, width: u32) -> u32 {
    (word >> shift) & ((1u64 << width) - 1) as u32
}

/// A control-flow instruction's two words: 32 bits, then 16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlFlow([u32; 2]);

/// When a conditional control-flow instruction takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Always,
    /// When bit `constant` of the bool constants equals `expected`.
    Bool {
        constant: u32,
        expected: bool,
    },
    /// When the predicate register equals `expected`.
    Predicate {
        expected: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlFlowKind {
    Nop,
    /// Runs `count` instructions from `address` when `condition` holds; `end` then finishes the
    /// shader, but only if the condition held.
    Exec {
        address: u32,
        count: u32,
        sequence: u32,
        end: bool,
        condition: Condition,
    },
    /// Starts loop `loop_id`; `address` is the control-flow index just past its LoopEnd.
    /// `repeat` keeps the current aL instead of resetting it to the loop's start.
    LoopStart {
        address: u32,
        loop_id: u32,
        repeat: bool,
    },
    /// Ends loop `loop_id`; `address` is the control-flow index of the loop body's start.
    /// `break_condition` leaves the loop early when the predicate equals it.
    LoopEnd {
        address: u32,
        loop_id: u32,
        break_condition: Option<bool>,
    },
    Call {
        address: u32,
        condition: Condition,
    },
    Return,
    Jump {
        address: u32,
        condition: Condition,
    },
    Alloc {
        size: u32,
        kind: u32,
    },
    MarkVertexFetchDone,
}

/// Control-flow opcodes, in the top four bits of the second word.
const CF_NOP: u32 = 0;
const CF_EXEC: u32 = 1;
const CF_EXEC_END: u32 = 2;
const CF_COND_EXEC: u32 = 3;
const CF_COND_EXEC_END: u32 = 4;
const CF_COND_EXEC_PRED: u32 = 5;
const CF_COND_EXEC_PRED_END: u32 = 6;
const CF_LOOP_START: u32 = 7;
const CF_LOOP_END: u32 = 8;
const CF_COND_CALL: u32 = 9;
const CF_RETURN: u32 = 10;
const CF_COND_JMP: u32 = 11;
const CF_ALLOC: u32 = 12;
const CF_COND_EXEC_PRED_CLEAN: u32 = 13;
const CF_COND_EXEC_PRED_CLEAN_END: u32 = 14;
const CF_MARK_VS_FETCH_DONE: u32 = 15;
const CF_OPCODE_SHIFT: u32 = 12;

impl ControlFlow {
    /// Two control-flow instructions from three words: the first takes word 0 and the low half
    /// of word 1, the second the high half of word 1 and all of word 2.
    pub fn unpack(words: [u32; 3]) -> [Self; 2] {
        [
            Self([words[0], words[1] & 0xFFFF]),
            Self([(words[1] >> 16) | (words[2] << 16), words[2] >> 16]),
        ]
    }

    pub fn kind(self) -> ControlFlowKind {
        let [low, high] = self.0;
        // Shared condition fields in the high word: a bool constant index and the expected value.
        let expected = bits(high, 10, 1) != 0;
        let on_bool = Condition::Bool {
            constant: bits(high, 2, 8),
            expected,
        };
        let on_predicate = Condition::Predicate { expected };
        let exec = |end, condition| ControlFlowKind::Exec {
            address: bits(low, 0, 12),
            count: bits(low, 12, 3),
            sequence: bits(low, 16, 12),
            end,
            condition,
        };
        // Jumps and calls are unconditional, predicated, or conditional on a bool constant.
        let branch_condition = if bits(low, 13, 1) != 0 {
            Condition::Always
        } else if bits(low, 14, 1) != 0 {
            on_predicate
        } else {
            on_bool
        };
        match bits(high, CF_OPCODE_SHIFT, 4) {
            CF_NOP => ControlFlowKind::Nop,
            CF_EXEC => exec(false, Condition::Always),
            CF_EXEC_END => exec(true, Condition::Always),
            // Despite the name, the "predicate clean" variants test a bool constant.
            CF_COND_EXEC | CF_COND_EXEC_PRED_CLEAN => exec(false, on_bool),
            CF_COND_EXEC_END | CF_COND_EXEC_PRED_CLEAN_END => exec(true, on_bool),
            CF_COND_EXEC_PRED => exec(false, on_predicate),
            CF_COND_EXEC_PRED_END => exec(true, on_predicate),
            CF_LOOP_START => ControlFlowKind::LoopStart {
                address: bits(low, 0, 13),
                loop_id: bits(low, 16, 5),
                repeat: bits(low, 13, 1) != 0,
            },
            CF_LOOP_END => ControlFlowKind::LoopEnd {
                address: bits(low, 0, 13),
                loop_id: bits(low, 16, 5),
                break_condition: (bits(low, 21, 1) != 0).then_some(expected),
            },
            CF_COND_CALL => ControlFlowKind::Call {
                address: bits(low, 0, 13),
                condition: branch_condition,
            },
            CF_RETURN => ControlFlowKind::Return,
            CF_COND_JMP => ControlFlowKind::Jump {
                address: bits(low, 0, 13),
                condition: branch_condition,
            },
            CF_ALLOC => ControlFlowKind::Alloc {
                size: bits(low, 0, 3),
                kind: bits(high, 9, 2),
            },
            CF_MARK_VS_FETCH_DONE => ControlFlowKind::MarkVertexFetchDone,
            _ => unreachable!("control-flow opcodes are four bits"),
        }
    }
}

/// An ALU or fetch instruction's three words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instruction {
    Alu(Alu),
    VertexFetch(VertexFetch),
    TextureFetch(TextureFetch),
    /// Another fetch opcode (LOD and gradient queries), kept as raw words.
    OtherFetch([u32; 3]),
}

/// Fetch opcodes, in the low five bits of the first word.
const FETCH_VERTEX: u32 = 0;
const FETCH_TEXTURE: u32 = 1;

impl Instruction {
    fn decode(words: [u32; 3], is_fetch: bool) -> Self {
        if !is_fetch {
            return Self::Alu(Alu(words));
        }
        match bits(words[0], 0, 5) {
            FETCH_VERTEX => Self::VertexFetch(VertexFetch(words)),
            FETCH_TEXTURE => Self::TextureFetch(TextureFetch(words)),
            _ => Self::OtherFetch(words),
        }
    }
}

/// An ALU instruction: a vector operation and a scalar operation issued together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alu(pub [u32; 3]);

impl Alu {
    pub fn vector_dest(self) -> u32 {
        bits(self.0[0], 0, 6)
    }
    pub fn scalar_dest(self) -> u32 {
        bits(self.0[0], 8, 6)
    }
    pub fn is_export(self) -> bool {
        bits(self.0[0], 15, 1) != 0
    }
    pub fn vector_write_mask(self) -> u32 {
        bits(self.0[0], 16, 4)
    }
    pub fn scalar_write_mask(self) -> u32 {
        bits(self.0[0], 20, 4)
    }
    pub fn scalar_opcode(self) -> u32 {
        bits(self.0[0], 26, 6)
    }
    pub fn vector_opcode(self) -> u32 {
        bits(self.0[2], 24, 5)
    }
    /// Destinations relative to the loop counter aL.
    pub fn vector_dest_relative(self) -> bool {
        bits(self.0[0], 6, 1) != 0
    }
    pub fn scalar_dest_relative(self) -> bool {
        bits(self.0[0], 14, 1) != 0
    }
    /// Whether constant sources are read as absolute values (temporaries carry their own bit).
    pub fn abs_constants(self) -> bool {
        bits(self.0[0], 7, 1) != 0
    }
    pub fn vector_clamp(self) -> bool {
        bits(self.0[0], 24, 1) != 0
    }
    pub fn scalar_clamp(self) -> bool {
        bits(self.0[0], 25, 1) != 0
    }
    /// Some(expected) when the instruction only runs while the predicate equals `expected`.
    pub fn predicate(self) -> Option<bool> {
        (bits(self.0[1], 28, 1) != 0).then_some(bits(self.0[1], 27, 1) != 0)
    }
    /// Whether relative constant sources are offset by a0 rather than aL.
    pub fn constant_relative_to_a0(self) -> bool {
        bits(self.0[1], 29, 1) != 0
    }
    fn is_temporary(self, n: u32) -> bool {
        bits(self.0[2], 29 + (3 - n), 1) != 0
    }
    /// Whether constant source `n` is relatively addressed. Two bits cover up to three
    /// constants: the first constant source uses bit 31, later ones bit 30.
    fn constant_is_relative(self, n: u32) -> bool {
        let first_constant = (1..n).all(|earlier| self.is_temporary(earlier));
        bits(self.0[1], if first_constant { 31 } else { 30 }, 1) != 0
    }
    /// Source `n` (1-3). A temporary's register field also carries its relative and
    /// absolute-value bits; a constant's is the whole index.
    pub fn source(self, n: u32) -> Source {
        const TEMPORARY_INDEX_MASK: u32 = 0x3F;
        const TEMPORARY_RELATIVE_BIT: u32 = 0x40;
        const TEMPORARY_ABSOLUTE_BIT: u32 = 0x80;
        let is_temporary = self.is_temporary(n);
        let register = bits(self.0[2], 8 * (3 - n), 8);
        Source {
            is_temporary,
            register: if is_temporary {
                register & TEMPORARY_INDEX_MASK
            } else {
                register
            },
            relative: if is_temporary {
                register & TEMPORARY_RELATIVE_BIT != 0
            } else {
                self.constant_is_relative(n)
            },
            absolute: if is_temporary {
                register & TEMPORARY_ABSOLUTE_BIT != 0
            } else {
                self.abs_constants()
            },
            swizzle: bits(self.0[1], 8 * (3 - n), 8),
            negate: bits(self.0[1], 24 + (3 - n), 1) != 0,
        }
    }
    /// The two operands of mulsc, addsc and subsc: a constant in source 3's register field, and
    /// a temporary whose index is packed into spare bits of the opcode, source 3's temporary
    /// flag and its swizzle. Both share source 3's swizzle and negation.
    pub fn scalar_constant_operands(self) -> (Source, Source) {
        const TEMPORARY_SWIZZLE_BITS: u32 = 0x3C;
        let source = self.source(3);
        let constant = Source {
            is_temporary: false,
            register: bits(self.0[2], 0, 8),
            relative: self.constant_is_relative(3),
            absolute: self.abs_constants(),
            ..source
        };
        let temporary = Source {
            is_temporary: true,
            register: (self.scalar_opcode() & 1)
                | (u32::from(self.is_temporary(3)) << 1)
                | (source.swizzle & TEMPORARY_SWIZZLE_BITS),
            relative: false,
            absolute: self.abs_constants(),
            ..source
        };
        (constant, temporary)
    }
}

/// One ALU source operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub is_temporary: bool,
    pub register: u32,
    /// Indexed by aL (temporaries) or by aL or a0 (constants).
    pub relative: bool,
    /// Read as an absolute value, before negation.
    pub absolute: bool,
    /// Two bits per component, relative to the component: 0 is the identity swizzle.
    pub swizzle: u32,
    pub negate: bool,
}

impl Source {
    /// The component (0-3) that result component `component` reads.
    pub fn component(self, component: u32) -> u32 {
        ((self.swizzle >> (2 * component)) + component) & 3
    }
}

/// A vertex fetch: loads one attribute of the current vertex from a vertex buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexFetch(pub [u32; 3]);

impl VertexFetch {
    pub fn source_register(self) -> u32 {
        bits(self.0[0], 5, 6)
    }
    pub fn dest_register(self) -> u32 {
        bits(self.0[0], 12, 6)
    }
    /// Which vertex fetch constant (buffer) it reads: three per fetch constant slot.
    pub fn constant(self) -> u32 {
        bits(self.0[0], 20, 5) * 3 + bits(self.0[0], 25, 2)
    }
    /// Three bits per component: 0-3 a component, 4 zero, 5 one, 7 leave unwritten.
    pub fn dest_swizzle(self) -> u32 {
        bits(self.0[1], 0, 12)
    }
    pub fn is_signed(self) -> bool {
        bits(self.0[1], 12, 1) != 0
    }
    pub fn is_normalized(self) -> bool {
        bits(self.0[1], 13, 1) == 0
    }
    /// The xenos::VertexFormat of the attribute.
    pub fn format(self) -> u32 {
        bits(self.0[1], 16, 6)
    }
    pub fn is_mini_fetch(self) -> bool {
        bits(self.0[1], 30, 1) != 0
    }
    /// Which component of the source register holds the vertex index.
    pub fn source_component(self) -> u32 {
        bits(self.0[0], 30, 2)
    }
    pub fn is_index_rounded(self) -> bool {
        bits(self.0[1], 15, 1) != 0
    }
    /// The fetched value is scaled by 2 to this power.
    pub fn exponent_adjust(self) -> i32 {
        ((self.0[1] << 2) as i32) >> 26
    }
    pub fn predicate(self) -> Option<bool> {
        (bits(self.0[1], 31, 1) != 0).then_some(bits(self.0[2], 31, 1) != 0)
    }
    /// Stride and offset, in words.
    pub fn stride(self) -> u32 {
        bits(self.0[2], 0, 8)
    }
    pub fn offset(self) -> i32 {
        // Sign-extend the 23-bit field.
        ((self.0[2] << 1) as i32) >> 9
    }
}

/// A texture fetch: samples a texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureFetch(pub [u32; 3]);

impl TextureFetch {
    pub fn source_register(self) -> u32 {
        bits(self.0[0], 5, 6)
    }
    pub fn dest_register(self) -> u32 {
        bits(self.0[0], 12, 6)
    }
    pub fn constant(self) -> u32 {
        bits(self.0[0], 20, 5)
    }
    /// Two bits per coordinate component, absolute.
    pub fn source_swizzle(self) -> u32 {
        bits(self.0[0], 26, 6)
    }
    pub fn dest_swizzle(self) -> u32 {
        bits(self.0[1], 0, 12)
    }
    /// 1D, 2D, 3D or cube.
    pub fn dimension(self) -> u32 {
        bits(self.0[2], 14, 2)
    }
    /// Coordinates are in texels rather than normalized.
    pub fn unnormalized_coordinates(self) -> bool {
        bits(self.0[0], 25, 1) != 0
    }
    pub fn predicate(self) -> Option<bool> {
        (bits(self.0[1], 31, 1) != 0).then_some(bits(self.0[2], 31, 1) != 0)
    }
}

/// A fetch destination swizzle as text: three bits per component, naming a fetched component,
/// a constant 0 or 1, or (as `_`) a component left unwritten.
fn fetch_swizzle(swizzle: u32) -> String {
    const KEEP: u32 = 7;
    (0..4)
        .map(|component| match (swizzle >> (3 * component)) & 0b111 {
            selector @ 0..=3 => b"xyzw"[selector as usize] as char,
            4 => '0',
            5 => '1',
            KEEP => '_',
            _ => '?',
        })
        .collect()
}

/// A texture fetch's coordinate swizzle: two absolute bits per coordinate, three coordinates.
fn coordinate_swizzle(swizzle: u32) -> String {
    const COORDINATES: u32 = 3;
    (0..COORDINATES)
        .map(|component| b"xyzw"[((swizzle >> (2 * component)) & 3) as usize] as char)
        .collect()
}

/// A decoded shader: its control flow and every instruction an `exec` runs, by address.
#[derive(Debug, Default)]
pub struct Program {
    pub control_flow: Vec<ControlFlowKind>,
    pub instructions: Vec<(u32, Instruction)>,
}

impl Program {
    pub fn decode(words: &[u32]) -> Self {
        let triple = |index: u32| -> Option<[u32; 3]> {
            let start = (index * WORDS_PER_INSTRUCTION) as usize;
            words
                .get(start..start + WORDS_PER_INSTRUCTION as usize)?
                .try_into()
                .ok()
        };
        let mut program = Self::default();
        // Control flow runs up to the lowest instruction address an exec uses.
        let mut end = words.len() as u32 / WORDS_PER_INSTRUCTION;
        let mut index = 0;
        while index < end {
            let Some(triple) = triple(index) else { break };
            for control_flow in ControlFlow::unpack(triple) {
                let kind = control_flow.kind();
                if let ControlFlowKind::Exec { address, count, .. } = kind
                    && count > 0
                {
                    end = end.min(address);
                }
                program.control_flow.push(kind);
            }
            index += 1;
        }
        for kind in &program.control_flow {
            let ControlFlowKind::Exec {
                address,
                count,
                sequence,
                ..
            } = *kind
            else {
                continue;
            };
            for slot in 0..count {
                let is_fetch = sequence >> (2 * slot) & 1 != 0;
                if let Some(words) = triple(address + slot) {
                    program
                        .instructions
                        .push((address + slot, Instruction::decode(words, is_fetch)));
                }
            }
        }
        program
    }

    /// A readable listing, for logs.
    pub fn disassemble(&self) -> String {
        let mut text = String::new();
        for kind in &self.control_flow {
            let _ = writeln!(text, "cf {kind:?}");
        }
        for (address, instruction) in &self.instructions {
            let _ = match instruction {
                Instruction::Alu(alu) => writeln!(
                    text,
                    "{address:3}: alu v{} -> {}{} mask {:04b} | s{} -> {}{} mask {:04b} | src {}",
                    alu.vector_opcode(),
                    if alu.is_export() { "export " } else { "r" },
                    alu.vector_dest(),
                    alu.vector_write_mask(),
                    alu.scalar_opcode(),
                    if alu.is_export() { "export " } else { "r" },
                    alu.scalar_dest(),
                    alu.scalar_write_mask(),
                    (1..=3)
                        .map(|n| {
                            let source = alu.source(n);
                            let kind = if source.is_temporary { "r" } else { "c" };
                            let sign = if source.negate { "-" } else { "" };
                            let swizzle: String = (0..4)
                                .map(|component| {
                                    b"xyzw"[source.component(component) as usize] as char
                                })
                                .collect();
                            format!("{sign}{kind}{}.{swizzle}", source.register)
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Instruction::VertexFetch(fetch) => writeln!(
                    text,
                    "{address:3}: vfetch r{}.{} <- r{} vf{} format {}{}{} stride {} offset {}{}",
                    fetch.dest_register(),
                    fetch_swizzle(fetch.dest_swizzle()),
                    fetch.source_register(),
                    fetch.constant(),
                    fetch.format(),
                    if fetch.is_signed() { " signed" } else { "" },
                    if fetch.is_normalized() {
                        " normalized"
                    } else {
                        ""
                    },
                    fetch.stride(),
                    fetch.offset(),
                    if fetch.is_mini_fetch() { " (mini)" } else { "" }
                ),
                Instruction::TextureFetch(fetch) => writeln!(
                    text,
                    "{address:3}: tfetch{}D r{}.{} <- r{}.{} tf{}",
                    fetch.dimension() + 1,
                    fetch.dest_register(),
                    fetch_swizzle(fetch.dest_swizzle()),
                    fetch.source_register(),
                    coordinate_swizzle(fetch.source_swizzle()),
                    fetch.constant()
                ),
                Instruction::OtherFetch(words) => writeln!(text, "{address:3}: fetch {words:08x?}"),
            };
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Packs two control-flow instructions (low word, high 16 bits) into three words.
    fn pack(first: [u32; 2], second: [u32; 2]) -> [u32; 3] {
        [
            first[0],
            first[1] | (second[0] << 16),
            (second[0] >> 16) | (second[1] << 16),
        ]
    }

    fn exec_end(address: u32, count: u32, sequence: u32) -> [u32; 2] {
        [
            address | (count << 12) | (sequence << 16),
            CF_EXEC_END << CF_OPCODE_SHIFT,
        ]
    }

    #[test]
    fn control_flow_unpacks_in_both_halves() {
        let first = exec_end(1, 2, 0b0100);
        let second = [0, CF_ALLOC << CF_OPCODE_SHIFT];
        let [a, b] = ControlFlow::unpack(pack(first, second));
        assert_eq!(
            a.kind(),
            ControlFlowKind::Exec {
                address: 1,
                count: 2,
                sequence: 0b0100,
                end: true,
                condition: Condition::Always,
            }
        );
        assert_eq!(b.kind(), ControlFlowKind::Alloc { size: 0, kind: 0 });
    }

    #[test]
    fn conditional_execs_carry_their_condition() {
        const BOOL_CONSTANT: u32 = 5;
        let high = (CF_COND_EXEC << CF_OPCODE_SHIFT) | (BOOL_CONSTANT << 2) | (1 << 10);
        let ControlFlowKind::Exec { condition, .. } = ControlFlow([0, high]).kind() else {
            panic!("expected an exec");
        };
        assert_eq!(
            condition,
            Condition::Bool {
                constant: BOOL_CONSTANT,
                expected: true
            }
        );
        let predicated = ControlFlow([0, CF_COND_EXEC_PRED_END << CF_OPCODE_SHIFT]).kind();
        assert!(matches!(
            predicated,
            ControlFlowKind::Exec {
                end: true,
                condition: Condition::Predicate { expected: false },
                ..
            }
        ));
    }

    #[test]
    fn exec_sequence_bits_choose_alu_or_fetch() {
        // exec_end at instruction 1 running two instructions: the first ALU, the second fetch.
        const VERTEX_FETCH_INTO_R3: u32 = 3 << 12;
        let mut words = pack(exec_end(1, 2, 0b0100), [0, CF_NOP << CF_OPCODE_SHIFT]).to_vec();
        words.extend([0, 0, 0]);
        words.extend([VERTEX_FETCH_INTO_R3, 0, 0]);
        let program = Program::decode(&words);
        assert!(matches!(program.instructions[0], (1, Instruction::Alu(_))));
        let (2, Instruction::VertexFetch(fetch)) = program.instructions[1] else {
            panic!("expected a vertex fetch at 2: {:?}", program.instructions);
        };
        assert_eq!(fetch.dest_register(), 3);
    }

    #[test]
    fn alu_swizzles_are_relative_to_the_component() {
        // Source 1's swizzle in bits 16-23 of word 1: 0 for every component reads .xyzw; a
        // relative step of 3 for x reads .w.
        let identity = Alu([0, 0, 0]).source(1);
        assert_eq!(
            (0..4).map(|c| identity.component(c)).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        let x_reads_w = Alu([0, 3 << 16, 0]).source(1);
        assert_eq!(x_reads_w.component(0), 3);
    }

    #[test]
    fn vertex_fetch_offsets_are_signed() {
        const MINUS_TWO: u32 = 0x7F_FFFE;
        let fetch = VertexFetch([0, 0, MINUS_TWO << 8]);
        assert_eq!(fetch.offset(), -2);
    }
}
