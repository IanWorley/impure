//! XMA decoder imports. Contexts live in physical memory. Enabling a context runs one decoding
//! pass at once (see xma.rs), as if the hardware decoded instantly, which is how Xenia's
//! decoder behaves too. One lock serializes read-modify-writes of context words.

use std::sync::Mutex;

use super::{ImportCall, NtStatus, memory};
use crate::physical_memory::{self, Protection};
use crate::xma::{CONTEXT_SIZE, Initialization, XmaContext};

static CONTEXTS: Mutex<()> = Mutex::new(());
const ALLOCATION_FAILED: u32 = 0;
/// XMAInitializeContext and XMADisableContext succeed with 0.
const SUCCESS: u32 = 0;

/// XMA_CONTEXT_INIT field offsets; the loop data's last three fields are bytes.
const INIT_INPUT_BUFFER_0: u32 = 0x00;
const INIT_INPUT_BUFFER_0_PACKETS: u32 = 0x04;
const INIT_INPUT_BUFFER_1: u32 = 0x08;
const INIT_INPUT_BUFFER_1_PACKETS: u32 = 0x0C;
const INIT_INPUT_READ_OFFSET: u32 = 0x10;
const INIT_OUTPUT_BUFFER: u32 = 0x14;
const INIT_OUTPUT_BLOCKS: u32 = 0x18;
const INIT_SUBFRAME_DECODE_COUNT: u32 = 0x20;
const INIT_CHANNEL_COUNT: u32 = 0x24;
const INIT_SAMPLE_RATE: u32 = 0x28;
const INIT_LOOP_START: u32 = 0x2C;
const INIT_LOOP_END: u32 = 0x30;
const INIT_LOOP_COUNT: u32 = 0x34;
const INIT_LOOP_SUBFRAME_END: u32 = 0x35;
const INIT_LOOP_SUBFRAME_SKIP: u32 = 0x36;

/// XMACreateContext(context out) -> NTSTATUS.
pub(super) fn create_context(call: &ImportCall) -> u32 {
    let (lowest, highest) = (0, u32::MAX);
    let context = memory::allocate_physical(CONTEXT_SIZE, 0, lowest, highest, Protection(0));
    call.memory.write_u32(call.argument(0), context);
    if context == ALLOCATION_FAILED {
        NtStatus::NO_MEMORY.0
    } else {
        NtStatus::SUCCESS.0
    }
}

/// XMAReleaseContext(context).
pub(super) fn release_context(call: &ImportCall) -> u32 {
    let _lock = CONTEXTS.lock().unwrap();
    memory::free_physical(call.argument(0));
    SUCCESS
}

/// XMAInitializeContext(context, initialization) -> 0.
pub(super) fn initialize_context(call: &ImportCall) -> u32 {
    let (context, init) = (call.argument(0), call.argument(1));
    let memory = call.memory;
    let word = |offset| memory.read_u32(init + offset);
    let byte = |offset| u32::from(memory.read_u8(init + offset));
    // Input buffers are optional; the context holds physical addresses.
    let physical = |address: u32| {
        if address == 0 {
            0
        } else {
            physical_memory::physical_address(address)
        }
    };
    let initialization = Initialization {
        input_buffers: [
            (
                physical(word(INIT_INPUT_BUFFER_0)),
                word(INIT_INPUT_BUFFER_0_PACKETS),
            ),
            (
                physical(word(INIT_INPUT_BUFFER_1)),
                word(INIT_INPUT_BUFFER_1_PACKETS),
            ),
        ],
        input_buffer_read_offset: word(INIT_INPUT_READ_OFFSET),
        output_buffer: physical(word(INIT_OUTPUT_BUFFER)),
        output_buffer_block_count: word(INIT_OUTPUT_BLOCKS),
        subframe_decode_count: word(INIT_SUBFRAME_DECODE_COUNT),
        channel_count: word(INIT_CHANNEL_COUNT),
        sample_rate: word(INIT_SAMPLE_RATE),
        loop_start: word(INIT_LOOP_START),
        loop_end: word(INIT_LOOP_END),
        loop_count: byte(INIT_LOOP_COUNT),
        loop_subframe_end: byte(INIT_LOOP_SUBFRAME_END),
        loop_subframe_skip: byte(INIT_LOOP_SUBFRAME_SKIP),
    };
    update(call, context, |state| state.initialize(&initialization));
    SUCCESS
}

/// XMAEnableContext(context) -> 0. Runs a decoding pass.
pub(super) fn enable_context(call: &ImportCall) -> u32 {
    update(call, call.argument(0), |context| {
        context.decode(call.memory, memory::view_address)
    });
    SUCCESS
}

/// XMADisableContext(context, wait) -> 0. Decoding passes finish within XMAEnableContext, so
/// there is nothing to stop or wait for.
pub(super) fn disable_context(_: &ImportCall) -> u32 {
    SUCCESS
}

pub(super) fn is_input_buffer_0_valid(call: &ImportCall) -> u32 {
    u32::from(read(call, |context| context.input_buffer_valid(0)))
}

pub(super) fn is_input_buffer_1_valid(call: &ImportCall) -> u32 {
    u32::from(read(call, |context| context.input_buffer_valid(1)))
}

pub(super) fn set_input_buffer_0_valid(call: &ImportCall) -> u32 {
    update(call, call.argument(0), |context| {
        context.set_input_buffer_valid(0)
    });
    SUCCESS
}

pub(super) fn set_input_buffer_1_valid(call: &ImportCall) -> u32 {
    update(call, call.argument(0), |context| {
        context.set_input_buffer_valid(1)
    });
    SUCCESS
}

pub(super) fn is_output_buffer_valid(call: &ImportCall) -> u32 {
    u32::from(read(call, XmaContext::output_buffer_valid))
}

pub(super) fn set_output_buffer_valid(call: &ImportCall) -> u32 {
    update(call, call.argument(0), XmaContext::set_output_buffer_valid);
    SUCCESS
}

pub(super) fn get_input_buffer_read_offset(call: &ImportCall) -> u32 {
    read(call, XmaContext::input_buffer_read_offset)
}

pub(super) fn set_input_buffer_read_offset(call: &ImportCall) -> u32 {
    let offset = call.argument(1);
    update(call, call.argument(0), |context| {
        context.set_input_buffer_read_offset(offset)
    });
    SUCCESS
}

pub(super) fn get_output_buffer_read_offset(call: &ImportCall) -> u32 {
    read(call, XmaContext::output_buffer_read_offset)
}

pub(super) fn set_output_buffer_read_offset(call: &ImportCall) -> u32 {
    let offset = call.argument(1);
    update(call, call.argument(0), |context| {
        context.set_output_buffer_read_offset(offset)
    });
    SUCCESS
}

pub(super) fn get_output_buffer_write_offset(call: &ImportCall) -> u32 {
    read(call, XmaContext::output_buffer_write_offset)
}

/// Reads a field of the context in argument 0.
fn read<T>(call: &ImportCall, field: impl FnOnce(&XmaContext) -> T) -> T {
    let _lock = CONTEXTS.lock().unwrap();
    field(&XmaContext::load(call.memory, call.argument(0)))
}

fn update(call: &ImportCall, context: u32, change: impl FnOnce(&mut XmaContext)) {
    let _lock = CONTEXTS.lock().unwrap();
    let mut state = XmaContext::load(call.memory, context);
    change(&mut state);
    state.store(call.memory);
}
