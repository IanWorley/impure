//! Video driver (Vd*) imports. There is no GPU yet: these accept the title's setup so boot can
//! continue, and a stand-in command processor consumes what the title submits.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{LazyLock, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use bevy::log::{debug, info, trace, warn};

use super::{ImportCall, NO_RETURN_VALUE, memory, threads};
use crate::command_processor::{
    self, CommandProcessor, WRITE_POINTER_ADDRESS, read_register, swap_packet,
};
use crate::gpu::{
    Draw, EDRAM_MODE_COPY, EDRAM_MODE_MASK, FETCH_CONSTANT_WORDS, FETCH_CONSTANTS, Shader,
    ShaderStage, TextureDescriptor, VIEWPORT_REGISTERS, registers,
};
use crate::guest::{self, ThreadEnvironment};
use crate::memory::GuestMemory;
use crate::physical_memory::{self, Protection};
use crate::renderer;
use crate::shader::{Instruction, Program};
use crate::status;
use crate::translate;

static COMMAND_PROCESSOR: LazyLock<Mutex<CommandProcessor>> =
    LazyLock::new(|| Mutex::new(CommandProcessor::new()));
/// Draws issued since the last VdSwap, and in the frame before it.
static DRAWS_THIS_FRAME: AtomicU32 = AtomicU32::new(0);
static DRAWS_LAST_FRAME: AtomicU32 = AtomicU32::new(0);
/// Loaded shaders whose WGSL translation passes validation.
static VALID_SHADERS: AtomicU32 = AtomicU32::new(0);
static COMMAND_PROCESSOR_STARTED: Once = Once::new();
/// The title's graphics interrupt callback and its user data.
static INTERRUPT_CALLBACK: Mutex<Option<(u32, u32)>> = Mutex::new(None);
static INTERRUPTS_STARTED: Once = Once::new();
/// Command processor interrupts waiting for the interrupt thread, by CPU.
static PENDING_INTERRUPTS: Mutex<Option<Sender<u32>>> = Mutex::new(None);
const VERTICAL_BLANK_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 60);
const INTERRUPT_THREAD_NAME: &str = "graphics-interrupts";
/// The interrupt callback's first argument: what raised the interrupt.
const VERTICAL_BLANK_SOURCE: u32 = 0;
const COMMAND_PROCESSOR_SOURCE: u32 = 1;
/// Vertical blank interrupts arrive on hardware thread 2, as Xenia delivers them.
const VERTICAL_BLANK_CPU: u32 = 2;
const INTERRUPT_STACK_SIZE: u32 = 0x1_0000;
/// How often the command processor checks for submitted commands; well under a 60 Hz frame.
const COMMAND_POLL_INTERVAL: Duration = Duration::from_micros(500);
const COMMAND_PROCESSOR_THREAD_NAME: &str = "command-processor";
const WORD_SIZE: u32 = 4;

/// Gives the command processor guest memory and the title's physical-address mapping.
struct KernelEnvironment;

impl command_processor::Environment for KernelEnvironment {
    fn memory(&self) -> &GuestMemory {
        guest::memory()
    }

    fn guest_address(&self, physical: u32) -> Option<u32> {
        memory::view_address(physical)
    }

    fn report(&self, message: String) {
        info!(target: "gpu", "{message}");
    }

    fn trace(&self, message: String) {
        trace!(target: "gpu", "{message}");
    }

    fn draw(&self, draw: &Draw) {
        DRAWS_THIS_FRAME.fetch_add(1, Ordering::Relaxed);
        let hash = |shader: &Option<Shader>| shader.as_ref().map(|shader| shader.hash);
        trace!(target: "draw", "primitive {:#x}, {} indices from {:?}, shaders {:x?}/{:x?}; {}",
            draw.initiator.primitive, draw.initiator.index_count, draw.indices,
            hash(&draw.vertex_shader), hash(&draw.pixel_shader), describe_render_state(guest::memory()));
        if let Some(pixel_shader) = &draw.pixel_shader {
            trace!(target: "draw", "textures: {}", describe_textures(guest::memory(), pixel_shader));
        }
        renderer::with(|renderer| renderer.draw(guest::memory(), &memory::view_address, draw));
    }

    fn swap(&self, front_buffer: u32, width: u32, height: u32) {
        trace!(target: "draw", "swap: front buffer at physical {front_buffer:#010x}, {width}x{height}");
        renderer::with(|renderer| renderer.swap(front_buffer));
    }

    fn shader_loaded(&self, stage: ShaderStage, shader: &Shader) {
        let program = Program::decode(&shader.words);
        debug!(target: "shader", "{stage:?} shader {:016x}: {} words, {} control-flow and {} ALU/fetch instructions",
            shader.hash, shader.words.len(), program.control_flow.len(), program.instructions.len());
        trace!(target: "shader", "{stage:?} shader {:016x}:\n{}", shader.hash, program.disassemble());
        let translation = translate::translate(stage, &shader.words);
        match translate::validate(&translation.wgsl) {
            Ok(()) => {
                VALID_SHADERS.fetch_add(1, Ordering::Relaxed);
                debug!(target: "shader", "{stage:?} shader {:016x}: valid WGSL, not translated: {:?}",
                    shader.hash, translation.unsupported);
            }
            Err(error) => {
                warn!(target: "shader", "{stage:?} shader {:016x}: invalid WGSL: {error}", shader.hash)
            }
        }
        trace!(target: "shader", "{stage:?} shader {:016x} as WGSL:\n{}", shader.hash, translation.wgsl);
    }

    fn raise_interrupt(&self, cpu: u32) {
        if let Some(pending) = PENDING_INTERRUPTS.lock().unwrap().as_ref() {
            let _ = pending.send(cpu);
        }
    }
}

/// VdInitializeRingBuffer's size argument counts 8-byte units, as a power of two.
const RING_SIZE_UNIT_LOG2: u32 = 3;

/// The display mode the runtime reports: 720p, widescreen, 60 Hz, as an HDMI console would.
const DISPLAY_WIDTH: u32 = 1280;
const DISPLAY_HEIGHT: u32 = 720;
const REFRESH_RATE_HZ: f32 = 60.0;
const NTSC_VIDEO_STANDARD: u32 = 1;
/// The narrowest and widest displays that count as high definition and as 1080-line.
const HIGH_DEFINITION_WIDTH: u32 = 1024;
const FULL_HIGH_DEFINITION_WIDTH: u32 = 1920;

/// X_VIDEO_MODE field offsets.
const MODE_WIDTH: u32 = 0x00;
const MODE_HEIGHT: u32 = 0x04;
const MODE_INTERLACED: u32 = 0x08;
const MODE_WIDESCREEN: u32 = 0x0C;
const MODE_HIGH_DEFINITION: u32 = 0x10;
const MODE_REFRESH_RATE: u32 = 0x14;
const MODE_VIDEO_STANDARD: u32 = 0x18;
/// Two fields whose meaning is unknown; set to the values Xenia reports.
const MODE_UNKNOWN_0X1C: (u32, u32) = (0x1C, 0x4A);
const MODE_UNKNOWN_0X20: (u32, u32) = (0x20, 0x01);
const MODE_SIZE: u32 = 0x30;

/// VdGetCurrentDisplayGamma: a power curve, with a standard television exponent (as Xenia
/// reports).
const POWER_GAMMA_CURVE: u32 = 2;
const GAMMA_POWER: f32 = 2.222_222_3;

/// VdQueryVideoFlags bits.
const VIDEO_FLAG_WIDESCREEN: u32 = 0x1;
const VIDEO_FLAG_HIGH_DEFINITION: u32 = 0x2;
const VIDEO_FLAG_FULL_HIGH_DEFINITION: u32 = 0x4;

/// VdIsHSIOTrainingSucceeded's result: the GPU link trained.
const TRAINING_SUCCEEDED: u32 = 1;
/// VdRetrainEDRAM's result for success.
const RETRAIN_SUCCEEDED: u32 = 0;

/// VdGetSystemCommandBuffer fills a block this size and two markers, as Xenia's stub does.
const SYSTEM_COMMAND_BUFFER_SIZE: usize = 0x94;
const SYSTEM_COMMAND_BUFFER_MARKERS: [u32; 2] = [0xBEEF_0000, 0xBEEF_0001];

/// VdSwap fills the 64 words the title reserved in its command stream: the swap packet, then
/// type-2 (no-op) packets.
const SWAP_RESERVED_WORDS: u32 = 64;
const TYPE2_NOP_PACKET: u32 = 0x8000_0000;
/// VdSwap's front buffer arguments, all pointers: to its address, width and height.
const SWAP_FRONT_BUFFER_ARGUMENT: u32 = 5;
const SWAP_WIDTH_ARGUMENT: u32 = 8;
const SWAP_HEIGHT_ARGUMENT: u32 = 9;
static FRAMES_PRESENTED: AtomicU32 = AtomicU32::new(0);
/// How often presentation progress is logged at info level: every ten seconds at 60 Hz.
const FRAMES_PER_PROGRESS_LINE: u32 = 600;

/// X_DISPLAY_INFO field offsets; the scaler parameters start at 0x8. Sizes are 16-bit except
/// the scaler fields and the refresh rate.
const DISPLAY_INFO_SIZE: usize = 0x58;
const INFO_FRONT_BUFFER_WIDTH: u32 = 0x00;
const INFO_FRONT_BUFFER_HEIGHT: u32 = 0x02;
const INFO_SOURCE_RECT_RIGHT: u32 = 0x10;
const INFO_SOURCE_RECT_BOTTOM: u32 = 0x14;
const INFO_SCALED_OUTPUT_WIDTH: u32 = 0x18;
const INFO_SCALED_OUTPUT_HEIGHT: u32 = 0x1C;
const INFO_VERTICAL_FILTER_TYPE: u32 = 0x20;
const INFO_HORIZONTAL_FILTER_TYPE: u32 = 0x30;
const INFO_OVERSCAN_LEFT: u32 = 0x40;
const INFO_OVERSCAN_TOP: u32 = 0x42;
const INFO_OVERSCAN_RIGHT: u32 = 0x44;
const INFO_OVERSCAN_BOTTOM: u32 = 0x46;
const INFO_DISPLAY_WIDTH: u32 = 0x48;
const INFO_DISPLAY_HEIGHT: u32 = 0x4A;
const INFO_REFRESH_RATE: u32 = 0x4C;
const INFO_ACTUAL_DISPLAY_WIDTH: u32 = 0x56;
/// The scaler's filter, and the overscan margins (a quarter of each dimension), as Xenia reports.
const SCALER_FILTER_TYPE: u32 = 1;
const OVERSCAN_HORIZONTAL: u16 = (DISPLAY_WIDTH / 4) as u16;
const OVERSCAN_VERTICAL: u16 = (DISPLAY_HEIGHT / 4) as u16;

/// VdInitializeScalerCommandBuffer's destination and word count arguments.
const SCALER_DESTINATION_ARGUMENT: u32 = 10;
const SCALER_WORD_COUNT_ARGUMENT: u32 = 11;

/// VdPersistDisplay hands back a block of this size, which the title later frees with
/// MmFreePhysicalMemory, and reports success.
const PERSISTED_DISPLAY_SIZE: u32 = 64;
const DISPLAY_PERSISTED: u32 = 1;

/// VdSetDisplayMode's result for success.
const DISPLAY_MODE_SET: u32 = 0;

/// VdInitializeEngines' result for success.
const ENGINES_INITIALIZED: u32 = 1;

/// VdInitializeEngines(unknown, callback, argument, PFP microcode, ME microcode) -> success.
/// The microcode is for the real GPU's command processor, which the runtime does not run.
pub(super) fn initialize_engines(_: &ImportCall) -> u32 {
    ENGINES_INITIALIZED
}

/// VdSetGraphicsInterruptCallback(callback, user data). Starts graphics interrupts: the runtime
/// calls `callback(source, user data)` as its own guest thread, as the console's GPU interrupts
/// a CPU core, 60 times a second for a vertical blank and whenever the command processor asks.
/// Titles pace frames and acknowledge GPU work there.
pub(super) fn set_graphics_interrupt_callback(call: &ImportCall) -> u32 {
    let (callback, user_data) = (call.argument(0), call.argument(1));
    info!(target: "video", "graphics interrupt callback {callback:#010x} (user data {user_data:#010x})");
    *INTERRUPT_CALLBACK.lock().unwrap() = Some((callback, user_data));
    INTERRUPTS_STARTED.call_once(|| {
        let (sender, receiver) = mpsc::channel();
        *PENDING_INTERRUPTS.lock().unwrap() = Some(sender);
        let started =
            threads::prepare_system_thread(INTERRUPT_STACK_SIZE).and_then(|environment| {
                guest::spawn(INTERRUPT_THREAD_NAME.to_string(), move || {
                    interrupt_loop(environment, receiver)
                })
            });
        if let Err(error) = started {
            status::halt_guest_thread(format!("cannot start vertical blank interrupts: {error}"));
        }
    });
    NO_RETURN_VALUE
}

/// Delivers command processor interrupts as they come, and a vertical blank every period.
fn interrupt_loop(environment: ThreadEnvironment, pending: Receiver<u32>) -> ! {
    // Advancing the deadline by whole periods keeps the rate exact however long callbacks take.
    let mut next_vertical_blank = Instant::now() + VERTICAL_BLANK_PERIOD;
    loop {
        let wait = next_vertical_blank.saturating_duration_since(Instant::now());
        match pending.recv_timeout(wait) {
            Ok(cpu) => dispatch_interrupt(environment, COMMAND_PROCESSOR_SOURCE, cpu),
            Err(RecvTimeoutError::Timeout) => {
                next_vertical_blank += VERTICAL_BLANK_PERIOD;
                COMMAND_PROCESSOR
                    .lock()
                    .unwrap()
                    .mark_vertical_blank(guest::memory());
                dispatch_interrupt(environment, VERTICAL_BLANK_SOURCE, VERTICAL_BLANK_CPU);
            }
            Err(RecvTimeoutError::Disconnected) => unreachable!("the sender lives in a static"),
        }
    }
}

fn dispatch_interrupt(environment: ThreadEnvironment, source: u32, cpu: u32) {
    let callback = *INTERRUPT_CALLBACK.lock().unwrap();
    if let Some((function, user_data)) = callback {
        guest::set_current_cpu(environment, cpu as u8);
        guest::call(environment, function, [source, user_data]);
    }
}

/// VdSetSystemCommandBufferGpuIdentifierAddress(address). Where the GPU would write a fence
/// identifier as it consumes the system command buffer; nothing consumes it yet.
pub(super) fn set_system_command_buffer_gpu_identifier_address(_: &ImportCall) -> u32 {
    NO_RETURN_VALUE
}

/// VdInitializeRingBuffer(physical address, log2 of the size in 8-byte units). Starts the
/// command processor.
pub(super) fn initialize_ring_buffer(call: &ImportCall) -> u32 {
    let (base, size_log2) = (call.argument(0), call.argument(1));
    let size = 1u32 << (size_log2 + RING_SIZE_UNIT_LOG2);
    info!(target: "video", "ring buffer at physical {base:#010x}, {size:#x} bytes");
    let Some(address) = memory::view_address(base) else {
        status::halt_guest_thread(format!(
            "ring buffer at physical {base:#010x}, which the title did not allocate"
        ));
    };
    COMMAND_PROCESSOR
        .lock()
        .unwrap()
        .set_ring(call.memory, address, size / WORD_SIZE);
    COMMAND_PROCESSOR_STARTED.call_once(|| {
        thread::Builder::new()
            .name(COMMAND_PROCESSOR_THREAD_NAME.to_string())
            .spawn(|| {
                loop {
                    if COMMAND_PROCESSOR.lock().unwrap().poll(&KernelEnvironment) {
                        let write_pointer = guest::memory().read_u32(WRITE_POINTER_ADDRESS);
                        debug!(target: "gpu", "executed ring up to word {write_pointer:#x}");
                    }
                    thread::sleep(COMMAND_POLL_INTERVAL);
                }
            })
            .expect("cannot start the command processor thread");
    });
    NO_RETURN_VALUE
}

/// VdEnableRingBufferRPtrWriteBack(physical address, log2 of the write-back block size). The
/// stand-in writes the read pointer back after every poll, which satisfies any block size.
pub(super) fn enable_ring_buffer_read_pointer_writeback(call: &ImportCall) -> u32 {
    let physical = call.argument(0);
    let Some(address) = memory::view_address(physical) else {
        status::halt_guest_thread(format!(
            "ring buffer read pointer write-back to physical {physical:#010x}, which the title did not allocate"
        ));
    };
    info!(target: "video", "read pointer written back to physical {physical:#010x} (guest {address:#010x}), block size 2^{}", call.argument(1));
    COMMAND_PROCESSOR
        .lock()
        .unwrap()
        .enable_read_pointer_writeback(call.memory, address);
    NO_RETURN_VALUE
}

/// VdQueryVideoMode(mode out).
pub(super) fn query_video_mode(call: &ImportCall) -> u32 {
    let mode = call.argument(0);
    let memory = call.memory;
    memory.write_bytes(mode, &[0; MODE_SIZE as usize]);
    memory.write_u32(mode + MODE_WIDTH, DISPLAY_WIDTH);
    memory.write_u32(mode + MODE_HEIGHT, DISPLAY_HEIGHT);
    memory.write_u32(mode + MODE_INTERLACED, u32::from(false));
    memory.write_u32(mode + MODE_WIDESCREEN, u32::from(true));
    memory.write_u32(
        mode + MODE_HIGH_DEFINITION,
        u32::from(DISPLAY_WIDTH >= HIGH_DEFINITION_WIDTH),
    );
    memory.write_u32(mode + MODE_REFRESH_RATE, REFRESH_RATE_HZ.to_bits());
    memory.write_u32(mode + MODE_VIDEO_STANDARD, NTSC_VIDEO_STANDARD);
    for (offset, value) in [MODE_UNKNOWN_0X1C, MODE_UNKNOWN_0X20] {
        memory.write_u32(mode + offset, value);
    }
    NO_RETURN_VALUE
}

/// VdQueryVideoFlags() -> flags describing the display mode.
pub(super) fn query_video_flags(_: &ImportCall) -> u32 {
    let mut flags = VIDEO_FLAG_WIDESCREEN;
    if DISPLAY_WIDTH >= HIGH_DEFINITION_WIDTH {
        flags |= VIDEO_FLAG_HIGH_DEFINITION;
    }
    if DISPLAY_WIDTH >= FULL_HIGH_DEFINITION_WIDTH {
        flags |= VIDEO_FLAG_FULL_HIGH_DEFINITION;
    }
    flags
}

/// VdCallGraphicsNotificationRoutines(event, arguments) -> 0. Calls the routines registered for
/// display changes; nothing in the runtime registers any.
pub(super) fn call_graphics_notification_routines(_: &ImportCall) -> u32 {
    NO_RETURN_VALUE
}

/// VdGetCurrentDisplayGamma(curve type out, power out).
pub(super) fn get_current_display_gamma(call: &ImportCall) -> u32 {
    call.memory.write_u32(call.argument(0), POWER_GAMMA_CURVE);
    call.memory
        .write_u32(call.argument(1), GAMMA_POWER.to_bits());
    NO_RETURN_VALUE
}

/// VdRetrainEDRAM(...) and VdRetrainEDRAMWorker(...) -> 0. Recalibrate the GPU's embedded
/// memory; there is no hardware to calibrate.
pub(super) fn retrain_edram(_: &ImportCall) -> u32 {
    RETRAIN_SUCCEEDED
}

/// VdIsHSIOTrainingSucceeded() -> whether the CPU-GPU link calibrated.
pub(super) fn is_hsio_training_succeeded(_: &ImportCall) -> u32 {
    TRAINING_SUCCEEDED
}

/// VdGetSystemCommandBuffer(buffer out, second out). The kernel's own command buffer, which the
/// runtime does not have; like Xenia, it clears the block and writes recognizable markers.
pub(super) fn get_system_command_buffer(call: &ImportCall) -> u32 {
    let (buffer, second) = (call.argument(0), call.argument(1));
    call.memory
        .write_bytes(buffer, &[0; SYSTEM_COMMAND_BUFFER_SIZE]);
    let [first_marker, second_marker] = SYSTEM_COMMAND_BUFFER_MARKERS;
    call.memory.write_u32(buffer, first_marker);
    call.memory.write_u32(second, second_marker);
    NO_RETURN_VALUE
}

/// VdSwap(command buffer, fetch constant, ..., front buffer, format, color space, width, height)
/// -> 0. Queues the frame's presentation behind the commands already submitted, and counts it.
pub(super) fn swap(call: &ImportCall) -> u32 {
    let buffer = call.argument(0);
    let width = call.memory.read_u32(call.argument(SWAP_WIDTH_ARGUMENT));
    let height = call.memory.read_u32(call.argument(SWAP_HEIGHT_ARGUMENT));
    let front_buffer = physical_memory::physical_address(
        call.memory
            .read_u32(call.argument(SWAP_FRONT_BUFFER_ARGUMENT)),
    );
    let packet = swap_packet(front_buffer, width, height);
    let padding = std::iter::repeat(TYPE2_NOP_PACKET);
    for (index, word) in (0..SWAP_RESERVED_WORDS).zip(packet.into_iter().chain(padding)) {
        call.memory.write_u32(buffer + index * WORD_SIZE, word);
    }
    let frame = FRAMES_PRESENTED.fetch_add(1, Ordering::Relaxed) + 1;
    let draws = DRAWS_THIS_FRAME.swap(0, Ordering::Relaxed);
    DRAWS_LAST_FRAME.store(draws, Ordering::Relaxed);
    if frame == 1 {
        info!(target: "video", "first frame presented ({width}x{height})");
    } else if frame.is_multiple_of(FRAMES_PER_PROGRESS_LINE) {
        let shaders = COMMAND_PROCESSOR.lock().unwrap().shader_count();
        let valid = VALID_SHADERS.load(Ordering::Relaxed);
        info!(target: "video", "{frame} frames presented ({draws} draws in the last, {shaders} distinct shaders loaded, {valid} valid as WGSL)");
    } else {
        debug!(target: "video", "frame {frame} presented ({width}x{height}, {draws} draws)");
    }
    NO_RETURN_VALUE
}

/// VdSetDisplayMode(mode) -> 0. The display keeps the single mode the runtime reports.
pub(super) fn set_display_mode(_: &ImportCall) -> u32 {
    DISPLAY_MODE_SET
}

/// VdGetCurrentDisplayInformation(information out). Describes the one display mode the runtime
/// reports, with the scaler passing the front buffer through unscaled.
pub(super) fn get_current_display_information(call: &ImportCall) -> u32 {
    let (info, memory) = (call.argument(0), call.memory);
    let (width, height) = (DISPLAY_WIDTH as u16, DISPLAY_HEIGHT as u16);
    memory.write_bytes(info, &[0; DISPLAY_INFO_SIZE]);
    for (offset, value) in [
        (INFO_FRONT_BUFFER_WIDTH, width),
        (INFO_FRONT_BUFFER_HEIGHT, height),
        (INFO_OVERSCAN_LEFT, OVERSCAN_HORIZONTAL),
        (INFO_OVERSCAN_TOP, OVERSCAN_VERTICAL),
        (INFO_OVERSCAN_RIGHT, OVERSCAN_HORIZONTAL),
        (INFO_OVERSCAN_BOTTOM, OVERSCAN_VERTICAL),
        (INFO_DISPLAY_WIDTH, width),
        (INFO_DISPLAY_HEIGHT, height),
        (INFO_ACTUAL_DISPLAY_WIDTH, width),
    ] {
        memory.write_u16(info + offset, value);
    }
    for (offset, value) in [
        (INFO_SOURCE_RECT_RIGHT, DISPLAY_WIDTH),
        (INFO_SOURCE_RECT_BOTTOM, DISPLAY_HEIGHT),
        (INFO_SCALED_OUTPUT_WIDTH, DISPLAY_WIDTH),
        (INFO_SCALED_OUTPUT_HEIGHT, DISPLAY_HEIGHT),
        (INFO_VERTICAL_FILTER_TYPE, SCALER_FILTER_TYPE),
        (INFO_HORIZONTAL_FILTER_TYPE, SCALER_FILTER_TYPE),
        (INFO_REFRESH_RATE, REFRESH_RATE_HZ.to_bits()),
    ] {
        memory.write_u32(info + offset, value);
    }
    NO_RETURN_VALUE
}

/// VdInitializeScalerCommandBuffer(scaler parameters..., destination, word count) -> words
/// written. The scaler passes the front buffer through unscaled, so the commands are no-ops.
pub(super) fn initialize_scaler_command_buffer(call: &ImportCall) -> u32 {
    let destination = call.argument(SCALER_DESTINATION_ARGUMENT);
    let count = call.argument(SCALER_WORD_COUNT_ARGUMENT);
    for index in 0..count {
        call.memory
            .write_u32(destination + index * WORD_SIZE, TYPE2_NOP_PACKET);
    }
    count
}

/// VdPersistDisplay(unknown, block out) -> 1. Keeps the last frame on screen across a title
/// change; with nothing on screen, it only provides the block the title expects to free.
pub(super) fn persist_display(call: &ImportCall) -> u32 {
    let block_out = call.argument(1);
    if block_out != 0 {
        let (lowest, highest) = (0, u32::MAX);
        let block =
            memory::allocate_physical(PERSISTED_DISPLAY_SIZE, 0, lowest, highest, Protection(0));
        call.memory.write_u32(block_out, block);
    }
    DISPLAY_PERSISTED
}

/// The render target, viewport and resolve registers behind a draw, for draw tracing.
fn describe_render_state(memory: &GuestMemory) -> String {
    use registers::*;
    let register = |index| read_register(memory, index);
    let viewport: Vec<f32> = (0..VIEWPORT_REGISTERS)
        .map(|offset| f32::from_bits(register(PA_CL_VPORT_XSCALE + offset)))
        .collect();
    let mode = register(RB_MODECONTROL) & EDRAM_MODE_MASK;
    let mut text = format!(
        "edram mode {mode}, surface {:#x}, color {:#x}, depth {:#x}, mask {:#x}, vte {:#x}, viewport {viewport:?}, window offset {:#x}, scissor {:#x}-{:#x}, depth control {:#x}, blend {:#x}, mode control {:#x}, program control {:#x}",
        register(RB_SURFACE_INFO),
        register(RB_COLOR_INFO),
        register(RB_DEPTH_INFO),
        register(RB_COLOR_MASK),
        register(PA_CL_VTE_CNTL),
        register(PA_SC_WINDOW_OFFSET),
        register(PA_SC_WINDOW_SCISSOR_TL),
        register(PA_SC_WINDOW_SCISSOR_BR),
        register(RB_DEPTHCONTROL),
        register(RB_BLENDCONTROL0),
        register(PA_SU_SC_MODE_CNTL),
        register(SQ_PROGRAM_CNTL),
    );
    if mode == EDRAM_MODE_COPY {
        text += &format!(
            "; copy control {:#x}, to {:#x} pitch {:#x} info {:#x}, clear color {:#x} depth {:#x}",
            register(RB_COPY_CONTROL),
            register(RB_COPY_DEST_BASE),
            register(RB_COPY_DEST_PITCH),
            register(RB_COPY_DEST_INFO),
            register(RB_COLOR_CLEAR),
            register(RB_DEPTH_CLEAR),
        );
    }
    text
}

/// The texture fetch constants a shader samples, for draw tracing.
fn describe_textures(memory: &GuestMemory, shader: &Shader) -> String {
    let constants: std::collections::BTreeSet<u32> = Program::decode(&shader.words)
        .instructions
        .iter()
        .filter_map(|(_, instruction)| match instruction {
            Instruction::TextureFetch(fetch) => Some(fetch.constant()),
            _ => None,
        })
        .collect();
    constants
        .into_iter()
        .map(|constant| {
            let first = FETCH_CONSTANTS + constant * FETCH_CONSTANT_WORDS;
            let words = std::array::from_fn(|word| read_register(memory, first + word as u32));
            format!("tf{constant} {:x?}", TextureDescriptor::decode(words))
        })
        .collect::<Vec<_>>()
        .join(", ")
}
