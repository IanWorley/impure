//! The XMA audio decoder's per-stream state (XMA_CONTEXT_DATA), and a decoder that steps
//! through the stream exactly as the hardware does but writes silence.
//!
//! A context is 16 big-endian words of bitfields in physical memory; once a word is loaded, its
//! first field occupies the least significant bits. Input is 2 KiB packets, each a 32-bit
//! header and then frames; a frame starts with its length in bits (15 bits) and ends with a bit
//! saying whether another frame follows in the same packet, and the last frame may continue in
//! the stream's next packet. Each frame decodes to 512 samples per channel. The decoder follows
//! that structure, the loop settings and the buffer handshakes as Xenia's `xma_context.cc`
//! does, so titles see the hardware's progress. Each frame's samples come from a
//! `FrameDecoder` (FFmpeg, with the `audio` feature); without one, frames decode to silence.

use crate::memory::GuestMemory;

pub const CONTEXT_SIZE: u32 = 64;
const WORD_SIZE: u32 = 4;

/// A field: which word it is in, its lowest bit, and its width.
#[derive(Clone, Copy)]
struct Field {
    word: u32,
    shift: u32,
    width: u32,
}

const fn field(word: u32, shift: u32, width: u32) -> Field {
    Field { word, shift, width }
}

const INPUT_BUFFER_0_PACKET_COUNT: Field = field(0, 0, 12);
const LOOP_COUNT: Field = field(0, 12, 8);
const INPUT_BUFFER_0_VALID: Field = field(0, 20, 1);
const INPUT_BUFFER_1_VALID: Field = field(0, 21, 1);
const OUTPUT_BUFFER_BLOCK_COUNT: Field = field(0, 22, 5);
const OUTPUT_BUFFER_WRITE_OFFSET: Field = field(0, 27, 5);
const INPUT_BUFFER_1_PACKET_COUNT: Field = field(1, 0, 12);
const LOOP_SUBFRAME_END: Field = field(1, 14, 3);
const LOOP_SUBFRAME_SKIP: Field = field(1, 17, 3);
const SUBFRAME_DECODE_COUNT: Field = field(1, 20, 4);
const SAMPLE_RATE: Field = field(1, 27, 2);
const IS_STEREO: Field = field(1, 29, 1);
const OUTPUT_BUFFER_VALID: Field = field(1, 31, 1);
const INPUT_BUFFER_READ_OFFSET: Field = field(2, 0, 26);
const LOOP_START: Field = field(3, 0, 26);
const LOOP_END: Field = field(4, 0, 26);
const CURRENT_BUFFER: Field = field(4, 31, 1);
const INPUT_BUFFER_0_POINTER: Field = field(5, 0, 32);
const INPUT_BUFFER_1_POINTER: Field = field(6, 0, 32);
const OUTPUT_BUFFER_POINTER: Field = field(7, 0, 32);
const OUTPUT_BUFFER_READ_OFFSET: Field = field(9, 0, 5);

/// Input arrives in 2 KiB packets, each starting with a 32-bit header.
const BYTES_PER_PACKET: u32 = 2048;
const BITS_PER_PACKET: u32 = BYTES_PER_PACKET * 8;
const BITS_PER_PACKET_HEADER: u32 = 32;
/// A frame's length field, and the length that marks an invalid frame.
const FRAME_LENGTH_BITS: u32 = 15;
const INVALID_FRAME_LENGTH: u32 = 0x7FFF;
/// Output is 16-bit samples, 512 per channel per frame, in 256-byte blocks.
const BYTES_PER_FRAME_CHANNEL: u32 = 512 * 2;
const BYTES_PER_OUTPUT_BLOCK: u32 = 256;
/// The sample rates the 2-bit sample rate field selects.
const SAMPLE_RATES_HZ: [u32; 4] = [24_000, 32_000, 44_100, 48_000];
/// A loop count of 255 loops forever.
const INFINITE_LOOPS: u32 = 255;

/// Decodes frames (`bits` long, most significant bit first) to samples.
pub trait FrameDecoder {
    /// The stream's channel count, given its next frame; `hint` is what the context says.
    fn channels(&mut self, frame: &[u8], bits: u32, hint: usize) -> usize;
    /// Decodes the frame `channels` was given into `output`: 512 16-bit big-endian samples per
    /// channel, channels interleaved, as the hardware writes them. Returns false when it
    /// cannot, and the frame is then silent.
    fn decode(&mut self, frame: &[u8], bits: u32, output: &mut [u8]) -> bool;
}

/// What the title passes to XMAInitializeContext (XMA_CONTEXT_INIT), with the buffer addresses
/// already translated to physical addresses.
pub struct Initialization {
    pub input_buffers: [(u32, u32); 2],
    pub input_buffer_read_offset: u32,
    pub output_buffer: u32,
    pub output_buffer_block_count: u32,
    pub subframe_decode_count: u32,
    pub channel_count: u32,
    pub sample_rate: u32,
    pub loop_start: u32,
    pub loop_end: u32,
    pub loop_count: u32,
    pub loop_subframe_end: u32,
    pub loop_subframe_skip: u32,
}

/// One context's words, as loaded from guest memory.
pub struct XmaContext {
    address: u32,
    words: [u32; 16],
    /// The words as loaded, to tell which bits the decoder changed.
    loaded: [u32; 16],
}

impl XmaContext {
    pub fn load(memory: &GuestMemory, address: u32) -> Self {
        let mut words = [0; 16];
        for (index, word) in (0..).zip(&mut words) {
            *word = memory.read_u32(address + index * WORD_SIZE);
        }
        Self {
            address,
            words,
            loaded: words,
        }
    }

    /// Writes back only the bits the decoder changed, onto guest memory as it is now: the title
    /// writes contexts directly (FMOD programs each new sound that way), and a decoding pass now
    /// takes long enough that writing whole words back would undo what it wrote meanwhile.
    pub fn store(&self, memory: &GuestMemory) {
        for (index, (&word, &loaded)) in (0..).zip(self.words.iter().zip(&self.loaded)) {
            let changed = word ^ loaded;
            if changed == 0 {
                continue;
            }
            let address = self.address + index * WORD_SIZE;
            let current = memory.read_u32(address);
            memory.write_u32(address, (current & !changed) | (word & changed));
        }
    }

    fn get(&self, field: Field) -> u32 {
        let mask = mask(field.width);
        (self.words[field.word as usize] >> field.shift) & mask
    }

    fn set(&mut self, field: Field, value: u32) {
        let mask = mask(field.width) << field.shift;
        let word = &mut self.words[field.word as usize];
        *word = (*word & !mask) | ((value << field.shift) & mask);
    }

    pub fn initialize(&mut self, init: &Initialization) {
        self.words = [0; 16];
        let [(input_0, packets_0), (input_1, packets_1)] = init.input_buffers;
        self.set(INPUT_BUFFER_0_POINTER, input_0);
        self.set(INPUT_BUFFER_0_PACKET_COUNT, packets_0);
        self.set(INPUT_BUFFER_1_POINTER, input_1);
        self.set(INPUT_BUFFER_1_PACKET_COUNT, packets_1);
        self.set(INPUT_BUFFER_READ_OFFSET, init.input_buffer_read_offset);
        self.set(OUTPUT_BUFFER_POINTER, init.output_buffer);
        self.set(OUTPUT_BUFFER_BLOCK_COUNT, init.output_buffer_block_count);
        self.set(SUBFRAME_DECODE_COUNT, init.subframe_decode_count);
        self.set(SAMPLE_RATE, init.sample_rate);
        self.set(IS_STEREO, u32::from(init.channel_count > 1));
        self.set(LOOP_START, init.loop_start);
        self.set(LOOP_END, init.loop_end);
        self.set(LOOP_COUNT, init.loop_count);
        self.set(LOOP_SUBFRAME_END, init.loop_subframe_end);
        self.set(LOOP_SUBFRAME_SKIP, init.loop_subframe_skip);
    }

    pub fn input_buffer_valid(&self, buffer: usize) -> bool {
        self.get([INPUT_BUFFER_0_VALID, INPUT_BUFFER_1_VALID][buffer]) != 0
    }

    pub fn set_input_buffer_valid(&mut self, buffer: usize) {
        self.set([INPUT_BUFFER_0_VALID, INPUT_BUFFER_1_VALID][buffer], 1);
    }

    pub fn output_buffer_valid(&self) -> bool {
        self.get(OUTPUT_BUFFER_VALID) != 0
    }

    pub fn set_output_buffer_valid(&mut self) {
        self.set(OUTPUT_BUFFER_VALID, 1);
    }

    pub fn input_buffer_read_offset(&self) -> u32 {
        self.get(INPUT_BUFFER_READ_OFFSET)
    }

    pub fn set_input_buffer_read_offset(&mut self, offset: u32) {
        self.set(INPUT_BUFFER_READ_OFFSET, offset);
    }

    pub fn output_buffer_read_offset(&self) -> u32 {
        self.get(OUTPUT_BUFFER_READ_OFFSET)
    }

    pub fn set_output_buffer_read_offset(&mut self, offset: u32) {
        self.set(OUTPUT_BUFFER_READ_OFFSET, offset);
    }

    pub fn output_buffer_write_offset(&self) -> u32 {
        self.get(OUTPUT_BUFFER_WRITE_OFFSET)
    }

    /// The stream's channel count: 1 or 2.
    pub fn channel_count(&self) -> usize {
        1 + self.get(IS_STEREO) as usize
    }

    /// The stream's sample rate in hertz.
    pub fn sample_rate_hz(&self) -> u32 {
        SAMPLE_RATES_HZ[self.get(SAMPLE_RATE) as usize]
    }

    /// One decoding pass, as the hardware runs when the title enables the context: decodes
    /// frames until the output ring is full or input runs out. Clears the output buffer's valid
    /// bit when the ring ends up full or there was no input, which titles wait for.
    /// `guest_address` maps a physical address to the guest address the title uses. Returns how
    /// many frames it decoded.
    pub fn decode(
        &mut self,
        memory: &GuestMemory,
        guest_address: impl Fn(u32) -> Option<u32>,
        mut decoder: Option<&mut dyn FrameDecoder>,
    ) -> u32 {
        let mut frames = 0;
        if !self.output_buffer_valid() {
            return frames;
        }
        if !self.input_buffer_valid(0) && !self.input_buffer_valid(1) {
            self.set(OUTPUT_BUFFER_VALID, 0);
            return frames;
        }
        let block_count = self.get(OUTPUT_BUFFER_BLOCK_COUNT);
        let read = self.output_buffer_read_offset();
        let mut write = self.output_buffer_write_offset();
        // Equal offsets mean an empty ring; the pass may fill it completely.
        let mut free = if read == write {
            block_count
        } else {
            (read + block_count - write) % block_count
        };
        let Some(output) = guest_address(self.get(OUTPUT_BUFFER_POINTER)) else {
            return frames;
        };
        while block_count > 0 && free > 0 {
            if !self.input_buffer_valid(0) && !self.input_buffer_valid(1) {
                break;
            }
            let looped = self.loop_back(false);
            let Some(input) = self.current_input(memory, &guest_address) else {
                break;
            };
            // At a packet boundary, move to the first frame that starts at or after it.
            let mut offset = self.input_buffer_read_offset();
            if offset.is_multiple_of(BITS_PER_PACKET) {
                match first_frame_from(&input, offset / BITS_PER_PACKET) {
                    Some(first) => offset = first,
                    None => {
                        self.finish_buffer(looped);
                        continue;
                    }
                }
            }
            // A frame takes 4 blocks per channel; the decoder can tell a stream's channel count
            // better than the context (see xma_ffmpeg.rs).
            let frame = frame_bits(&input, offset);
            let hint = self.channel_count();
            let channels = match (decoder.as_deref_mut(), &frame) {
                (Some(decoder), Some((frame, bits))) => decoder.channels(frame, *bits, hint),
                _ => hint,
            } as u32;
            let frame_blocks = channels * BYTES_PER_FRAME_CHANNEL / BYTES_PER_OUTPUT_BLOCK;
            if free < frame_blocks {
                break;
            }
            let mut samples = vec![0; (frame_blocks * BYTES_PER_OUTPUT_BLOCK) as usize];
            let decoded = match (decoder.as_deref_mut(), &frame) {
                (Some(decoder), Some((frame, bits))) => decoder.decode(frame, *bits, &mut samples),
                _ => false,
            };
            if !decoded {
                samples.fill(0);
            }
            for (block, bytes) in (0..).zip(samples.chunks(BYTES_PER_OUTPUT_BLOCK as usize)) {
                let index = (write + block) % block_count;
                memory.write_bytes(output + index * BYTES_PER_OUTPUT_BLOCK, bytes);
            }
            write = (write + frame_blocks) % block_count;
            free -= frame_blocks;
            frames += 1;
            match next_frame(&input, offset) {
                Some(next) => self.set_input_buffer_read_offset(next),
                None => self.finish_buffer(looped),
            }
        }
        self.set(OUTPUT_BUFFER_WRITE_OFFSET, write);
        if write == read {
            self.set(OUTPUT_BUFFER_VALID, 0);
        }
        frames
    }

    /// The current input buffer's packets, if it is valid and mapped.
    fn current_input(
        &self,
        memory: &GuestMemory,
        guest_address: &impl Fn(u32) -> Option<u32>,
    ) -> Option<Vec<u8>> {
        let buffer = self.get(CURRENT_BUFFER) as usize;
        if !self.input_buffer_valid(buffer) {
            return None;
        }
        let pointer = self.get([INPUT_BUFFER_0_POINTER, INPUT_BUFFER_1_POINTER][buffer]);
        let packets = self.get([INPUT_BUFFER_0_PACKET_COUNT, INPUT_BUFFER_1_PACKET_COUNT][buffer]);
        let mut data = vec![0; (packets * BYTES_PER_PACKET) as usize];
        memory.read_bytes(guest_address(pointer)?, &mut data);
        Some(data)
    }

    /// Restarts at the loop start once the read offset reaches the loop end (or regardless,
    /// when the buffer has run out), counting the loop down. Returns whether it looped.
    fn loop_back(&mut self, at_buffer_end: bool) -> bool {
        let (count, start, end) = (
            self.get(LOOP_COUNT),
            self.get(LOOP_START),
            self.get(LOOP_END),
        );
        if count == 0 || start >= end || !(at_buffer_end || self.input_buffer_read_offset() >= end)
        {
            return false;
        }
        self.set_input_buffer_read_offset(start);
        if count < INFINITE_LOOPS {
            self.set(LOOP_COUNT, count - 1);
        }
        true
    }

    /// The current buffer has no more frames: loops back if a loop is due, otherwise releases
    /// the buffer and moves to the other one.
    fn finish_buffer(&mut self, looped: bool) {
        if looped || self.loop_back(true) {
            return;
        }
        let buffer = self.get(CURRENT_BUFFER);
        self.set(
            [INPUT_BUFFER_0_VALID, INPUT_BUFFER_1_VALID][buffer as usize],
            0,
        );
        self.set(CURRENT_BUFFER, buffer ^ 1);
        self.set_input_buffer_read_offset(0);
    }
}

/// `count` bits (at most 32) of `data` from bit `offset`, most significant first.
fn read_bits(data: &[u8], offset: u32, count: u32) -> Option<u32> {
    let mut value = 0u64;
    for bit in offset..offset + count {
        let byte = *data.get((bit / 8) as usize)?;
        value = (value << 1) | u64::from((byte >> (7 - bit % 8)) & 1);
    }
    Some(value as u32)
}

/// The bit offset (from the buffer start) of the first frame starting in packet `packet`, or
/// None if no frame starts there.
fn first_frame(input: &[u8], packet: u32) -> Option<u32> {
    const OFFSET_SHIFT_BITS: u32 = 6;
    const OFFSET_BITS: u32 = 15;
    let start = packet * BITS_PER_PACKET;
    let relative =
        read_bits(input, start + OFFSET_SHIFT_BITS, OFFSET_BITS)? + BITS_PER_PACKET_HEADER;
    (relative < BITS_PER_PACKET - FRAME_LENGTH_BITS).then_some(start + relative)
}

/// Where the frame after the one at `offset` starts: later in the same packet, or (when this is
/// the packet's last frame) at the first frame of the stream's next packet. None when the
/// buffer has no more packets.
fn next_frame(input: &[u8], offset: u32) -> Option<u32> {
    const SKIP_SHIFT_BITS: u32 = 24;
    const SKIP_BITS: u32 = 8;
    let packet = offset / BITS_PER_PACKET;
    let packet_end = (packet + 1) * BITS_PER_PACKET;
    if packet_end - offset >= FRAME_LENGTH_BITS {
        let length = read_bits(input, offset, FRAME_LENGTH_BITS)?;
        let end = offset + length;
        // A frame wholly inside the packet whose trailing bit says another frame follows.
        if length != INVALID_FRAME_LENGTH
            && length > FRAME_LENGTH_BITS
            && end < packet_end
            && read_bits(input, end - 1, 1)? == 1
        {
            return Some(end);
        }
    }
    let skip = read_bits(input, packet * BITS_PER_PACKET + SKIP_SHIFT_BITS, SKIP_BITS)?;
    first_frame_from(input, packet + skip + 1)
}

/// The bits of the frame at `offset`, packed most significant first, and how many there are. A
/// frame may continue in the stream's next packet, after that packet's header; None when it
/// continues past the end of this buffer.
fn frame_bits(input: &[u8], offset: u32) -> Option<(Vec<u8>, u32)> {
    const SKIP_SHIFT_BITS: u32 = 24;
    const SKIP_BITS: u32 = 8;
    let mut frame = Vec::new();
    let mut length = FRAME_LENGTH_BITS;
    let mut taken = 0;
    let mut position = offset;
    while taken < length {
        let packet = position / BITS_PER_PACKET;
        let packet_end = (packet + 1) * BITS_PER_PACKET;
        let count = (length - taken).min(packet_end - position);
        for bit in position..position + count {
            if taken.is_multiple_of(8) {
                frame.push(0);
            }
            let value = read_bits(input, bit, 1)? as u8;
            *frame.last_mut()? |= value << (7 - taken % 8);
            taken += 1;
            if taken == FRAME_LENGTH_BITS {
                length = read_bits(&frame, 0, FRAME_LENGTH_BITS)?;
                if length <= FRAME_LENGTH_BITS || length == INVALID_FRAME_LENGTH {
                    return None;
                }
            }
        }
        position += count;
        if taken < length && position == packet_end {
            let skip = read_bits(input, packet * BITS_PER_PACKET + SKIP_SHIFT_BITS, SKIP_BITS)?;
            position = (packet + skip + 1) * BITS_PER_PACKET + BITS_PER_PACKET_HEADER;
        }
    }
    Some((frame, length))
}

/// The first frame starting in packet `packet` or a later one of the buffer.
fn first_frame_from(input: &[u8], packet: u32) -> Option<u32> {
    let packets = input.len() as u32 / BYTES_PER_PACKET;
    (packet..packets).find_map(|packet| first_frame(input, packet))
}

fn mask(width: u32) -> u32 {
    if width == u32::BITS {
        u32::MAX
    } else {
        (1 << width) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXT: u32 = 0x1000;
    const OUTPUT: u32 = 0x2000;
    const OUTPUT_BLOCKS: u32 = 16;
    /// Physical addresses are guest addresses in these tests.
    fn identity(physical: u32) -> Option<u32> {
        Some(physical)
    }

    fn initialized(memory: &GuestMemory, packets: u32) -> XmaContext {
        let mut context = XmaContext::load(memory, CONTEXT);
        context.initialize(&Initialization {
            input_buffers: [(0x3000, packets), (0x4000, packets)],
            input_buffer_read_offset: BITS_PER_PACKET_HEADER,
            output_buffer: OUTPUT,
            output_buffer_block_count: OUTPUT_BLOCKS,
            subframe_decode_count: 0,
            channel_count: 2,
            sample_rate: 0,
            loop_start: 0,
            loop_end: 0,
            loop_count: 0,
            loop_subframe_end: 0,
            loop_subframe_skip: 0,
        });
        context
    }

    #[test]
    fn fields_round_trip_through_guest_memory_in_their_bits() {
        let memory = GuestMemory::reserve().unwrap();
        let mut context = initialized(&memory, 1);
        context.set_input_buffer_valid(1);
        context.set_output_buffer_read_offset(5);
        context.store(&memory);
        let reloaded = XmaContext::load(&memory, CONTEXT);
        assert!(reloaded.input_buffer_valid(1) && !reloaded.input_buffer_valid(0));
        assert_eq!(reloaded.output_buffer_read_offset(), 5);
        assert_eq!(
            memory.read_u32(CONTEXT) >> INPUT_BUFFER_1_VALID.shift & 1,
            1
        );
        assert_eq!(
            memory.read_u32(CONTEXT + IS_STEREO.word * WORD_SIZE) >> IS_STEREO.shift & 1,
            1
        );
    }

    #[test]
    fn storing_keeps_fields_the_title_wrote_meanwhile() {
        let memory = GuestMemory::reserve().unwrap();
        initialized(&memory, 1).store(&memory);
        let mut context = XmaContext::load(&memory, CONTEXT);
        // The title switches the stream to mono while the decoder holds the context.
        let mut title = XmaContext::load(&memory, CONTEXT);
        title.set(IS_STEREO, 0);
        title.store(&memory);
        context.set_output_buffer_read_offset(3);
        context.store(&memory);
        let reloaded = XmaContext::load(&memory, CONTEXT);
        assert_eq!(reloaded.channel_count(), 1);
        assert_eq!(reloaded.output_buffer_read_offset(), 3);
    }

    /// One packet holding frames of the given lengths, the first right after the header.
    fn packet(lengths: &[u32]) -> Vec<u8> {
        const XMA2_METADATA: u32 = 1;
        let mut bits = Vec::new();
        let mut push = |value: u32, count: u32| {
            bits.extend((0..count).rev().map(|bit| (value >> bit) & 1 == 1));
        };
        // Frame count, first frame offset (0: right after the header), metadata, skip count.
        push(lengths.len() as u32, 6);
        push(0, 15);
        push(XMA2_METADATA, 3);
        push(0, 8);
        for (index, &length) in lengths.iter().enumerate() {
            push(length, FRAME_LENGTH_BITS);
            // The frame's contents, then whether another frame follows.
            for _ in 0..length - FRAME_LENGTH_BITS - 1 {
                push(0, 1);
            }
            push(u32::from(index + 1 < lengths.len()), 1);
        }
        let mut bytes = vec![0u8; BYTES_PER_PACKET as usize];
        for (index, bit) in bits.into_iter().enumerate() {
            bytes[index / 8] |= u8::from(bit) << (7 - index % 8);
        }
        bytes
    }

    const FRAME_LENGTH: u32 = 1000;
    const MONO_FRAME_BLOCKS: u32 = 4;

    /// A mono context reading `packets` from buffer 0 (and the same from buffer 1), with room
    /// for `frames` decoded frames.
    fn ready(memory: &GuestMemory, packets: &[u8], frames: u32) -> XmaContext {
        memory.write_bytes(0x3000, packets);
        memory.write_bytes(0x8000, packets);
        let count = packets.len() as u32 / BYTES_PER_PACKET;
        let mut context = XmaContext::load(memory, CONTEXT);
        context.initialize(&Initialization {
            input_buffers: [(0x3000, count), (0x8000, count)],
            input_buffer_read_offset: BITS_PER_PACKET_HEADER,
            output_buffer: OUTPUT,
            output_buffer_block_count: frames * MONO_FRAME_BLOCKS,
            subframe_decode_count: 0,
            channel_count: 1,
            sample_rate: 0,
            loop_start: 0,
            loop_end: 0,
            loop_count: 0,
            loop_subframe_end: 0,
            loop_subframe_skip: 0,
        });
        context.set_input_buffer_valid(0);
        context.set_output_buffer_valid();
        context
    }

    #[test]
    fn a_pass_fills_the_ring_then_clears_output_valid() {
        let memory = GuestMemory::reserve().unwrap();
        let mut context = ready(&memory, &packet(&[FRAME_LENGTH; 4]), 2);
        context.decode(&memory, identity, None);
        // Two frames fill the ring: the write offset comes round to the read offset.
        assert_eq!(context.output_buffer_write_offset(), 0);
        assert!(!context.output_buffer_valid());
        assert_eq!(
            context.input_buffer_read_offset(),
            BITS_PER_PACKET_HEADER + 2 * FRAME_LENGTH
        );
    }

    #[test]
    fn a_used_up_buffer_is_released_for_the_other() {
        let memory = GuestMemory::reserve().unwrap();
        let mut context = ready(&memory, &packet(&[FRAME_LENGTH]), 4);
        context.set_input_buffer_valid(1);
        context.decode(&memory, identity, None);
        // One frame from each buffer, then no input: the ring is not full, so it stays valid.
        assert!(!context.input_buffer_valid(0) && !context.input_buffer_valid(1));
        assert_eq!(context.output_buffer_write_offset(), 2 * MONO_FRAME_BLOCKS);
        assert!(context.output_buffer_valid());
    }

    #[test]
    fn loops_return_to_the_loop_start_and_count_down() {
        let memory = GuestMemory::reserve().unwrap();
        let mut context = ready(&memory, &packet(&[FRAME_LENGTH; 2]), 4);
        context.set(LOOP_START, BITS_PER_PACKET_HEADER);
        context.set(LOOP_END, BITS_PER_PACKET_HEADER + FRAME_LENGTH);
        context.set(LOOP_COUNT, 2);
        context.decode(&memory, identity, None);
        // The first frame plays three times (once, then two loops), then the second frame.
        assert_eq!(context.get(LOOP_COUNT), 0);
        assert!(!context.input_buffer_valid(0));
        assert!(!context.output_buffer_valid());
    }

    #[test]
    fn a_frame_continuing_in_the_next_packet_is_put_back_together() {
        // The frame starts right after the first packet's header and runs 100 bits into the
        // second, after that packet's own header.
        const SPILL: u32 = 100;
        let length = BITS_PER_PACKET - BITS_PER_PACKET_HEADER + SPILL;
        let pattern = |index: u32| index.is_multiple_of(3);
        let mut input = vec![0u8; 2 * BYTES_PER_PACKET as usize];
        let mut set = |bit: u32, value: bool| {
            input[(bit / 8) as usize] |= u8::from(value) << (7 - bit % 8);
        };
        let stream_bit = |index: u32| {
            let in_first = BITS_PER_PACKET - BITS_PER_PACKET_HEADER;
            if index < in_first {
                BITS_PER_PACKET_HEADER + index
            } else {
                BITS_PER_PACKET + BITS_PER_PACKET_HEADER + index - in_first
            }
        };
        for index in 0..length {
            let value = if index < FRAME_LENGTH_BITS {
                (length >> (FRAME_LENGTH_BITS - 1 - index)) & 1 == 1
            } else {
                pattern(index)
            };
            set(stream_bit(index), value);
        }
        let (frame, bits) = frame_bits(&input, BITS_PER_PACKET_HEADER).unwrap();
        assert_eq!(bits, length);
        for index in [
            FRAME_LENGTH_BITS,
            length - SPILL - 1,
            length - SPILL,
            length - 1,
        ] {
            assert_eq!(read_bits(&frame, index, 1), Some(u32::from(pattern(index))));
        }
    }

    #[test]
    fn without_input_the_output_buffer_is_handed_back() {
        let memory = GuestMemory::reserve().unwrap();
        let mut context = ready(&memory, &packet(&[FRAME_LENGTH]), 2);
        context.set(INPUT_BUFFER_0_VALID, 0);
        context.decode(&memory, identity, None);
        assert!(!context.output_buffer_valid());
    }
}
