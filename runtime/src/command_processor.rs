//! A stand-in for the GPU's command processor. The title writes PM4 command packets into a ring
//! buffer and then stores the new write pointer to the memory-mapped CP_RB_WPTR register; with
//! flat guest memory that store is an ordinary write, so the runtime polls it.
//!
//! Nothing is drawn. The processor walks every packet the title submits (following indirect
//! buffers), applies register writes, performs the memory writes the title uses to track the
//! GPU's progress, and raises the interrupts it asks for. A WAIT_REG_MEM whose condition does not
//! hold stalls execution at that packet until a later poll finds it true, as the real GPU does;
//! titles pace their frames that way. Packets it does not execute are reported once each and
//! skipped by length.

use std::collections::BTreeSet;

use crate::gpu::{Draw, IndexBuffer, IndexSource, Initiator, Shader, ShaderCache, ShaderStage};
use crate::memory::GuestMemory;

/// Guest address of the GPU register block. Register writes from packets land here too, so
/// guest reads of a register see the last value written to it.
const GPU_REGISTERS: u32 = 0x7FC8_0000;
const WORD_SIZE: u32 = 4;
/// Registers, by index.
const CP_RB_WPTR: u32 = 0x1C5;
const SCRATCH_UMSK: u32 = 0x1DC;
const SCRATCH_ADDR: u32 = 0x1DD;
const SCRATCH_REG0: u32 = 0x578;
const SCRATCH_REGISTER_COUNT: u32 = 8;
pub const WRITE_POINTER_ADDRESS: u32 = GPU_REGISTERS + CP_RB_WPTR * WORD_SIZE;

/// Registers whose reads return fixed values on the console (as Xenia reports them): EDRAM
/// timing, the display's vertical counter (720 lines), interrupt status (always "vertical
/// blank"), and the viewport size (1280x720). Stored once, since guest reads are memory reads.
const FIXED_REGISTERS: [(u32, u32); 4] = [
    (0x0F00, 0x0810_0748),
    (0x194C, 0x0000_02D0),
    (0x1951, 0x0000_0001),
    (0x1961, 0x0500_02D0),
];

/// Words the runtime pads command buffers with. Read as headers they would be type-0 register
/// writes that swallow the next word, so (as Xenia does) each is a one-word no-op.
const PADDING_WORDS: [u32; 2] = [0, 0x0BAD_F00D];

/// PM4 packet headers: the type in the top two bits, then type-specific fields.
const PACKET_TYPE_SHIFT: u32 = 30;
const TYPE0: u32 = 0;
const TYPE1: u32 = 1;
const TYPE2: u32 = 2;
const COUNT_SHIFT: u32 = 16;
const COUNT_MASK: u32 = 0x3FFF;
const TYPE0_BASE_INDEX_MASK: u32 = 0x7FFF;
/// Type 0: every value goes to the base register instead of consecutive ones.
const TYPE0_ONE_REGISTER: u32 = 1 << 15;
const TYPE1_REGISTER_MASK: u32 = 0x7FF;
const TYPE1_SECOND_REGISTER_SHIFT: u32 = 11;
const OPCODE_SHIFT: u32 = 8;
const OPCODE_MASK: u32 = 0x7F;

/// Type-3 opcodes.
const OPCODE_NOP: u32 = 0x10;
const OPCODE_REG_RMW: u32 = 0x21;
const OPCODE_COND_WRITE: u32 = 0x45;
const OPCODE_EVENT_WRITE: u32 = 0x46;
const OPCODE_INTERRUPT: u32 = 0x54;
const OPCODE_INDIRECT_BUFFER_PFD: u32 = 0x37;
const OPCODE_INVALIDATE_STATE: u32 = 0x3B;
const OPCODE_WAIT_REG_MEM: u32 = 0x3C;
const OPCODE_INDIRECT_BUFFER: u32 = 0x3F;
const OPCODE_ME_INIT: u32 = 0x48;
const OPCODE_EVENT_WRITE_SHD: u32 = 0x58;
const OPCODE_SET_BIN_MASK_LO: u32 = 0x60;
const OPCODE_SET_BIN_MASK_HI: u32 = 0x61;
const OPCODE_SET_BIN_SELECT_LO: u32 = 0x62;
const OPCODE_SET_BIN_SELECT_HI: u32 = 0x63;
const OPCODE_DRAW_INDX: u32 = 0x22;
const OPCODE_IM_LOAD: u32 = 0x27;
const OPCODE_IM_LOAD_IMMEDIATE: u32 = 0x2B;
const OPCODE_SET_CONSTANT: u32 = 0x2D;
const OPCODE_LOAD_ALU_CONSTANT: u32 = 0x2F;
const OPCODE_DRAW_INDX_2: u32 = 0x36;
const OPCODE_SET_CONSTANT2: u32 = 0x55;
const OPCODE_SET_SHADER_CONSTANTS: u32 = 0x56;
/// Not a hardware opcode: VdSwap queues this packet (as Xenia does) so the frame is presented
/// in GPU order, after the draws and resolves submitted before it.
const OPCODE_XE_SWAP: u32 = 0x64;
const SWAP_SIGNATURE: u32 = u32::from_be_bytes(*b"SWAP");

/// SET_CONSTANT and LOAD_ALU_CONSTANT: a register index in the low 11 bits and, above it, which
/// block of constants it indexes: ALU, fetch, bool, loop, or ordinary registers.
const CONSTANT_INDEX_MASK: u32 = 0x7FF;
const CONSTANT_TYPE_SHIFT: u32 = 16;
const CONSTANT_TYPE_MASK: u32 = 0xFF;
const CONSTANT_BASES: [u32; 5] = [0x4000, 0x4800, 0x4900, 0x4908, 0x2000];
/// SET_CONSTANT2 and SET_SHADER_CONSTANTS index registers directly with 16 bits.
const DIRECT_INDEX_MASK: u32 = 0xFFFF;
/// LOAD_ALU_CONSTANT's source is a physical address; its size counts words in 12 bits.
const LOAD_ADDRESS_MASK: u32 = 0x3FFF_FFFF;
const LOAD_SIZE_MASK: u32 = 0xFFF;
/// IM_LOAD and IM_LOAD_IMMEDIATE: the shader stage (vertex 0, pixel 1) in the low two bits
/// (of the address, for IM_LOAD), and the size in words in the low 16 bits of the next word.
const SHADER_STAGE_MASK: u32 = 0x3;
const SHADER_SIZE_MASK: u32 = 0xFFFF;
const VERTEX_SHADER: u32 = 0;
const PIXEL_SHADER: u32 = 1;
/// Indices are 16 or 32 bits.
const INDEX_16_BIT_SIZE: u32 = 2;
const INDEX_32_BIT_SIZE: u32 = 4;

/// Packets the processor deliberately does nothing for: they pad the stream, or configure the
/// real GPU's microcode, caches and pipeline events, none of which the stand-in has.
const IGNORED_OPCODES: [u32; 4] = [
    OPCODE_NOP,
    OPCODE_ME_INIT,
    OPCODE_EVENT_WRITE,
    OPCODE_INVALIDATE_STATE,
];

/// Predicated tiling: titles replay one command buffer once per screen tile ("bin"), and a
/// type-3 packet with this header bit set only executes when the selected bins and the bin mask
/// share a bit. Both start with every bin set.
const PREDICATED: u32 = 0x1;
const ALL_BINS: u64 = u64::MAX;
const HALF_SHIFT: u32 = 32;

/// GPU memory writes name the byte order in an address's low two bits. The GPU is
/// little-endian; swapping bytes within 32 bits gives the CPU's big-endian order.
const ENDIAN_MASK: u32 = 0x3;
const ENDIAN_NONE: u32 = 0;
const ENDIAN_8_IN_16: u32 = 1;
const ENDIAN_8_IN_32: u32 = 2;
const ENDIAN_16_IN_32: u32 = 3;
/// EVENT_WRITE_SHD writes the GPU's event counter instead of its value when this bit is set.
const EVENT_WRITE_COUNTER: u32 = 1 << 31;
/// WAIT_REG_MEM and COND_WRITE: the comparison in the low three bits, whether they poll memory
/// rather than a register, and (COND_WRITE) whether they write memory rather than a register.
const WAIT_FUNCTION_MASK: u32 = 0x7;
const WAIT_ON_MEMORY: u32 = 1 << 4;
const WRITE_TO_MEMORY: u32 = 1 << 8;
/// A stall this long (about a second at the kernel's poll rate) is worth reporting.
const STALL_REPORT_POLLS: u32 = 2000;
/// How many recently executed packets a stall report lists.
const RECENT_PACKETS: usize = 12;
/// PM4_INTERRUPT's body is a mask of the CPUs to interrupt.
const CPU_COUNT: u32 = 6;

pub struct CommandProcessor {
    /// Guest address and size in words of the primary ring, once the title configures it.
    ring: Option<(u32, u32)>,
    /// Ring pointers count words from the start of the ring.
    read_pointer: u32,
    /// Guest address the read pointer is mirrored to, once the title enables it.
    read_pointer_writeback: Option<u32>,
    /// Opcodes already reported as not executed, so each is reported once.
    reported_opcodes: BTreeSet<u32>,
    /// Counts vertical blanks, for EVENT_WRITE_SHD packets that write the counter.
    vertical_blanks: u32,
    bin_mask: u64,
    bin_select: u64,
    shaders: ShaderCache,
    vertex_shader: Option<Shader>,
    pixel_shader: Option<Shader>,
    /// What the current stall waits for, and how many polls in a row it has lasted.
    stall: String,
    stalled_polls: u32,
    /// The guest address and header of the latest packets, oldest first, for stall reports.
    recent_packets: std::collections::VecDeque<(u32, u32)>,
    /// Indirect buffers being executed, outermost first. Their calls came from packets the read
    /// pointer has already passed, so they finish before the ring continues.
    indirect_buffers: Vec<Buffer>,
}

/// An indirect buffer: where its words are, and how far execution has got.
#[derive(Clone, Copy, Debug)]
struct Buffer {
    address: u32,
    size_words: u32,
    position: u32,
}

/// What executing one packet leads to.
enum Step {
    Continue,
    /// The packet waits for a condition that does not hold yet; retry it on a later poll.
    Stall,
    /// Execute this indirect buffer before anything after the packet.
    Call(Buffer),
}

/// What the processor needs from the rest of the runtime.
pub trait Environment {
    fn memory(&self) -> &GuestMemory;
    /// The guest address the title uses for a physical address, if it allocated it.
    fn guest_address(&self, physical: u32) -> Option<u32>;
    /// Reports a packet the processor does not execute.
    fn report(&self, message: String);
    /// Records detail useful only when debugging the processor itself.
    fn trace(&self, message: String);
    /// Interrupts `cpu` with a command processor interrupt.
    fn raise_interrupt(&self, cpu: u32);
    /// Executes (or, until there is a renderer, records) a draw.
    fn draw(&self, draw: &Draw);
    /// Presents the front buffer at physical address `front_buffer`.
    fn swap(&self, front_buffer: u32, width: u32, height: u32);
    /// A shader the title had not loaded before.
    fn shader_loaded(&self, stage: ShaderStage, shader: &Shader);
}

/// The register a SET_CONSTANT or LOAD_ALU_CONSTANT offset names, if its block is known.
fn constant_register(offset_type: u32) -> Option<u32> {
    let block = (offset_type >> CONSTANT_TYPE_SHIFT) & CONSTANT_TYPE_MASK;
    CONSTANT_BASES
        .get(block as usize)
        .map(|base| base + (offset_type & CONSTANT_INDEX_MASK))
}

fn set_low_half(value: &mut u64, low: u32) {
    *value = (*value & !u64::from(u32::MAX)) | u64::from(low);
}

fn set_high_half(value: &mut u64, high: u32) {
    *value = (*value & u64::from(u32::MAX)) | (u64::from(high) << HALF_SHIFT);
}

/// Reads one packet's words, given how many words are available to it.
fn read_packet(read: impl Fn(u32) -> u32, available: u32) -> Vec<u32> {
    let length = packet_length(read(0)).min(available);
    (0..length).map(read).collect()
}

/// A packet's length in words, from its header.
fn packet_length(header: u32) -> u32 {
    let count = ((header >> COUNT_SHIFT) & COUNT_MASK) + 1;
    if PADDING_WORDS.contains(&header) {
        return 1;
    }
    match header >> PACKET_TYPE_SHIFT {
        TYPE1 => 3,
        TYPE2 => 1,
        _ => 1 + count,
    }
}

/// WAIT_REG_MEM's and COND_WRITE's comparisons, as encoded in their info word.
const WAIT_LESS: u32 = 1;
const WAIT_LESS_OR_EQUAL: u32 = 2;
const WAIT_EQUAL: u32 = 3;
const WAIT_NOT_EQUAL: u32 = 4;
const WAIT_GREATER_OR_EQUAL: u32 = 5;
const WAIT_GREATER: u32 = 6;
const WAIT_ALWAYS: u32 = 7;

fn wait_condition_holds(function: u32, value: u32, reference: u32) -> bool {
    match function {
        WAIT_LESS => value < reference,
        WAIT_LESS_OR_EQUAL => value <= reference,
        WAIT_EQUAL => value == reference,
        WAIT_NOT_EQUAL => value != reference,
        WAIT_GREATER_OR_EQUAL => value >= reference,
        WAIT_GREATER => value > reference,
        WAIT_ALWAYS => true,
        _ => false,
    }
}

/// Reads a word the way the GPU does, from a physical address whose low two bits pick the byte
/// order. Unallocated memory reads as 0.
fn read_gpu_memory(environment: &impl Environment, address: u32) -> u32 {
    environment
        .guest_address(address & !ENDIAN_MASK)
        .map_or(0, |guest| {
            // Every byte-order mapping is its own inverse.
            big_endian_equivalent(environment.memory().read_u32(guest), address & ENDIAN_MASK)
        })
}

/// Writes a word to memory the way the GPU does: to a physical address whose low two bits pick
/// the byte order.
fn write_gpu_memory(environment: &impl Environment, address: u32, value: u32) {
    let physical = address & !ENDIAN_MASK;
    let Some(guest) = environment.guest_address(physical) else {
        environment.report(format!(
            "GPU memory write to unallocated physical {physical:#010x}"
        ));
        return;
    };
    environment
        .memory()
        .write_u32(guest, big_endian_equivalent(value, address & ENDIAN_MASK));
}

/// The big-endian word whose bytes match what the GPU stores for `value` in `endian` mode. The
/// GPU stores little-endian after swapping, so 8-in-32 swapping yields big-endian directly.
fn big_endian_equivalent(value: u32, endian: u32) -> u32 {
    match endian {
        ENDIAN_NONE => value.swap_bytes(),
        ENDIAN_8_IN_16 => value.rotate_left(16),
        ENDIAN_8_IN_32 => value,
        ENDIAN_16_IN_32 => value.rotate_left(16).swap_bytes(),
        _ => unreachable!("endian modes are two bits"),
    }
}

impl CommandProcessor {
    pub fn new() -> Self {
        Self {
            ring: None,
            read_pointer: 0,
            read_pointer_writeback: None,
            reported_opcodes: BTreeSet::new(),
            vertical_blanks: 0,
            bin_mask: ALL_BINS,
            bin_select: ALL_BINS,
            shaders: ShaderCache::default(),
            vertex_shader: None,
            pixel_shader: None,
            indirect_buffers: Vec::new(),
            stall: String::new(),
            stalled_polls: 0,
            recent_packets: std::collections::VecDeque::with_capacity(RECENT_PACKETS),
        }
    }

    /// Starts (or restarts) the ring empty: both pointers at its start, as the console resets
    /// them, so commands left from an earlier ring are not replayed.
    pub fn set_ring(&mut self, memory: &GuestMemory, address: u32, size_words: u32) {
        self.ring = Some((address, size_words));
        self.read_pointer = 0;
        self.indirect_buffers.clear();
        memory.write_u32(WRITE_POINTER_ADDRESS, 0);
        if let Some(writeback) = self.read_pointer_writeback {
            memory.write_u32(writeback, 0);
        }
        for (register, value) in FIXED_REGISTERS {
            memory.write_u32(GPU_REGISTERS + register * WORD_SIZE, value);
        }
    }

    /// How many distinct shaders the title has loaded.
    pub fn shader_count(&self) -> usize {
        self.shaders.len()
    }

    /// Called at each vertical blank, so titles that read the counter see the GPU progress.
    pub fn mark_vertical_blank(&mut self) {
        self.vertical_blanks = self.vertical_blanks.wrapping_add(1);
    }

    pub fn enable_read_pointer_writeback(&mut self, memory: &GuestMemory, address: u32) {
        self.read_pointer_writeback = Some(address);
        memory.write_u32(address, self.read_pointer);
    }

    /// Executes what the title has submitted, until it runs out or a packet stalls. Returns
    /// whether any packet executed.
    pub fn poll(&mut self, environment: &impl Environment) -> bool {
        let memory = environment.memory();
        let Some((ring, ring_words)) = self.ring else {
            return false;
        };
        let mut progressed = false;
        loop {
            let (words, step) = if let Some(&buffer) = self.indirect_buffers.last() {
                let remaining = buffer.size_words - buffer.position;
                if remaining == 0 {
                    self.indirect_buffers.pop();
                    continue;
                }
                let read = |offset: u32| {
                    memory.read_u32(buffer.address + (buffer.position + offset) * WORD_SIZE)
                };
                let words = read_packet(read, remaining);
                self.remember(buffer.address + buffer.position * WORD_SIZE, words[0]);
                let step = self.execute_packet(environment, &words);
                if !matches!(step, Step::Stall) {
                    self.indirect_buffers.last_mut().unwrap().position += words.len() as u32;
                }
                (words, step)
            } else {
                let write_pointer = memory.read_u32(WRITE_POINTER_ADDRESS) % ring_words;
                let available = (write_pointer + ring_words - self.read_pointer) % ring_words;
                if available == 0 {
                    break;
                }
                let start = self.read_pointer;
                // Submitted commands may wrap around the end of the ring.
                let read = |offset: u32| {
                    memory.read_u32(ring + ((start + offset) % ring_words) * WORD_SIZE)
                };
                let words = read_packet(read, available);
                self.remember(ring + start * WORD_SIZE, words[0]);
                let step = self.execute_packet(environment, &words);
                if !matches!(step, Step::Stall) {
                    self.read_pointer = (start + words.len() as u32) % ring_words;
                    if let Some(address) = self.read_pointer_writeback {
                        memory.write_u32(address, self.read_pointer);
                    }
                }
                (words, step)
            };
            match step {
                Step::Stall => {
                    self.stalled_polls += 1;
                    if self.stalled_polls == STALL_REPORT_POLLS {
                        let recent: Vec<String> = self
                            .recent_packets
                            .iter()
                            .map(|(address, header)| format!("{address:#010x}: {header:#010x}"))
                            .collect();
                        environment.report(format!(
                            "stalled for {STALL_REPORT_POLLS} polls: {}; {} indirect buffers deep, latest packets {}",
                            self.stall,
                            self.indirect_buffers.len(),
                            recent.join(", ")
                        ));
                    }
                    break;
                }
                Step::Call(buffer) => self.indirect_buffers.push(buffer),
                Step::Continue => {}
            }
            progressed |= !words.is_empty();
            self.stalled_polls = 0;
        }
        progressed
    }

    fn remember(&mut self, address: u32, header: u32) {
        if self.recent_packets.len() == RECENT_PACKETS {
            self.recent_packets.pop_front();
        }
        self.recent_packets.push_back((address, header));
    }

    fn execute_packet(&mut self, environment: &impl Environment, words: &[u32]) -> Step {
        let header = words[0];
        let body = &words[1..];
        environment.trace(format!("packet {header:#010x}"));
        if PADDING_WORDS.contains(&header) {
            return Step::Continue;
        }
        match header >> PACKET_TYPE_SHIFT {
            TYPE0 => {
                let base = header & TYPE0_BASE_INDEX_MASK;
                for (offset, &value) in (0..).zip(body) {
                    let register = if header & TYPE0_ONE_REGISTER != 0 {
                        base
                    } else {
                        base + offset
                    };
                    self.write_register(environment, register, value);
                }
                Step::Continue
            }
            TYPE1 => {
                let first = header & TYPE1_REGISTER_MASK;
                let second = (header >> TYPE1_SECOND_REGISTER_SHIFT) & TYPE1_REGISTER_MASK;
                if let [a, b] = *body {
                    self.write_register(environment, first, a);
                    self.write_register(environment, second, b);
                }
                Step::Continue
            }
            TYPE2 => Step::Continue,
            _ => {
                if header & PREDICATED != 0 && self.bin_select & self.bin_mask == 0 {
                    return Step::Continue;
                }
                let opcode = (header >> OPCODE_SHIFT) & OPCODE_MASK;
                self.execute_type3(environment, opcode, body)
            }
        }
    }

    fn execute_type3(&mut self, environment: &impl Environment, opcode: u32, body: &[u32]) -> Step {
        match (opcode, body) {
            (OPCODE_INDIRECT_BUFFER | OPCODE_INDIRECT_BUFFER_PFD, &[physical, size_words, ..]) => {
                let Some(address) = environment.guest_address(physical) else {
                    environment.report(format!(
                        "indirect buffer at unallocated physical {physical:#010x}"
                    ));
                    return Step::Continue;
                };
                environment.trace(format!(
                    "indirect buffer {physical:#010x}, {size_words:#x} words"
                ));
                return Step::Call(Buffer {
                    address,
                    size_words,
                    position: 0,
                });
            }
            (OPCODE_REG_RMW, &[register, and_mask, or_mask, ..]) => {
                let memory = environment.memory();
                let current = memory.read_u32(GPU_REGISTERS + register * WORD_SIZE);
                self.write_register(environment, register, (current & and_mask) | or_mask);
            }
            (OPCODE_EVENT_WRITE_SHD, &[initiator, address, value, ..]) => {
                let data = if initiator & EVENT_WRITE_COUNTER != 0 {
                    self.vertical_blanks
                } else {
                    value
                };
                environment.trace(format!("event write {data:#x} to {address:#x}"));
                write_gpu_memory(environment, address, data);
            }
            (OPCODE_WAIT_REG_MEM, &[info, poll_address, reference, mask, ..]) => {
                if !self.condition_holds(environment, info, poll_address, reference, mask) {
                    let place = if info & WAIT_ON_MEMORY != 0 {
                        "memory"
                    } else {
                        "register"
                    };
                    let value = self.polled_value(environment, info, poll_address);
                    self.stall = format!(
                        "WAIT_REG_MEM on {place} {poll_address:#x}: value {value:#x}, test {} against {reference:#x} under mask {mask:#x}",
                        info & WAIT_FUNCTION_MASK
                    );
                    return Step::Stall;
                }
            }
            (OPCODE_COND_WRITE, &[info, poll_address, reference, mask, target, data, ..]) => {
                if self.condition_holds(environment, info, poll_address, reference, mask) {
                    if info & WRITE_TO_MEMORY != 0 {
                        write_gpu_memory(environment, target, data);
                    } else {
                        self.write_register(environment, target, data);
                    }
                }
            }
            (OPCODE_SET_CONSTANT, &[offset_type, ref values @ ..]) => {
                if let Some(first) = constant_register(offset_type) {
                    self.write_registers(environment, first, values);
                }
            }
            (OPCODE_SET_CONSTANT2 | OPCODE_SET_SHADER_CONSTANTS, &[index, ref values @ ..]) => {
                self.write_registers(environment, index & DIRECT_INDEX_MASK, values);
            }
            (OPCODE_LOAD_ALU_CONSTANT, &[address, offset_type, size, ..]) => {
                let (Some(first), Some(source)) = (
                    constant_register(offset_type),
                    environment.guest_address(address & LOAD_ADDRESS_MASK),
                ) else {
                    return Step::Continue;
                };
                let memory = environment.memory();
                let values: Vec<u32> = (0..size & LOAD_SIZE_MASK)
                    .map(|index| memory.read_u32(source + index * WORD_SIZE))
                    .collect();
                self.write_registers(environment, first, &values);
            }
            (OPCODE_IM_LOAD, &[address_stage, start_size, ..]) => {
                let Some(source) = environment.guest_address(address_stage & !SHADER_STAGE_MASK)
                else {
                    environment.report(format!(
                        "shader at unallocated physical {address_stage:#010x}"
                    ));
                    return Step::Continue;
                };
                let memory = environment.memory();
                let words: Vec<u32> = (0..start_size & SHADER_SIZE_MASK)
                    .map(|index| memory.read_u32(source + index * WORD_SIZE))
                    .collect();
                self.load_shader(environment, address_stage & SHADER_STAGE_MASK, &words);
            }
            (OPCODE_IM_LOAD_IMMEDIATE, &[stage, start_size, ref words @ ..]) => {
                let size = (start_size & SHADER_SIZE_MASK) as usize;
                self.load_shader(
                    environment,
                    stage & SHADER_STAGE_MASK,
                    &words[..size.min(words.len())],
                );
            }
            // DRAW_INDX starts with a visibility query token, which the stand-in ignores.
            (OPCODE_DRAW_INDX, &[_, ref draw @ ..]) => self.issue_draw(environment, draw),
            (OPCODE_DRAW_INDX_2, draw) => self.issue_draw(environment, draw),
            (OPCODE_SET_BIN_MASK_LO, &[low, ..]) => set_low_half(&mut self.bin_mask, low),
            (OPCODE_SET_BIN_MASK_HI, &[high, ..]) => set_high_half(&mut self.bin_mask, high),
            (OPCODE_SET_BIN_SELECT_LO, &[low, ..]) => set_low_half(&mut self.bin_select, low),
            (OPCODE_SET_BIN_SELECT_HI, &[high, ..]) => set_high_half(&mut self.bin_select, high),
            (OPCODE_XE_SWAP, &[SWAP_SIGNATURE, front_buffer, width, height, ..]) => {
                environment.swap(front_buffer, width, height);
            }
            (OPCODE_INTERRUPT, &[cpu_mask, ..]) => {
                for cpu in (0..CPU_COUNT).filter(|cpu| cpu_mask & (1 << cpu) != 0) {
                    environment.raise_interrupt(cpu);
                }
            }
            _ if IGNORED_OPCODES.contains(&opcode) => {}
            _ => {
                if self.reported_opcodes.insert(opcode) {
                    environment.report(format!(
                        "PM4 opcode {opcode:#04x} not executed yet (body {body:08x?})"
                    ));
                }
            }
        }
        Step::Continue
    }

    fn write_registers(&mut self, environment: &impl Environment, first: u32, values: &[u32]) {
        for (offset, &value) in (0..).zip(values) {
            self.write_register(environment, first + offset, value);
        }
    }

    fn load_shader(&mut self, environment: &impl Environment, stage: u32, words: &[u32]) {
        let stage = match stage {
            VERTEX_SHADER => ShaderStage::Vertex,
            PIXEL_SHADER => ShaderStage::Pixel,
            _ => return,
        };
        let (shader, is_new) = self.shaders.intern(stage, words);
        if is_new {
            environment.shader_loaded(stage, &shader);
        }
        match stage {
            ShaderStage::Vertex => self.vertex_shader = Some(shader),
            ShaderStage::Pixel => self.pixel_shader = Some(shader),
        }
    }

    fn issue_draw(&self, environment: &impl Environment, body: &[u32]) {
        if let Some(draw) = self.decode_draw(environment, body) {
            environment.draw(&draw);
        }
    }

    /// A DRAW_INDX or DRAW_INDX_2 body after any visibility query: the initiator, then the index
    /// buffer's base and size for indexed draws.
    fn decode_draw(&self, environment: &impl Environment, body: &[u32]) -> Option<Draw> {
        let (&initiator, rest) = body.split_first()?;
        let initiator = Initiator::decode(initiator);
        let indices = match (initiator.source, rest) {
            (IndexSource::Dma, &[base, size, ..]) => {
                let index_size = if initiator.indices_are_32_bit {
                    INDEX_32_BIT_SIZE
                } else {
                    INDEX_16_BIT_SIZE
                };
                let address = environment.guest_address(base & !(index_size - 1))?;
                Some(IndexBuffer::new(address, size))
            }
            (IndexSource::Dma, _) | (IndexSource::Immediate, _) => {
                environment.report(format!("draw with unsupported index source {initiator:?}"));
                return None;
            }
            (IndexSource::Auto, _) => None,
        };
        Some(Draw {
            initiator,
            indices,
            vertex_shader: self.vertex_shader.clone(),
            pixel_shader: self.pixel_shader.clone(),
        })
    }

    /// WAIT_REG_MEM's and COND_WRITE's test of a register or memory word.
    fn condition_holds(
        &self,
        environment: &impl Environment,
        info: u32,
        poll_address: u32,
        reference: u32,
        mask: u32,
    ) -> bool {
        let value = self.polled_value(environment, info, poll_address);
        wait_condition_holds(info & WAIT_FUNCTION_MASK, value & mask, reference)
    }

    fn polled_value(&self, environment: &impl Environment, info: u32, poll_address: u32) -> u32 {
        if info & WAIT_ON_MEMORY != 0 {
            read_gpu_memory(environment, poll_address)
        } else {
            environment
                .memory()
                .read_u32(GPU_REGISTERS + poll_address * WORD_SIZE)
        }
    }

    fn write_register(&mut self, environment: &impl Environment, register: u32, value: u32) {
        environment.trace(format!("register {register:#06x} = {value:#010x}"));
        let memory = environment.memory();
        memory.write_u32(GPU_REGISTERS + register * WORD_SIZE, value);
        // Scratch registers whose mask bit is set are also written to memory, which titles use
        // to learn how far the GPU has got.
        let scratch = register.wrapping_sub(SCRATCH_REG0);
        if scratch < SCRATCH_REGISTER_COUNT
            && memory.read_u32(GPU_REGISTERS + SCRATCH_UMSK * WORD_SIZE) & (1 << scratch) != 0
        {
            let physical =
                memory.read_u32(GPU_REGISTERS + SCRATCH_ADDR * WORD_SIZE) + scratch * WORD_SIZE;
            match environment.guest_address(physical) {
                Some(address) => memory.write_u32(address, value),
                None => environment.report(format!(
                    "scratch write-back to unallocated physical {physical:#010x}"
                )),
            }
        }
    }
}

/// The packet VdSwap writes into the title's command stream to present a frame.
pub fn swap_packet(front_buffer: u32, width: u32, height: u32) -> [u32; 5] {
    const TYPE3: u32 = 3;
    const BODY_WORDS: u32 = 4;
    [
        (TYPE3 << PACKET_TYPE_SHIFT)
            | ((BODY_WORDS - 1) << COUNT_SHIFT)
            | (OPCODE_XE_SWAP << OPCODE_SHIFT),
        SWAP_SIGNATURE,
        front_buffer,
        width,
        height,
    ]
}

/// The current value of a GPU register.
pub fn read_register(memory: &GuestMemory, register: u32) -> u32 {
    memory.read_u32(GPU_REGISTERS + register * WORD_SIZE)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const RING: u32 = 0x1_0000;
    const RING_WORDS: u32 = 0x100;
    const WRITEBACK: u32 = 0x2000;
    /// Physical addresses map to guest addresses at this offset in the tests.
    const VIEW: u32 = 0xE000_0000;
    const INDIRECT_PHYSICAL: u32 = 0x3000;
    const FENCE_PHYSICAL: u32 = 0x4000;
    const FENCE_VALUE: u32 = 7;
    const SCRATCH_SLOT: u32 = 1;

    struct TestEnvironment {
        memory: GuestMemory,
        reports: RefCell<Vec<String>>,
        interrupts: RefCell<Vec<u32>>,
        draws: RefCell<Vec<Draw>>,
        swaps: RefCell<Vec<(u32, u32, u32)>>,
    }

    impl Environment for TestEnvironment {
        fn memory(&self) -> &GuestMemory {
            &self.memory
        }
        fn guest_address(&self, physical: u32) -> Option<u32> {
            Some(VIEW + physical)
        }
        fn report(&self, message: String) {
            self.reports.borrow_mut().push(message);
        }
        fn trace(&self, _: String) {}
        fn raise_interrupt(&self, cpu: u32) {
            self.interrupts.borrow_mut().push(cpu);
        }
        fn draw(&self, draw: &Draw) {
            self.draws.borrow_mut().push(draw.clone());
        }
        fn shader_loaded(&self, _: ShaderStage, _: &Shader) {}
        fn swap(&self, front_buffer: u32, width: u32, height: u32) {
            self.swaps.borrow_mut().push((front_buffer, width, height));
        }
    }

    fn type0(register: u32, count: u32) -> u32 {
        (TYPE0 << PACKET_TYPE_SHIFT) | ((count - 1) << COUNT_SHIFT) | register
    }

    fn type3(opcode: u32, count: u32) -> u32 {
        (3 << PACKET_TYPE_SHIFT) | ((count - 1) << COUNT_SHIFT) | (opcode << OPCODE_SHIFT)
    }

    /// Writes `words` into the ring and submits them.
    fn submit(environment: &TestEnvironment, processor: &mut CommandProcessor, words: &[u32]) {
        let memory = &environment.memory;
        let start = memory.read_u32(WRITE_POINTER_ADDRESS);
        for (offset, &word) in (0..).zip(words) {
            memory.write_u32(RING + ((start + offset) % RING_WORDS) * WORD_SIZE, word);
        }
        memory.write_u32(
            WRITE_POINTER_ADDRESS,
            (start + words.len() as u32) % RING_WORDS,
        );
        processor.poll(environment);
    }

    fn setup() -> (TestEnvironment, CommandProcessor) {
        let environment = TestEnvironment {
            memory: GuestMemory::reserve().unwrap(),
            reports: RefCell::new(Vec::new()),
            interrupts: RefCell::new(Vec::new()),
            draws: RefCell::new(Vec::new()),
            swaps: RefCell::new(Vec::new()),
        };
        let mut processor = CommandProcessor::new();
        processor.set_ring(&environment.memory, RING, RING_WORDS);
        processor.enable_read_pointer_writeback(&environment.memory, WRITEBACK);
        (environment, processor)
    }

    #[test]
    fn submitted_commands_are_consumed_and_the_read_pointer_written_back() {
        let (environment, mut processor) = setup();
        let padding = [type3(OPCODE_NOP, 1), 0];
        submit(&environment, &mut processor, &padding);
        assert_eq!(environment.memory.read_u32(WRITEBACK), padding.len() as u32);
        assert!(!processor.poll(&environment));
    }

    #[test]
    fn scratch_writes_in_an_indirect_buffer_reach_memory() {
        let (environment, mut processor) = setup();
        let memory = &environment.memory;
        let indirect = [
            type0(SCRATCH_UMSK, 2),
            1 << SCRATCH_SLOT,
            FENCE_PHYSICAL,
            type0(SCRATCH_REG0 + SCRATCH_SLOT, 1),
            FENCE_VALUE,
        ];
        for (offset, &word) in (0..).zip(&indirect) {
            memory.write_u32(VIEW + INDIRECT_PHYSICAL + offset * WORD_SIZE, word);
        }
        let call = [
            type3(OPCODE_INDIRECT_BUFFER, 2),
            INDIRECT_PHYSICAL,
            indirect.len() as u32,
        ];
        submit(&environment, &mut processor, &call);
        let fence = VIEW + FENCE_PHYSICAL + SCRATCH_SLOT * WORD_SIZE;
        assert_eq!(memory.read_u32(fence), FENCE_VALUE);
        assert!(environment.reports.borrow().is_empty());
    }

    #[test]
    fn commands_that_wrap_around_the_ring_are_read_in_order() {
        const REGISTER: u32 = 0x100;
        const VALUE: u32 = 0x1234;
        let (environment, mut processor) = setup();
        let filler: Vec<u32> =
            std::iter::repeat_n(TYPE2 << PACKET_TYPE_SHIFT, RING_WORDS as usize - 1).collect();
        submit(&environment, &mut processor, &filler);
        submit(&environment, &mut processor, &[type0(REGISTER, 1), VALUE]);
        assert_eq!(
            environment
                .memory
                .read_u32(GPU_REGISTERS + REGISTER * WORD_SIZE),
            VALUE
        );
    }

    #[test]
    fn gpu_memory_writes_honor_the_byte_order_in_the_address() {
        const VALUE: u32 = 0x1122_3344;
        let expected_bytes = [
            (ENDIAN_NONE, [0x44, 0x33, 0x22, 0x11]),
            (ENDIAN_8_IN_16, [0x33, 0x44, 0x11, 0x22]),
            (ENDIAN_8_IN_32, [0x11, 0x22, 0x33, 0x44]),
            (ENDIAN_16_IN_32, [0x22, 0x11, 0x44, 0x33]),
        ];
        let (environment, _) = setup();
        for (endian, bytes) in expected_bytes {
            write_gpu_memory(&environment, FENCE_PHYSICAL | endian, VALUE);
            let stored = environment.memory.read_u32(VIEW + FENCE_PHYSICAL);
            assert_eq!(stored.to_be_bytes(), bytes, "endian mode {endian}");
        }
    }

    #[test]
    fn padding_words_do_not_swallow_the_next_packet() {
        const REGISTER: u32 = 0x100;
        const VALUE: u32 = 0x1234;
        let (environment, mut processor) = setup();
        let mut words = PADDING_WORDS.to_vec();
        words.extend([type0(REGISTER, 1), VALUE]);
        submit(&environment, &mut processor, &words);
        assert_eq!(
            environment
                .memory
                .read_u32(GPU_REGISTERS + REGISTER * WORD_SIZE),
            VALUE
        );
    }

    #[test]
    fn restarting_the_ring_does_not_replay_old_commands() {
        let (environment, mut processor) = setup();
        submit(&environment, &mut processor, &[type3(OPCODE_NOP, 1), 0]);
        processor.set_ring(&environment.memory, RING, RING_WORDS);
        assert!(!processor.poll(&environment));
        assert_eq!(environment.memory.read_u32(WRITEBACK), 0);
    }

    #[test]
    fn a_failed_wait_inside_an_indirect_buffer_stalls_until_the_condition_holds() {
        const FLAG_PHYSICAL: u32 = 0x5000;
        const REGISTER: u32 = 0x100;
        const VALUE: u32 = 0x1234;
        let (environment, mut processor) = setup();
        let memory = &environment.memory;
        memory.write_u32(VIEW + FLAG_PHYSICAL, 1);
        // WAIT_REG_MEM's body: test, polled address, reference, mask, poll interval.
        let indirect = [
            type3(OPCODE_WAIT_REG_MEM, 5),
            WAIT_ON_MEMORY | WAIT_EQUAL,
            FLAG_PHYSICAL | ENDIAN_8_IN_32,
            0,
            u32::MAX,
            0,
            type0(REGISTER, 1),
            VALUE,
        ];
        for (offset, &word) in (0..).zip(&indirect) {
            memory.write_u32(VIEW + INDIRECT_PHYSICAL + offset * WORD_SIZE, word);
        }
        let call = [
            type3(OPCODE_INDIRECT_BUFFER, 2),
            INDIRECT_PHYSICAL,
            indirect.len() as u32,
        ];
        submit(&environment, &mut processor, &call);
        let register = GPU_REGISTERS + REGISTER * WORD_SIZE;
        assert_eq!(memory.read_u32(register), 0);
        assert!(!processor.poll(&environment));

        memory.write_u32(VIEW + FLAG_PHYSICAL, 0);
        assert!(processor.poll(&environment));
        assert_eq!(memory.read_u32(register), VALUE);
    }

    #[test]
    fn interrupts_go_to_each_cpu_in_the_mask() {
        const CPU_MASK: u32 = 0b10_0100;
        let (environment, mut processor) = setup();
        submit(
            &environment,
            &mut processor,
            &[type3(OPCODE_INTERRUPT, 1), CPU_MASK],
        );
        assert_eq!(*environment.interrupts.borrow(), [2, 5]);
    }

    #[test]
    fn predicated_packets_run_only_for_selected_bins() {
        const FENCE_PHYSICAL: u32 = 0x4000;
        const FIRST_VALUE: u32 = 5;
        const SKIPPED_VALUE: u32 = 3;
        let (environment, mut processor) = setup();
        let event_write = |value| {
            [
                type3(OPCODE_EVENT_WRITE_SHD, 3) | PREDICATED,
                0,
                FENCE_PHYSICAL | ENDIAN_8_IN_32,
                value,
            ]
        };
        submit(&environment, &mut processor, &event_write(FIRST_VALUE));
        // Select bin 1 while only bin 0 is in the mask: the predicated write is skipped.
        submit(
            &environment,
            &mut processor,
            &[
                type3(OPCODE_SET_BIN_MASK_LO, 1),
                0b01,
                type3(OPCODE_SET_BIN_MASK_HI, 1),
                0,
            ],
        );
        submit(
            &environment,
            &mut processor,
            &[
                type3(OPCODE_SET_BIN_SELECT_LO, 1),
                0b10,
                type3(OPCODE_SET_BIN_SELECT_HI, 1),
                0,
            ],
        );
        submit(&environment, &mut processor, &event_write(SKIPPED_VALUE));
        assert_eq!(
            environment.memory.read_u32(VIEW + FENCE_PHYSICAL),
            FIRST_VALUE
        );
    }

    #[test]
    fn constants_land_in_their_register_block() {
        const FETCH_BLOCK: u32 = 1;
        const INDEX: u32 = 6;
        const VALUES: [u32; 2] = [0x1111, 0x2222];
        let (environment, mut processor) = setup();
        let offset_type = (FETCH_BLOCK << CONSTANT_TYPE_SHIFT) | INDEX;
        submit(
            &environment,
            &mut processor,
            &[
                type3(OPCODE_SET_CONSTANT, 3),
                offset_type,
                VALUES[0],
                VALUES[1],
            ],
        );
        let first = CONSTANT_BASES[FETCH_BLOCK as usize] + INDEX;
        for (offset, value) in (0..).zip(VALUES) {
            assert_eq!(
                environment
                    .memory
                    .read_u32(GPU_REGISTERS + (first + offset) * WORD_SIZE),
                value
            );
        }
    }

    #[test]
    fn draws_carry_the_shaders_loaded_before_them() {
        const MICROCODE: [u32; 3] = [0xA, 0xB, 0xC];
        const AUTO_QUAD_LIST: u32 = 0x0004_008D;
        let (environment, mut processor) = setup();
        let mut words = vec![
            type3(OPCODE_IM_LOAD_IMMEDIATE, 2 + MICROCODE.len() as u32),
            PIXEL_SHADER,
            MICROCODE.len() as u32,
        ];
        words.extend(MICROCODE);
        words.extend([type3(OPCODE_DRAW_INDX_2, 1), AUTO_QUAD_LIST]);
        submit(&environment, &mut processor, &words);
        let draws = environment.draws.borrow();
        let [draw] = draws.as_slice() else {
            panic!("one draw expected")
        };
        assert_eq!(draw.initiator.index_count, 4);
        assert!(draw.vertex_shader.is_none());
        assert_eq!(&*draw.pixel_shader.as_ref().unwrap().words, &MICROCODE);
    }

    #[test]
    fn swap_packets_present_the_front_buffer() {
        const FRONT_BUFFER: u32 = 0x0100_0000;
        let (environment, mut processor) = setup();
        submit(
            &environment,
            &mut processor,
            &swap_packet(FRONT_BUFFER, 1280, 720),
        );
        assert_eq!(*environment.swaps.borrow(), [(FRONT_BUFFER, 1280, 720)]);
    }

    #[test]
    fn unexecuted_opcodes_are_reported_once() {
        const UNEXECUTED: u32 = 0x7E;
        let (environment, mut processor) = setup();
        submit(&environment, &mut processor, &[type3(UNEXECUTED, 1), 0]);
        submit(&environment, &mut processor, &[type3(UNEXECUTED, 1), 0]);
        assert_eq!(environment.reports.borrow().len(), 1);
    }
}
