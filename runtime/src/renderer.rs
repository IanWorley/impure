//! Runs the title's draws on the host GPU with wgpu, on Bevy's device.
//!
//! The console renders into EDRAM, then "resolves" (copies) a rectangle of it to a texture in
//! memory, and finally displays a resolved front buffer. Here each EDRAM render target is a host
//! texture, a resolve copies into a host texture named by its destination address, and a swap
//! copies the named front buffer into the window's display texture. Textures sampled from a
//! resolve destination use the resolved host texture. Nothing is written back to guest memory,
//! so a title reading its resolved textures with the CPU would not see them.
//!
//! Translated shaders read guest data themselves (see `translate`): constants from one storage
//! buffer, vertices and indices from a window of guest memory uploaded per draw, and textures
//! uploaded from guest memory as they are sampled. Anything not reproduced is reported once.
//!
//! Draws and resolves are recorded into one command buffer, consecutive draws to the same
//! surfaces sharing a render pass, and submitted together at a swap or when the batch's buffers
//! are full: a submission and a pass per draw cost more than most draws.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Mutex;

use bevy::log::{debug, info, warn};
use wgpu::util::DeviceExt;

use crate::command_processor::{read_register, read_registers};
use crate::edram::{self, Rectangle, Surface};
use crate::gpu::{
    Draw, EDRAM_MODE_COPY, EDRAM_MODE_MASK, FETCH_CONSTANT_WORDS, FETCH_CONSTANTS, IndexBuffer,
    ShaderStage, TextureDescriptor, VIEWPORT_REGISTERS, registers,
};
use crate::memory::GuestMemory;
use crate::physical_memory;
use crate::shader::{Instruction, Program};
use crate::submitter::{Readback, Recycled, Submitter, Uploads, Work};
use crate::texture::{self, HostFormat};
use crate::translate::{self, TextureDimension, VertexEntry};

/// The renderer, once Bevy's device is available.
static RENDERER: Mutex<Option<Renderer>> = Mutex::new(None);

/// Starts rendering on the device Bevy created; `software` says whether it is a CPU
/// rasterizer.
pub fn install(device: wgpu::Device, queue: wgpu::Queue, software: bool) {
    *RENDERER.lock().unwrap() = Some(Renderer::new(device, queue, software));
}

/// The texture the window shows; swaps copy the front buffer into it.
static DISPLAY: Mutex<Option<wgpu::Texture>> = Mutex::new(None);

pub fn set_display(texture: wgpu::Texture) {
    *DISPLAY.lock().unwrap() = Some(texture);
}

/// Where to save the next presented frames, as input scripts ask.
static CAPTURES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Saves the next frame the title presents, as the window shows it, to a PNG file at `path`.
/// It reads the display texture itself, so it needs no window system support and works while
/// the window is hidden.
pub fn capture_next_frame(path: PathBuf) {
    CAPTURES.lock().unwrap().push(path);
}

/// Where to save the resolves of the next frame the title starts, as input scripts ask.
static RESOLVE_DUMPS: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Saves every resolve of the next whole frame, in order, as `<prefix>-<n>-<address>.png`: the
/// passes a frame's picture is built from, for finding which one goes wrong.
pub fn dump_next_frame_resolves(prefix: PathBuf) {
    *RESOLVE_DUMPS.lock().unwrap() = Some(prefix);
}

/// A frame whose resolves are being saved.
struct ResolveDump {
    prefix: PathBuf,
    readbacks: Vec<Readback>,
}

/// Runs `action` on the renderer, if rendering has started.
pub fn with(action: impl FnOnce(&mut Renderer)) {
    if let Some(renderer) = RENDERER.lock().unwrap().as_mut() {
        action(renderer);
    }
}

/// Maps a physical address to the guest address the title uses for it.
pub type View<'a> = &'a dyn Fn(u32) -> Option<u32>;

/// Register blocks the shaders read, copied into the constants buffer in this order.
const FLOAT_CONSTANTS: (u32, usize) = (0x4000, 512 * 4);
const FETCH: (u32, usize) = (FETCH_CONSTANTS, 192);
const BOOLS: (u32, usize) = (0x4900, 8);
const LOOPS: (u32, usize) = (0x4908, 32);
const TEXTURE_CONSTANTS: usize = 32;
/// The per-draw words after the register blocks (`index_offset` to `position_offset_z`).
const DRAW_WORDS: usize = 11;
/// The alpha test's words after the texture swizzles (`alpha_function` to `sample_shift`).
const ALPHA_TEST_WORDS: usize = 4;
/// Then the register the pixel parameters go to (`pixel_parameters`).
const PIXEL_PARAMETERS_WORDS: usize = 1;
const CONSTANTS_WORDS: usize = FLOAT_CONSTANTS.1
    + FETCH.1
    + BOOLS.1
    + LOOPS.1
    + DRAW_WORDS
    + TEXTURE_CONSTANTS
    + ALPHA_TEST_WORDS
    + PIXEL_PARAMETERS_WORDS;
/// WGSL rounds the struct's size up to its 16-byte alignment.
const CONSTANTS_SIZE: u64 = (CONSTANTS_WORDS * 4).next_multiple_of(16) as u64;
const WORD_SIZE: u32 = 4;
/// A cube texture's faces, which it stores as array layers.
const CUBE_FACES: u32 = 6;
/// xenos::TextureFormat k_8_8_8_8.
const TEXTURE_FORMAT_8_8_8_8: u32 = 6;
/// How long a swap waits for the previous frame's GPU work before going on regardless.
const FRAME_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// How many draws' constants, and how many indices, one batch holds.
const BATCH_DRAWS: u64 = 4096;
const BATCH_INDICES: u64 = 8 << 20;
/// How much guest memory one batch holds: the largest storage binding every device allows, as
/// the shaders bind it whole. Draws reading more are skipped.
const BATCH_MEMORY_SIZE: u64 = 128 << 20;
/// How many texture views to keep before starting over, so views of textures since replaced
/// do not keep them alive for ever.
const MAXIMUM_CACHED_VIEWS: usize = 4096;
/// Texture bind groups kept for reuse; each holds its textures alive, so the cache is bounded.
const MAXIMUM_CACHED_TEXTURE_GROUPS: usize = 4096;
/// Render target and copy register fields.
const COLOR_BASE_MASK: u32 = 0xFFF;
const COLOR_FORMAT_SHIFT: u32 = 16;
const COLOR_FORMAT_MASK: u32 = 0xF;
/// xenos::ColorRenderTargetFormat values stored as 8-bit RGBA.
const COLOR_FORMAT_8_8_8_8: u32 = 0;
const COLOR_FORMAT_8_8_8_8_GAMMA: u32 = 1;
const COLOR_FORMAT_32_FLOAT: u32 = 14;
const COPY_SOURCE_MASK: u32 = 0x7;
/// RB_COPY_CONTROL's source for depth; 0-3 are the colour targets.
const COPY_SOURCE_DEPTH: u32 = 4;
const COPY_COLOR_CLEAR: u32 = 1 << 8;
const COPY_DEPTH_CLEAR: u32 = 1 << 9;
const STENCIL_MASK: u32 = 0xFF;
/// RB_STENCILREFMASK: reference, read mask and write mask, a byte each.
const STENCIL_READ_MASK_SHIFT: u32 = 8;
const STENCIL_WRITE_MASK_SHIFT: u32 = 16;
/// PA_SU_SC_MODE_CNTL: strips restart at VGT_MULTI_PRIM_IB_RESET_INDX's index.
const MULTI_PRIM_IB_ENABLE: u32 = 1 << 21;
/// The index that restarts a strip on the host, as wgpu takes it with 32-bit indices.
const HOST_RESTART_INDEX: u32 = u32::MAX;
/// PA_SU_SC_MODE_CNTL: culling of front and back faces, and clockwise (rather than
/// counter-clockwise) triangles being the front.
const CULL_FRONT: u32 = 1 << 0;
const CULL_BACK: u32 = 1 << 1;
const FRONT_FACE_CLOCKWISE: u32 = 1 << 2;
/// PA_SU_SC_MODE_CNTL: positions are moved by the window offset.
const VTX_WINDOW_OFFSET_ENABLE: u32 = 1 << 16;
/// The resolve source registers for colour targets 0-3.
const COLOR_INFO_REGISTERS: [u32; 4] = [
    registers::RB_COLOR_INFO,
    registers::RB_COLOR1_INFO,
    registers::RB_COLOR2_INFO,
    registers::RB_COLOR3_INFO,
];
const COPY_DEST_PITCH_MASK: u32 = 0x3FFF;
const COPY_DEST_HEIGHT_SHIFT: u32 = 16;
const COPY_DEST_FORMAT_SHIFT: u32 = 7;
const COPY_DEST_FORMAT_MASK: u32 = 0x3F;
/// RB_COPY_DEST_INFO: the resolve writes red and blue exchanged, as A8R8G8B8 textures expect.
const COPY_DEST_SWAP_RED_BLUE: u32 = 1 << 24;
/// Every resolve destination format handled (8_8_8_8, 32_FLOAT) is 32 bits a texel in memory.
const RESOLVE_DESTINATION_BYTES_PER_TEXEL: u32 = 4;
/// xenos::ColorFormat k_8_8_8_8, the only resolve destination format handled.
const COPY_FORMAT_8_8_8_8: u32 = 6;
const COPY_FORMAT_32_FLOAT: u32 = 36;
/// PA_SU_VTX_CNTL: pixel centers at integer coordinates (Direct3D 9) rather than half-way.
const PIXEL_CENTER_HALF: u32 = 1;
/// PA_CL_VTE_CNTL: enables for the viewport's X, Y and Z scale and offset, then the formats.
const VTE_X_SCALE: u32 = 1 << 0;
const VTE_X_OFFSET: u32 = 1 << 1;
const VTE_Y_SCALE: u32 = 1 << 2;
const VTE_Y_OFFSET: u32 = 1 << 3;
const VTE_Z_SCALE: u32 = 1 << 4;
const VTE_Z_OFFSET: u32 = 1 << 5;
const VTE_FORMAT_SHIFT: u32 = 8;
const VTE_FORMAT_MASK: u32 = 0x7;
/// RB_BLENDCONTROL fields: colour source factor, operation, destination factor; then the same
/// for alpha from bit 16.
const BLEND_ALPHA_SHIFT: u32 = 16;
const BLEND_FACTOR_MASK: u32 = 0x1F;
const BLEND_OPERATION_SHIFT: u32 = 5;
const BLEND_OPERATION_MASK: u32 = 0x7;
const BLEND_DESTINATION_SHIFT: u32 = 8;
/// Blending that replaces the target: one, add, zero.
const BLEND_REPLACE: u32 = 0x0001_0001;
const COLOR_MASK_TARGET_0: u32 = 0xF;
/// Vertex fetch constants: the dword address above two type bits, then the size in words.
const VERTEX_ADDRESS_MASK: u32 = !0x3;
const VERTEX_SIZE_SHIFT: u32 = 2;
const VERTEX_SIZE_MASK: u32 = 0xFF_FFFF;

/// Xenos primitive types (xenos::PrimitiveType).
const POINT_LIST: u32 = 0x01;
const LINE_LIST: u32 = 0x02;
const LINE_STRIP: u32 = 0x03;
const TRIANGLE_LIST: u32 = 0x04;
const TRIANGLE_FAN: u32 = 0x05;
const TRIANGLE_STRIP: u32 = 0x06;
const RECTANGLE_LIST: u32 = 0x08;
const QUAD_LIST: u32 = 0x0D;

/// A guest texture by everything that decides its host contents' layout.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TextureKey {
    base: u32,
    /// 1, or 6 for a cube.
    faces: u32,
    format: u32,
    width: u32,
    height: u32,
    pitch: u32,
    tiled: bool,
    endian: u32,
}

impl TextureKey {
    fn new(descriptor: &TextureDescriptor, faces: u32) -> Self {
        Self {
            base: descriptor.base,
            faces,
            format: descriptor.format,
            width: descriptor.width,
            height: descriptor.height,
            pitch: descriptor.pitch,
            tiled: descriptor.tiled,
            endian: descriptor.endian,
        }
    }
}

struct CachedTexture {
    texture: wgpu::Texture,
    /// The guest bytes it was decoded from. Comparing them is several times faster than hashing
    /// them, which matters at thousands of draws a frame.
    guest: Vec<u8>,
    /// The frame it was last compared with guest memory.
    checked_frame: u64,
}

/// A texture bind group's layout, and each texture constant's texture and sampler.
type TextureGroupKey = (
    wgpu::BindGroupLayout,
    Vec<(u32, wgpu::Texture, wgpu::Sampler)>,
);

/// A translated shader, ready for pipelines.
struct CompiledShader {
    module: wgpu::ShaderModule,
    textures: BTreeMap<u32, TextureDimension>,
    vertex_fetch_constants: BTreeSet<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipelineKey {
    vertex_shader: u64,
    pixel_shader: u64,
    entry: VertexEntry,
    topology: wgpu::PrimitiveTopology,
    blend_control: u32,
    color_format: wgpu::TextureFormat,
    color_mask: u32,
    /// Which faces are culled, if any, and which winding faces the front.
    cull: Option<(wgpu::Face, wgpu::FrontFace)>,
    /// Depth and stencil testing, if the draw does any.
    depth: Option<DepthState>,
}

/// RB_DEPTHCONTROL and the stencil masks of RB_STENCILREFMASK (its reference is set per draw).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DepthState {
    control: u32,
    stencil_masks: u32,
}

/// The surfaces a render pass draws to: a colour surface and, if the draws test depth, a depth
/// surface.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PassTargets {
    color: (RenderTargetKey, wgpu::TextureFormat),
    depth: Option<RenderTargetKey>,
}

/// What draws have set in the open render pass, so each draw sets only what changed: wgpu
/// validates and encodes every call, which at thousands of draws a frame adds up.
#[derive(Default)]
struct PassState {
    pipeline: Option<wgpu::RenderPipeline>,
    texture_group: Option<wgpu::BindGroup>,
    scissor: Option<[u32; 4]>,
    stencil_reference: Option<u32>,
    blend_constant: Option<[u32; 4]>,
    index_buffer_set: bool,
}

/// Draws recorded since the last submission.
struct Batch {
    encoder: wgpu::CommandEncoder,
    /// The pass the latest draws share, with the surfaces it was begun on.
    pass: Option<(PassTargets, wgpu::RenderPass<'static>, PassState)>,
    /// Each draw's constants, at the offset its bind group uses, and the guest memory the
    /// draws read; uploaded when the batch is submitted.
    constants: Vec<u8>,
    memory: BatchMemory,
}

/// Guest memory the batched draws read: each range once, however many draws read it or ranges
/// inside it. Titles often draw from a vertex buffer through fetch constants that start further
/// into it each draw and run to its end, so sharing only identical ranges would upload the rest
/// of the buffer again for every draw.
#[derive(Default)]
struct BatchMemory {
    bytes: Vec<u8>,
    /// Ranges placed since the title last submitted commands, by first word: their length in
    /// whole words and where they start in `bytes`.
    placed: BTreeMap<u32, (u32, u32)>,
    /// The longest range in `placed`, which bounds how far back a range holding another starts.
    longest_placed: u32,
    /// Indexed draws' guest indices, in host order, for the host's index buffer.
    indices: Vec<u32>,
}

impl BatchMemory {
    /// Places the guest memory of each (physical address, length) range not placed yet, as raw
    /// bytes in guest order (which the shaders byte-swap as the GPU does), and returns the byte
    /// offset of each range's start; None, placing nothing, if they would not fit in `capacity`.
    fn place(
        &mut self,
        memory: &GuestMemory,
        view: View,
        ranges: &[(u32, u32)],
        capacity: usize,
    ) -> Option<Vec<u32>> {
        // Ranges start on words, as the shaders read whole words.
        let words = |&(start, length): &(u32, u32)| {
            let first = start & !(WORD_SIZE - 1);
            (first, (start + length - first).next_multiple_of(WORD_SIZE))
        };
        let added: usize = ranges
            .iter()
            .map(words)
            .filter(|&(first, length)| self.find(first, length).is_none())
            .map(|(_, length)| length as usize)
            .sum();
        if self.bytes.len() + added > capacity {
            return None;
        }
        let offsets = ranges
            .iter()
            .map(|range| {
                let (first, length) = words(range);
                let start = self.find(first, length).unwrap_or_else(|| {
                    let start = self.bytes.len();
                    self.bytes.resize(start + length as usize, 0);
                    if let Some(address) = view(first) {
                        memory.read_bytes(address, &mut self.bytes[start..]);
                    }
                    self.placed.insert(first, (length, start as u32));
                    self.longest_placed = self.longest_placed.max(length);
                    start as u32
                });
                start + range.0 - first
            })
            .collect();
        Some(offsets)
    }

    /// Where `first` is in `bytes`, if a placed range holds all of `first..first + length`.
    fn find(&self, first: u32, length: u32) -> Option<u32> {
        let end = u64::from(first) + u64::from(length);
        self.placed
            .range(first.saturating_sub(self.longest_placed)..=first)
            .rev()
            .find(|&(&base, &(placed_length, _))| u64::from(base) + u64::from(placed_length) >= end)
            .map(|(&base, &(_, start))| start + (first - base))
    }

    /// Forgets what was placed, keeping the bytes for the draws already batched, so later draws
    /// read guest memory afresh.
    fn forget_placed(&mut self) {
        self.placed.clear();
        self.longest_placed = 0;
    }
}

impl Batch {
    /// A batch filling the allocations a submitted batch has given back, if any.
    fn new(device: &wgpu::Device, recycled: Option<Recycled>) -> Self {
        let (constants, bytes, indices) = recycled.unwrap_or_default();
        Self {
            encoder: device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default()),
            pass: None,
            constants,
            memory: BatchMemory {
                bytes,
                indices,
                ..BatchMemory::default()
            },
        }
    }
}

/// An EDRAM surface: where it starts in EDRAM (in tiles) and its width in samples. Colour
/// surfaces are also told apart by their host format, as one EDRAM range can hold either.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RenderTargetKey {
    base: u32,
    width: u32,
}

pub struct Renderer {
    device: wgpu::Device,
    /// Uploads, submissions and presentation, in order, on their own thread.
    submitter: Submitter,
    /// Every batched draw's constants, each at a multiple of `constants_stride`.
    constants: wgpu::Buffer,
    constants_stride: u64,
    /// Every batched draw's window of guest memory.
    memory: wgpu::Buffer,
    /// Every batched indexed draw's indices.
    indices: wgpu::Buffer,
    batch: Option<Batch>,
    /// Views of whole textures (one aspect of depth textures), kept because making and
    /// destroying them waits for the GPU on some drivers.
    views: HashMap<(wgpu::Texture, wgpu::TextureAspect), wgpu::TextureView>,
    /// The window's texture viewed as plain (not sRGB) bytes.
    display_view: Option<(wgpu::Texture, wgpu::TextureView)>,
    constants_layout: wgpu::BindGroupLayout,
    /// The constants and guest memory buffers, for every draw.
    constants_group: wgpu::BindGroup,
    /// Bind groups for the sets of textures and samplers draws have used.
    texture_groups: HashMap<TextureGroupKey, wgpu::BindGroup>,
    /// None for shaders that did not translate to valid WGSL.
    shaders: HashMap<(ShaderStage, u64), Option<CompiledShader>>,
    /// Pipelines and the texture bind group layout they were made with; None if creation failed.
    pipelines: HashMap<PipelineKey, Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout)>>,
    render_targets: HashMap<(RenderTargetKey, wgpu::TextureFormat), wgpu::Texture>,
    depth_targets: HashMap<RenderTargetKey, wgpu::Texture>,
    /// Resolved textures, by their physical destination address.
    resolved: HashMap<u32, wgpu::Texture>,
    /// Copies of the top-left parts of resolved textures that draws sample, by address, size
    /// and format.
    resolved_parts: HashMap<(u32, u32, u32, wgpu::TextureFormat), wgpu::Texture>,
    textures: HashMap<TextureKey, CachedTexture>,
    /// Opaque white textures, by their number of faces.
    placeholders: HashMap<u32, wgpu::Texture>,
    /// Resolved textures holding depth, and whether it was 20e4 floats.
    depth_resolves: HashMap<wgpu::Texture, bool>,
    /// Each depth resolve as the console's memory holds it, for draws reading it as 8888.
    packed_depths: HashMap<wgpu::Texture, wgpu::Texture>,
    /// The passes moving depth between its host form and EDRAM's, by kind and float format.
    depth_aliasing: HashMap<(DepthAliasing, bool), wgpu::RenderPipeline>,
    /// EDRAM ranges a depth surface uses where colour has been drawn since, which the next
    /// draw testing depth there reads as depth.
    color_over_depth: HashSet<RenderTargetKey>,
    /// EDRAM ranges a colour surface uses where depth has been written since (and whether as
    /// 20e4 floats), which the next colour draw or resolve there reads as colour.
    depth_over_color: HashMap<RenderTargetKey, bool>,
    samplers: HashMap<(u32, u32, u32, u32), wgpu::Sampler>,
    /// What has been reported as not reproduced, so each is reported once.
    reported: BTreeSet<String>,
    presented: u64,
    /// The resolves being saved this frame, if an input script asked.
    resolve_dump: Option<ResolveDump>,
    /// Copies a front buffer to the display, created on first use: as stored, and with red
    /// and blue exchanged back.
    blits: HashMap<bool, wgpu::RenderPipeline>,
    /// Resolved textures whose latest resolve exchanged red and blue, as the console's memory
    /// then holds them and its display reads them.
    swapped: HashSet<wgpu::Texture>,
    /// Average EDRAM samples into a resolve destination, by destination format.
    resolves: HashMap<wgpu::TextureFormat, ResolvePipeline>,
    resolve_depth: Option<ResolvePipeline>,
    /// Whether the device is a CPU rasterizer, where 2x surfaces are drawn at one sample per
    /// pixel (see `Surface::without_vertical_samples`).
    software: bool,
    /// A resolve's parameters (its samples per pixel, how far down the destination it writes,
    /// and whether it swaps red and blue), one buffer for each.
    resolve_parameters: HashMap<(u32, u32, u32, u32), wgpu::Buffer>,
}

impl Renderer {
    fn new(device: wgpu::Device, queue: wgpu::Queue, software: bool) -> Self {
        let storage = |binding, has_dynamic_offset| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset,
                min_binding_size: None,
            },
            count: None,
        };
        let constants_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("guest constants and memory"),
            // Each draw's constants are at their own offset in one buffer, given when the draw
            // binds the group, so one bind group serves every draw.
            entries: &[storage(0, true), storage(1, false)],
        });
        let constants_stride = CONSTANTS_SIZE.next_multiple_of(storage_offset_alignment(&device));
        let constants = storage_buffer(&device, constants_stride * BATCH_DRAWS);
        let memory = storage_buffer(&device, BATCH_MEMORY_SIZE);
        let constants_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("guest constants and memory"),
            layout: &constants_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &constants,
                        offset: 0,
                        size: NonZeroU64::new(CONSTANTS_SIZE),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: memory.as_entire_binding(),
                },
            ],
        });
        let indices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("batched indices"),
            size: BATCH_INDICES * u64::from(WORD_SIZE),
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        info!(target: "renderer", "rendering guest draws with wgpu");
        Self {
            submitter: Submitter::start(device.clone(), queue, FRAME_WAIT_LIMIT),
            device,
            constants,
            constants_stride,
            memory,
            indices,
            batch: None,
            views: HashMap::new(),
            display_view: None,
            constants_layout,
            constants_group,
            texture_groups: HashMap::new(),
            shaders: HashMap::new(),
            pipelines: HashMap::new(),
            render_targets: HashMap::new(),
            depth_targets: HashMap::new(),
            resolved: HashMap::new(),
            resolved_parts: HashMap::new(),
            textures: HashMap::new(),
            placeholders: HashMap::new(),
            depth_resolves: HashMap::new(),
            packed_depths: HashMap::new(),
            depth_aliasing: HashMap::new(),
            color_over_depth: HashSet::new(),
            depth_over_color: HashMap::new(),
            samplers: HashMap::new(),
            reported: BTreeSet::new(),
            presented: 0,
            resolve_dump: None,
            blits: HashMap::new(),
            swapped: HashSet::new(),
            resolves: HashMap::new(),
            resolve_depth: None,
            resolve_parameters: HashMap::new(),
            software,
        }
    }

    fn report(&mut self, message: String) {
        if self.reported.insert(message.clone()) {
            warn!(target: "renderer", "not reproduced: {message}");
        }
    }

    /// Executes a draw packet: renders it, or resolves EDRAM if the title set copy mode.
    pub fn draw(&mut self, memory: &GuestMemory, view: View, draw: &Draw) {
        match read_register(memory, registers::RB_MODECONTROL) & EDRAM_MODE_MASK {
            EDRAM_MODE_COPY => self.checked("resolve", |renderer| renderer.resolve(memory, view)),
            EDRAM_MODE_COLOR_DEPTH => self.checked("draw", |renderer| {
                renderer.render(memory, view, draw, false)
            }),
            EDRAM_MODE_DEPTH_ONLY => {
                self.checked("draw", |renderer| renderer.render(memory, view, draw, true))
            }
            _ => {}
        }
    }

    /// Copies the front buffer the title presents into the window's texture.
    pub fn swap(&mut self, front_buffer: u32) {
        self.checked("swap", |renderer| renderer.present(front_buffer));
    }

    /// Runs `action`, reporting GPU validation errors it causes instead of letting them reach
    /// Bevy, which would close the app.
    fn checked(&mut self, what: &str, action: impl FnOnce(&mut Self)) {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        action(self);
        if let Some(error) = bevy::tasks::block_on(scope.pop()) {
            self.report(format!("{what} failed: {error}"));
        }
    }

    /// The title submitted more commands. It wrote what their draws read before submitting
    /// them, but may since have written over what earlier draws read, so later draws read guest
    /// memory afresh rather than sharing what earlier ones placed.
    pub fn commands_submitted(&mut self) {
        if let Some(batch) = &mut self.batch {
            batch.memory.forget_placed();
        }
    }

    fn present(&mut self, front_buffer: u32) {
        self.submit();
        let display = DISPLAY.lock().unwrap().clone();
        let (Some(display), Some(source)) = (display, self.resolved.get(&front_buffer).cloned())
        else {
            return;
        };
        let (width, height) = (
            source.width().min(display.width()),
            source.height().min(display.height()),
        );
        let source_view = self.view(&source, wgpu::TextureAspect::All);
        // Rendered through a plain view: the bytes are already gamma-encoded.
        let display_view = match &self.display_view {
            Some((texture, view)) if *texture == display => view.clone(),
            _ => {
                let view = display.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(wgpu::TextureFormat::Rgba8Unorm),
                    ..Default::default()
                });
                self.display_view = Some((display.clone(), view.clone()));
                view
            }
        };
        let swapped = self.swapped.contains(&source);
        let device = &self.device;
        let blit = self
            .blits
            .entry(swapped)
            .or_insert_with(|| create_blit(device, swapped));
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &blit.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source_view),
            }],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &display_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(blit);
            pass.set_bind_group(0, &group, &[]);
            pass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
            pass.draw(0..FULL_SCREEN_TRIANGLE, 0..1);
        }
        // Keep at most one frame in flight: without waiting, a slow GPU falls ever further
        // behind the title, and the window's own frames queue up behind the backlog. The
        // title records the next frame while the GPU finishes this one.
        self.presented += 1;
        let readbacks = self
            .resolve_dump
            .take()
            .map_or_else(Vec::new, |dump| dump.readbacks);
        self.submitter.send(Work::Present {
            encoder,
            frame: self.presented,
            display,
            captures: std::mem::take(&mut *CAPTURES.lock().unwrap()),
            readbacks,
        });
        // A dump asked for during this frame covers the next one from its start.
        self.resolve_dump = RESOLVE_DUMPS
            .lock()
            .unwrap()
            .take()
            .map(|prefix| ResolveDump {
                prefix,
                readbacks: Vec::new(),
            });
        self.submitter
            .wait_for_frame(self.presented - 1, FRAME_WAIT_LIMIT);
        if self.presented == 1 {
            info!(target: "renderer", "first guest frame presented from physical {front_buffer:#010x}");
        }
    }

    /// Renders a draw into its EDRAM surfaces; `depth_only` draws write no colour.
    fn render(&mut self, memory: &GuestMemory, view: View, draw: &Draw, depth_only: bool) {
        let register = |index| read_register(memory, index);
        let (Some(vertex_shader), Some(pixel_shader)) = (&draw.vertex_shader, &draw.pixel_shader)
        else {
            return;
        };
        let surface = self.surface(register(registers::RB_SURFACE_INFO));
        let color_info = register(registers::RB_COLOR_INFO);
        let format = (color_info >> COLOR_FORMAT_SHIFT) & COLOR_FORMAT_MASK;
        // Depth-only draws write no colour, so any host format stands in.
        let color_format = match color_target_format(format) {
            Some(color_format) => color_format,
            None if depth_only => wgpu::TextureFormat::Rgba8Unorm,
            None => return self.report(format!("render target format {format}")),
        };
        let Some((entry, topology, host_vertices)) =
            host_primitive(draw.initiator.primitive, draw.initiator.index_count)
        else {
            return self.report(format!("primitive type {:#x}", draw.initiator.primitive));
        };
        // Indexed lists and strips use the host's index buffer, so the vertices their triangles
        // share are shaded once.
        let entry = if entry == VertexEntry::Direct && draw.indices.is_some() {
            VertexEntry::Indexed
        } else {
            entry
        };
        let cull = match culling(register(registers::PA_SU_SC_MODE_CNTL), entry, topology) {
            Culling::Nothing => None,
            Culling::Faces(face, front_face) => Some((face, front_face)),
            Culling::Everything => return,
        };
        if surface.pitch == 0 || host_vertices == 0 {
            return;
        }
        let size = surface.host_size();
        let window_offset = edram::window_offset(register(registers::PA_SC_WINDOW_OFFSET));
        let scissor = surface
            .samples(edram::scissor(
                register(registers::PA_SC_WINDOW_SCISSOR_TL),
                register(registers::PA_SC_WINDOW_SCISSOR_BR),
                window_offset,
            ))
            .clamp(size.0, size.1);
        if scissor.is_empty() {
            return;
        }
        let target = self.render_target(
            RenderTargetKey {
                base: color_info & COLOR_BASE_MASK,
                width: size.0,
            },
            color_format,
            size,
        );
        let depth_control = register(registers::RB_DEPTHCONTROL);
        let stencil = register(registers::RB_STENCILREFMASK);
        let depth = (depth_control & DEPTH_ENABLES != 0).then_some(DepthState {
            control: depth_control,
            stencil_masks: stencil & !STENCIL_MASK,
        });
        let depth_target = depth.map(|_| {
            self.depth_target(
                &RenderTargetKey {
                    base: register(registers::RB_DEPTH_INFO) & COLOR_BASE_MASK,
                    width: size.0,
                },
                size,
            )
        });
        let vertex = self.shader(
            ShaderStage::Vertex,
            vertex_shader.hash,
            &vertex_shader.words,
        );
        let pixel = self.shader(ShaderStage::Pixel, pixel_shader.hash, &pixel_shader.words);
        let (Some(vertex), Some(pixel)) = (vertex, pixel) else {
            return;
        };
        let pipeline_key = PipelineKey {
            vertex_shader: vertex_shader.hash,
            pixel_shader: pixel_shader.hash,
            entry,
            topology,
            blend_control: register(registers::RB_BLENDCONTROL0),
            color_format,
            color_mask: if depth_only {
                0
            } else {
                register(registers::RB_COLOR_MASK) & COLOR_MASK_TARGET_0
            },
            cull,
            depth,
        };
        let mut textures = self.shaders[&(ShaderStage::Vertex, vertex)]
            .as_ref()
            .unwrap()
            .textures
            .clone();
        for (&constant, &dimension) in &self.shaders[&(ShaderStage::Pixel, pixel)]
            .as_ref()
            .unwrap()
            .textures
        {
            textures.entry(constant).or_insert(dimension);
        }
        let writes_color = pipeline_key.color_mask != 0;
        let Some((pipeline, texture_layout)) = self.pipeline(pipeline_key, &textures) else {
            return;
        };

        // Guest memory the shaders read: vertex buffers and the index buffer.
        let fetch_constants = self.shaders[&(ShaderStage::Vertex, vertex)]
            .as_ref()
            .unwrap()
            .vertex_fetch_constants
            .clone();
        let mut ranges: Vec<(u32, u32)> = fetch_constants
            .iter()
            .map(|&constant| {
                let first = FETCH_CONSTANTS + constant * 2;
                let address = register(first) & VERTEX_ADDRESS_MASK;
                let words = (register(first + 1) >> VERTEX_SIZE_SHIFT) & VERTEX_SIZE_MASK;
                (address, words * WORD_SIZE)
            })
            .collect();
        let shader_indices = draw.indices.filter(|_| entry != VertexEntry::Indexed);
        if let Some(indices) = shader_indices {
            let physical = physical_memory::physical_address(indices.address);
            ranges.push((physical, indices.words * WORD_SIZE));
        }
        let host_index_count = if entry == VertexEntry::Indexed {
            host_vertices as usize
        } else {
            0
        };
        let Some(mut offsets) = self.place_memory(memory, view, &ranges, host_index_count) else {
            return self.report("draws reading more guest memory than a batch holds".into());
        };
        let indices = shader_indices.map(|indices| (offsets.pop().unwrap(), indices));
        let first_index = draw
            .indices
            .filter(|_| entry == VertexEntry::Indexed)
            .map(|indices| {
                // Strips the title cuts with a reset index restart there on the host too;
                // drawn through, each cut would join two strips with triangles through
                // whatever vertex the reset index names.
                let restart = (topology.is_strip()
                    && register(registers::PA_SU_SC_MODE_CNTL) & MULTI_PRIM_IB_ENABLE != 0)
                    .then(|| register(registers::VGT_MULTI_PRIM_IB_RESET_INDX));
                self.push_indices(
                    memory,
                    indices,
                    draw.initiator.indices_are_32_bit,
                    host_index_count,
                    restart,
                )
            });
        let vertex_offsets: Vec<(u32, u32)> =
            fetch_constants.iter().copied().zip(offsets).collect();

        let viewport: [f32; VIEWPORT_REGISTERS as usize] = std::array::from_fn(|offset| {
            f32::from_bits(register(registers::PA_CL_VPORT_XSCALE + offset as u32))
        });
        let half_pixel = register(registers::PA_SU_VTX_CNTL) & PIXEL_CENTER_HALF == 0;
        let vertex_window_offset =
            if register(registers::PA_SU_SC_MODE_CNTL) & VTX_WINDOW_OFFSET_ENABLE != 0 {
                window_offset
            } else {
                (0, 0)
            };
        // Host normalized coordinates span the whole surface; in pixels, that is its size in
        // samples divided by the samples per pixel.
        let (sample_x, sample_y) = surface.sample_shift;
        let (scale, offset) = position_transform(
            register(registers::PA_CL_VTE_CNTL),
            viewport,
            [(size.0 >> sample_x) as f32, (size.1 >> sample_y) as f32],
            half_pixel,
            vertex_window_offset,
        );
        let draw_words = DrawWords {
            vertex_offsets,
            indices,
            index_bias: register(registers::VGT_INDX_OFFSET),
            position_format: position_format(
                register(registers::PA_CL_VTE_CNTL),
                register(registers::PA_CL_CLIP_CNTL),
            ),
            scale,
            offset,
            indices_are_32_bit: draw.initiator.indices_are_32_bit,
            alpha_test: alpha_test_words(
                register(registers::RB_COLORCONTROL),
                register(registers::RB_ALPHA_REF),
                surface.sample_shift,
            ),
            pixel_parameters: pixel_parameters_register(
                register(registers::SQ_PROGRAM_CNTL),
                register(registers::SQ_CONTEXT_MISC),
            ),
        };
        if self.resolve_dump.is_some() {
            info!(target: "dump", "draw: shaders {:016x}/{:016x}, target {:#010x} format {format} {}x{}, blend {:#x}, colorcontrol {:#x} ref {}, depth {:#x}, mode {:#x}, primitive {:#x}, {} vertices",
                vertex_shader.hash, pixel_shader.hash, color_info & COLOR_BASE_MASK, size.0, size.1,
                register(registers::RB_BLENDCONTROL0), register(registers::RB_COLORCONTROL),
                f32::from_bits(register(registers::RB_ALPHA_REF)), register(registers::RB_DEPTHCONTROL),
                register(registers::PA_SU_SC_MODE_CNTL), draw.initiator.primitive, host_vertices);
            info!(target: "dump", "  program control {:#010x}, context misc {:#010x}, color mask {:#x}, vte {:#x}, scissor {scissor:?}, window offset {window_offset:?}, surface {:#010x}, depth info {:#010x}",
                register(registers::SQ_PROGRAM_CNTL), register(registers::SQ_CONTEXT_MISC),
                register(registers::RB_COLOR_MASK), register(registers::PA_CL_VTE_CNTL),
                register(registers::RB_SURFACE_INFO), register(registers::RB_DEPTH_INFO));
            // Full-screen passes are where post-processing goes wrong: their pixel constants.
            const FULL_SCREEN_VERTICES: u32 = 6;
            const CONSTANTS_PER_STAGE: u32 = 256;
            const COMPONENTS: u32 = 4;
            const PIXEL_CONSTANTS: u32 = FLOAT_CONSTANTS.0 + CONSTANTS_PER_STAGE * COMPONENTS;
            if host_vertices <= FULL_SCREEN_VERTICES {
                for index in 0..CONSTANTS_PER_STAGE {
                    let value: [f32; 4] = std::array::from_fn(|component| {
                        f32::from_bits(register(
                            PIXEL_CONSTANTS + index * COMPONENTS + component as u32,
                        ))
                    });
                    if value != [0.0; 4] {
                        info!(target: "dump", "  c{index}: {value:?}");
                    }
                }
            }
        }
        let mut swizzles = [0; TEXTURE_CONSTANTS];
        let mut texture_entries = Vec::new();
        for (&constant, &dimension) in &textures {
            let first = FETCH_CONSTANTS + constant * FETCH_CONSTANT_WORDS;
            let words = std::array::from_fn(|word| register(first + word as u32));
            let descriptor = TextureDescriptor::decode(words);
            if self.resolve_dump.is_some() {
                info!(target: "dump", "  texture {constant}: {descriptor:?}, words {words:08x?}");
            }
            let texture = self.upload_texture(memory, view, &descriptor, dimension);
            swizzles[constant as usize] =
                texture::host_swizzle(descriptor.swizzle, texture.format().components());
            let sampler = self.sampler(&descriptor);
            texture_entries.push((constant, texture, sampler));
        }
        let words = constants_words(memory, &draw_words, &swizzles);
        let constants_offset = self.push_constants(words_as_bytes(&words));
        let constants_offset =
            u32::try_from(constants_offset).expect("the constants buffer is far below 4 GiB");
        let constants_group = self.constants_group.clone();
        let texture_group = self.texture_group(texture_layout, texture_entries);

        let targets = PassTargets {
            color: (
                RenderTargetKey {
                    base: color_info & COLOR_BASE_MASK,
                    width: size.0,
                },
                color_format,
            ),
            depth: depth.map(|_| RenderTargetKey {
                base: register(registers::RB_DEPTH_INFO) & COLOR_BASE_MASK,
                width: size.0,
            }),
        };
        let index_buffer = self.indices.clone();
        let blend_constant: [u32; 4] =
            std::array::from_fn(|channel| register(registers::RB_BLEND_RED + channel as u32));
        let scissor = [
            scissor.left,
            scissor.top,
            scissor.right - scissor.left,
            scissor.bottom - scissor.top,
        ];
        let stencil_reference = stencil & STENCIL_MASK;
        let float_depth = edram::depth_is_float(register(registers::RB_DEPTH_INFO));
        if writes_color {
            self.transfer_depth_to_color(&targets.color.0);
        }
        if let (Some(key), Some(depth_texture)) = (&targets.depth, &depth_target) {
            self.transfer_color_to_depth(key, depth_texture, float_depth);
        }
        if writes_color && self.depth_targets.contains_key(&targets.color.0) {
            self.color_over_depth.insert(targets.color.0);
            self.depth_over_color.remove(&targets.color.0);
        }
        if let Some(key) = targets.depth
            && depth.is_some_and(|state| writes_depth_or_stencil(state.control))
        {
            self.mark_depth_over_color(key, float_depth);
        }
        let (pass, state) = self.pass(targets, &target, depth_target.as_ref());
        if state.pipeline.as_ref() != Some(&pipeline) {
            pass.set_pipeline(&pipeline);
            state.pipeline = Some(pipeline);
        }
        // The constants' offset changes with every draw.
        pass.set_bind_group(0, &constants_group, &[constants_offset]);
        if state.texture_group.as_ref() != Some(&texture_group) {
            pass.set_bind_group(1, &texture_group, &[]);
            state.texture_group = Some(texture_group);
        }
        if state.scissor != Some(scissor) {
            let [x, y, width, height] = scissor;
            pass.set_scissor_rect(x, y, width, height);
            state.scissor = Some(scissor);
        }
        if state.stencil_reference != Some(stencil_reference) {
            pass.set_stencil_reference(stencil_reference);
            state.stencil_reference = Some(stencil_reference);
        }
        if state.blend_constant != Some(blend_constant) {
            let [r, g, b, a] = blend_constant.map(|bits| f64::from(f32::from_bits(bits)));
            pass.set_blend_constant(wgpu::Color { r, g, b, a });
            state.blend_constant = Some(blend_constant);
        }
        match first_index {
            Some(first) => {
                if !state.index_buffer_set {
                    pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                    state.index_buffer_set = true;
                }
                pass.draw_indexed(first..first + host_vertices, 0, 0..1);
            }
            None => pass.draw(0..host_vertices, 0..1),
        }
    }

    /// The bind group for these textures and samplers, created the first time they are used
    /// together.
    fn texture_group(
        &mut self,
        layout: wgpu::BindGroupLayout,
        textures: Vec<(u32, wgpu::Texture, wgpu::Sampler)>,
    ) -> wgpu::BindGroup {
        let key = (layout, textures);
        if let Some(group) = self.texture_groups.get(&key) {
            return group.clone();
        }
        if self.texture_groups.len() >= MAXIMUM_CACHED_TEXTURE_GROUPS {
            self.texture_groups.clear();
        }
        let views: Vec<wgpu::TextureView> = key
            .1
            .iter()
            .map(|(_, texture, _)| self.view(texture, wgpu::TextureAspect::All))
            .collect();
        let entries: Vec<wgpu::BindGroupEntry> = key
            .1
            .iter()
            .zip(&views)
            .flat_map(|((constant, _, sampler), view)| {
                [
                    wgpu::BindGroupEntry {
                        binding: constant * 2,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: constant * 2 + 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ]
            })
            .collect();
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &key.0,
            entries: &entries,
        });
        self.texture_groups.insert(key, group.clone());
        group
    }

    /// Places the guest memory a draw reads in the batch (see `BatchMemory::place`), first
    /// submitting the batch if the draw's constants or memory would not fit in it.
    fn place_memory(
        &mut self,
        memory: &GuestMemory,
        view: View,
        ranges: &[(u32, u32)],
        index_count: usize,
    ) -> Option<Vec<u32>> {
        let capacity = self.memory.size() as usize;
        if self.batch.as_ref().is_some_and(|batch| {
            batch.constants.len() as u64 + self.constants_stride > self.constants.size()
                || (batch.memory.indices.len() + index_count) as u64 > BATCH_INDICES
        }) {
            self.submit();
        }
        if let Some(batch) = &mut self.batch
            && let Some(offsets) = batch.memory.place(memory, view, ranges, capacity)
        {
            return Some(offsets);
        }
        self.submit();
        let batch = Batch::new(&self.device, self.submitter.recycled());
        let batch = self.batch.insert(batch);
        batch.memory.place(memory, view, ranges, capacity)
    }

    /// Adds an indexed draw's indices to the batch `place_memory` made room in, in host order;
    /// returns where they start in the batch's index buffer.
    fn push_indices(
        &mut self,
        memory: &GuestMemory,
        indices: IndexBuffer,
        is_32_bit: bool,
        count: usize,
        restart: Option<u32>,
    ) -> u32 {
        let index_size = if is_32_bit {
            WORD_SIZE as usize
        } else {
            size_of::<u16>()
        };
        let first_word = indices.address & !(WORD_SIZE - 1);
        let start = (indices.address - first_word) as usize;
        let mut guest = vec![0; (start + count * index_size).next_multiple_of(WORD_SIZE as usize)];
        memory.read_bytes(first_word, &mut guest);
        let batch = self.batch.as_mut().expect("place_memory opens the batch");
        let first = batch.memory.indices.len() as u32;
        batch.memory.indices.extend(host_indices(
            &guest,
            start,
            count,
            is_32_bit,
            indices.endian,
            restart,
        ));
        first
    }

    /// Adds a draw's constants to the batch `place_memory` made room in; returns their offset.
    fn push_constants(&mut self, constants: &[u8]) -> u64 {
        let batch = self.batch.as_mut().expect("place_memory opens the batch");
        let offset = batch.constants.len();
        batch.constants.extend_from_slice(constants);
        batch
            .constants
            .resize(offset + self.constants_stride as usize, 0);
        offset as u64
    }

    /// The EDRAM surface RB_SURFACE_INFO describes, as this device renders it.
    fn surface(&self, surface_info: u32) -> Surface {
        let surface = Surface::decode(surface_info);
        if self.software {
            surface.without_vertical_samples()
        } else {
            surface
        }
    }

    /// The batch's render pass for `targets`, beginning one if the latest draws went elsewhere.
    fn pass(
        &mut self,
        targets: PassTargets,
        color: &wgpu::Texture,
        depth: Option<&wgpu::Texture>,
    ) -> (&mut wgpu::RenderPass<'static>, &mut PassState) {
        let reuse = self
            .batch
            .as_ref()
            .and_then(|batch| batch.pass.as_ref())
            .is_some_and(|(open, _, _)| *open == targets);
        if !reuse {
            let color_view = self.view(color, wgpu::TextureAspect::All);
            let depth_view = depth.map(|texture| self.view(texture, wgpu::TextureAspect::All));
            let batch = self.batch.as_mut().expect("place_memory opens the batch");
            // A pass borrows the encoder until it ends.
            batch.pass = None;
            // The same four slots as the pipeline's targets.
            let pass = batch
                .encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("guest draws"),
                    color_attachments: &[
                        Some(wgpu::RenderPassColorAttachment {
                            view: &color_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: LOAD_AND_STORE_COLOR,
                        }),
                        None,
                        None,
                        None,
                    ],
                    depth_stencil_attachment: depth_view.as_ref().map(|view| {
                        wgpu::RenderPassDepthStencilAttachment {
                            view,
                            depth_ops: Some(LOAD_AND_STORE),
                            stencil_ops: Some(LOAD_AND_STORE_STENCIL),
                        }
                    }),
                    ..Default::default()
                })
                .forget_lifetime();
            batch.pass = Some((targets, pass, PassState::default()));
        }
        let batch = self.batch.as_mut().expect("place_memory opens the batch");
        let (_, pass, state) = batch.pass.as_mut().unwrap();
        (pass, state)
    }

    /// The batch, with no render pass open, to record other passes into.
    fn take_batch(&mut self) -> Batch {
        let mut batch = self
            .batch
            .take()
            .unwrap_or_else(|| Batch::new(&self.device, self.submitter.recycled()));
        batch.pass = None;
        batch
    }

    /// Uploads the batched draws' data and submits them.
    fn submit(&mut self) {
        let Some(mut batch) = self.batch.take() else {
            return;
        };
        batch.pass = None;
        debug!(
            target: "renderer",
            "submitting {} draws reading {} bytes of guest memory",
            batch.constants.len() as u64 / self.constants_stride,
            batch.memory.bytes.len()
        );
        // The allocations come back for a later batch rather than faulting in fresh ones.
        self.submitter.send(Work::Submit {
            encoder: batch.encoder,
            uploads: Uploads {
                constants: (self.constants.clone(), batch.constants),
                memory: (self.memory.clone(), batch.memory.bytes),
                indices: (self.indices.clone(), batch.memory.indices),
            },
        });
    }

    /// A view of the whole texture, or of one aspect of a depth texture.
    fn view(&mut self, texture: &wgpu::Texture, aspect: wgpu::TextureAspect) -> wgpu::TextureView {
        if self.views.len() >= MAXIMUM_CACHED_VIEWS {
            self.views.clear();
        }
        self.views
            .entry((texture.clone(), aspect))
            .or_insert_with(|| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    aspect,
                    // The only six-layer textures are cubes.
                    dimension: (texture.depth_or_array_layers() == CUBE_FACES)
                        .then_some(wgpu::TextureViewDimension::Cube),
                    ..Default::default()
                })
            })
            .clone()
    }

    /// The host texture standing in for an EDRAM colour surface.
    fn render_target(
        &mut self,
        key: RenderTargetKey,
        format: wgpu::TextureFormat,
        size: (u32, u32),
    ) -> wgpu::Texture {
        let device = &self.device;
        self.render_targets
            .entry((key, format))
            .or_insert_with(|| {
                surface_texture(device, size, format, wgpu::TextureUsages::TEXTURE_BINDING)
            })
            .clone()
    }

    /// The host texture standing in for an EDRAM depth and stencil surface.
    fn depth_target(&mut self, key: &RenderTargetKey, size: (u32, u32)) -> wgpu::Texture {
        let device = &self.device;
        self.depth_targets
            .entry(*key)
            .or_insert_with(|| {
                surface_texture(
                    device,
                    size,
                    wgpu::TextureFormat::Depth24PlusStencil8,
                    wgpu::TextureUsages::TEXTURE_BINDING,
                )
            })
            .clone()
    }

    /// Translates and compiles a shader the first time it is used; returns its hash if usable.
    fn shader(&mut self, stage: ShaderStage, hash: u64, words: &[u32]) -> Option<u64> {
        if !self.shaders.contains_key(&(stage, hash)) {
            let translation = translate::translate(stage, words);
            let compiled = match translate::validate(&translation.wgsl) {
                Ok(()) => {
                    let module = self
                        .device
                        .create_shader_module(wgpu::ShaderModuleDescriptor {
                            label: Some("guest shader"),
                            source: wgpu::ShaderSource::Wgsl(Cow::Owned(translation.wgsl)),
                        });
                    let vertex_fetch_constants = Program::decode(words)
                        .instructions
                        .iter()
                        .filter_map(|(_, instruction)| match instruction {
                            Instruction::VertexFetch(fetch) => Some(fetch.constant()),
                            _ => None,
                        })
                        .collect();
                    Some(CompiledShader {
                        module,
                        textures: translation.textures,
                        vertex_fetch_constants,
                    })
                }
                Err(error) => {
                    warn!(target: "renderer", "{stage:?} shader {hash:016x} is not valid WGSL: {error}");
                    None
                }
            };
            self.shaders.insert((stage, hash), compiled);
        }
        self.shaders[&(stage, hash)].as_ref().map(|_| hash)
    }

    fn pipeline(
        &mut self,
        key: PipelineKey,
        textures: &BTreeMap<u32, TextureDimension>,
    ) -> Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout)> {
        if let Some(pipeline) = self.pipelines.get(&key) {
            return pipeline.clone();
        }
        let mut blend = blend_state(key.blend_control);
        if blend.is_none() && key.blend_control != BLEND_REPLACE {
            self.report(format!("blend control {:#x}", key.blend_control));
        }
        let blendable = key
            .color_format
            .guaranteed_format_features(self.device.features())
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::BLENDABLE);
        if blend.is_some() && !blendable {
            self.report(format!(
                "blending into {:?} render targets",
                key.color_format
            ));
            blend = None;
        }
        let mut entries = Vec::new();
        for (&constant, &dimension) in textures {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: constant * 2,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: match dimension {
                        TextureDimension::TwoD => wgpu::TextureViewDimension::D2,
                        TextureDimension::Cube => wgpu::TextureViewDimension::Cube,
                    },
                    multisampled: false,
                },
                count: None,
            });
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: constant * 2 + 1,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            });
        }
        let texture_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("guest textures"),
                    entries: &entries,
                });
        let layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(&self.constants_layout), Some(&texture_layout)],
                immediate_size: 0,
            });
        let vertex = &self.shaders[&(ShaderStage::Vertex, key.vertex_shader)]
            .as_ref()
            .unwrap()
            .module;
        let pixel = &self.shaders[&(ShaderStage::Pixel, key.pixel_shader)]
            .as_ref()
            .unwrap()
            .module;
        // Pixel shaders export four colours; only the first target exists so far.
        let targets = [
            Some(wgpu::ColorTargetState {
                format: key.color_format,
                blend,
                write_mask: wgpu::ColorWrites::from_bits_truncate(key.color_mask),
            }),
            None,
            None,
            None,
        ];
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("guest pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: vertex,
                    entry_point: Some(key.entry.name()),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: key.topology,
                    // Indexed strips name their index format.
                    strip_index_format: (key.entry == VertexEntry::Indexed
                        && key.topology.is_strip())
                    .then_some(wgpu::IndexFormat::Uint32),
                    front_face: key
                        .cull
                        .map_or(wgpu::FrontFace::Ccw, |(_, front_face)| front_face),
                    cull_mode: key.cull.map(|(face, _)| face),
                    ..Default::default()
                },
                depth_stencil: key.depth.map(depth_stencil_state),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: pixel,
                    entry_point: Some("main"),
                    compilation_options: Default::default(),
                    targets: &targets,
                }),
                multiview_mask: None,
                cache: None,
            });
        let result = match bevy::tasks::block_on(scope.pop()) {
            None => Some((pipeline, texture_layout)),
            Some(error) => {
                warn!(target: "renderer", "pipeline for shaders {:016x}/{:016x} failed: {error}", key.vertex_shader, key.pixel_shader);
                None
            }
        };
        self.pipelines.insert(key, result.clone());
        result
    }

    /// A texture's base level as it is in guest memory now (each of a cube's faces, one after
    /// another at 4 KiB boundaries). Each texture is read at most once a frame, and decoded and
    /// uploaded again only when its bytes have changed.
    fn upload_texture(
        &mut self,
        memory: &GuestMemory,
        view: View,
        descriptor: &TextureDescriptor,
        dimension: TextureDimension,
    ) -> wgpu::Texture {
        const FACE_ALIGNMENT: u32 = 4096;
        let faces = match dimension {
            TextureDimension::TwoD => 1,
            TextureDimension::Cube => CUBE_FACES,
        };
        // Render to texture: what a resolve wrote exists only on the host. Titles also sample
        // the top-left part of a destination (a luminance reduction halving its rectangle).
        if faces == 1
            && let Some(mut resolved) = self.resolved.get(&descriptor.base).cloned()
        {
            // Read as 8888, a depth resolve is the raw words EDRAM held.
            if let Some(&float_depth) = self.depth_resolves.get(&resolved)
                && descriptor.format == TEXTURE_FORMAT_8_8_8_8
            {
                resolved = self.packed_depth(&resolved, float_depth);
            }
            let (width, height) = (descriptor.width, descriptor.height);
            if (resolved.width(), resolved.height()) == (width, height) {
                return resolved;
            }
            if width <= resolved.width() && height <= resolved.height() {
                return self.resolved_part(&resolved, descriptor.base, width, height);
            }
        }
        let key = TextureKey::new(descriptor, faces);
        if let Some(cached) = self.textures.get(&key)
            && cached.checked_frame == self.presented
        {
            return cached.texture.clone();
        }
        let Some(face_size) = texture::guest_size(descriptor) else {
            self.report(format!("texture format {}", descriptor.format));
            return self.placeholder_texture(faces);
        };
        let face_stride = face_size.next_multiple_of(FACE_ALIGNMENT);
        let Some(address) = view(descriptor.base) else {
            self.report("textures past the end of physical memory".into());
            return self.placeholder_texture(faces);
        };
        let mut guest = vec![0; (face_stride * (faces - 1) + face_size) as usize];
        memory.read_bytes(address, &mut guest);
        if let Some(cached) = self.textures.get_mut(&key)
            && cached.guest == guest
        {
            cached.checked_frame = self.presented;
            return cached.texture.clone();
        }
        let decoded: Option<Vec<_>> = (0..faces)
            .map(|face| {
                let start = (face * face_stride) as usize;
                texture::decode(descriptor, &guest[start..start + face_size as usize])
            })
            .collect();
        let Some(decoded) = decoded else {
            self.report(format!("texture format {}", descriptor.format));
            return self.placeholder_texture(faces);
        };
        let data = decoded
            .iter()
            .flat_map(|face| &face.data)
            .copied()
            .collect();
        let (format, bytes_per_texel) = match decoded[0].format {
            HostFormat::R8 => (wgpu::TextureFormat::R8Unorm, 1),
            HostFormat::Rg8 => (wgpu::TextureFormat::Rg8Unorm, 2),
            HostFormat::Rgba8 => (wgpu::TextureFormat::Rgba8Unorm, 4),
            HostFormat::R32Float => (wgpu::TextureFormat::R32Float, 4),
            HostFormat::Rgba16Float => (wgpu::TextureFormat::Rgba16Float, 8),
        };
        let texture = match self.textures.get(&key) {
            Some(cached) if cached.texture.format() == format => cached.texture.clone(),
            _ => self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("guest texture"),
                size: wgpu::Extent3d {
                    width: descriptor.width,
                    height: descriptor.height,
                    depth_or_array_layers: faces,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }),
        };
        self.submitter.send(Work::WriteTexture {
            texture: texture.clone(),
            data,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(descriptor.width * bytes_per_texel),
                rows_per_image: Some(descriptor.height),
            },
        });
        self.textures.insert(
            key,
            CachedTexture {
                texture: texture.clone(),
                guest,
                checked_frame: self.presented,
            },
        );
        texture
    }

    /// The top-left `width` by `height` of a resolve destination, copied out after the passes
    /// recorded so far.
    fn resolved_part(
        &mut self,
        resolved: &wgpu::Texture,
        base: u32,
        width: u32,
        height: u32,
    ) -> wgpu::Texture {
        let device = &self.device;
        let part = self
            .resolved_parts
            .entry((base, width, height, resolved.format()))
            .or_insert_with(|| {
                surface_texture(
                    device,
                    (width, height),
                    resolved.format(),
                    wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                )
            })
            .clone();
        let mut batch = self.take_batch();
        batch.encoder.copy_texture_to_texture(
            resolved.as_image_copy(),
            part.as_image_copy(),
            part.size(),
        );
        self.batch = Some(batch);
        part
    }

    /// A depth resolve as the console's memory holds it: each pixel's EDRAM word, 24 bits of
    /// depth above 8 of stencil (kept as zero), as an 8888 texture. Titles read it this way to
    /// copy depth back into EDRAM through a colour surface (see `transfer_color_to_depth`).
    fn packed_depth(&mut self, resolved: &wgpu::Texture, float_depth: bool) -> wgpu::Texture {
        let device = &self.device;
        let packed = self
            .packed_depths
            .entry(resolved.clone())
            .or_insert_with(|| {
                surface_texture(
                    device,
                    (resolved.width(), resolved.height()),
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureUsages::TEXTURE_BINDING,
                )
            })
            .clone();
        let source = self.view(resolved, wgpu::TextureAspect::All);
        let target = self.view(&packed, wgpu::TextureAspect::All);
        let pipeline = self.depth_aliasing_pipeline(DepthAliasing::Pack, float_depth);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source),
            }],
        });
        let mut batch = self.take_batch();
        let mut pass = batch
            .encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pack depth"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: LOAD_AND_STORE_COLOR,
                })],
                ..Default::default()
            });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..FULL_SCREEN_TRIANGLE, 0..1);
        drop(pass);
        self.batch = Some(batch);
        packed
    }

    /// EDRAM is plain memory, so colour drawn where a depth surface lies is that surface's
    /// depth: titles restore depth by drawing a packed depth resolve as colour there. The host
    /// keeps the two apart, so before a draw tests depth, colour drawn over it since becomes
    /// its depth (stencil is left as it was; the host cannot write it from a shader).
    fn transfer_color_to_depth(
        &mut self,
        key: &RenderTargetKey,
        depth: &wgpu::Texture,
        float_depth: bool,
    ) {
        if !self.color_over_depth.remove(key) {
            return;
        }
        let Some(color) = self
            .render_targets
            .get(&(*key, wgpu::TextureFormat::Rgba8Unorm))
            .cloned()
        else {
            return;
        };
        let source = self.view(&color, wgpu::TextureAspect::All);
        let target = self.view(depth, wgpu::TextureAspect::All);
        let pipeline = self.depth_aliasing_pipeline(DepthAliasing::Unpack, float_depth);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&source),
            }],
        });
        let mut batch = self.take_batch();
        let mut pass = batch
            .encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("colour to depth"),
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &target,
                    depth_ops: Some(LOAD_AND_STORE),
                    stencil_ops: Some(LOAD_AND_STORE_STENCIL),
                }),
                ..Default::default()
            });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..FULL_SCREEN_TRIANGLE, 0..1);
        drop(pass);
        self.batch = Some(batch);
    }

    /// Depth written where a colour surface lies is that surface's colour on the console;
    /// titles clear both at once by clearing depth.
    fn mark_depth_over_color(&mut self, key: RenderTargetKey, float_depth: bool) {
        if self
            .render_targets
            .contains_key(&(key, wgpu::TextureFormat::Rgba8Unorm))
        {
            self.depth_over_color.insert(key, float_depth);
        }
    }

    /// The other way round from `transfer_color_to_depth`: before colour is drawn or resolved
    /// where depth has been written since, that colour becomes the depth surface's EDRAM words.
    fn transfer_depth_to_color(&mut self, key: &RenderTargetKey) {
        let Some(float_depth) = self.depth_over_color.remove(key) else {
            return;
        };
        let (Some(depth), Some(color)) = (
            self.depth_targets.get(key).cloned(),
            self.render_targets
                .get(&(*key, wgpu::TextureFormat::Rgba8Unorm))
                .cloned(),
        ) else {
            return;
        };
        let source = self.view(&depth, wgpu::TextureAspect::DepthOnly);
        let target = self.view(&color, wgpu::TextureAspect::All);
        let pipeline = self.depth_aliasing_pipeline(DepthAliasing::Repack, float_depth);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&source),
            }],
        });
        let mut batch = self.take_batch();
        let mut pass = batch
            .encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("depth to colour"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: LOAD_AND_STORE_COLOR,
                })],
                ..Default::default()
            });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..FULL_SCREEN_TRIANGLE, 0..1);
        drop(pass);
        self.batch = Some(batch);
    }

    fn depth_aliasing_pipeline(
        &mut self,
        kind: DepthAliasing,
        float_depth: bool,
    ) -> wgpu::RenderPipeline {
        let device = &self.device;
        self.depth_aliasing
            .entry((kind, float_depth))
            .or_insert_with(|| create_depth_aliasing(device, kind, float_depth))
            .clone()
    }

    /// Opaque white, one texel per face, for textures that cannot be uploaded.
    fn placeholder_texture(&mut self, faces: u32) -> wgpu::Texture {
        const TEXEL_BYTES: u32 = 4;
        if let Some(texture) = self.placeholders.get(&faces) {
            return texture.clone();
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("placeholder texture"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: faces,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.submitter.send(Work::WriteTexture {
            texture: texture.clone(),
            data: vec![u8::MAX; (TEXEL_BYTES * faces) as usize],
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(TEXEL_BYTES),
                rows_per_image: Some(1),
            },
        });
        self.placeholders.insert(faces, texture.clone());
        texture
    }

    fn sampler(&mut self, descriptor: &TextureDescriptor) -> wgpu::Sampler {
        let key = (
            descriptor.clamp_x,
            descriptor.clamp_y,
            descriptor.mag_filter,
            descriptor.min_filter,
        );
        let device = &self.device;
        self.samplers
            .entry(key)
            .or_insert_with(|| {
                device.create_sampler(&wgpu::SamplerDescriptor {
                    label: Some("guest sampler"),
                    address_mode_u: address_mode(descriptor.clamp_x),
                    address_mode_v: address_mode(descriptor.clamp_y),
                    mag_filter: filter(descriptor.mag_filter),
                    min_filter: filter(descriptor.min_filter),
                    ..Default::default()
                })
            })
            .clone()
    }

    /// A resolve: averages the samples of a rectangle of an EDRAM surface into the texture at
    /// the destination address (at the same pixel position, as the GPU does), then clears the
    /// surfaces if asked. Clears cover the whole surface rather than just the rectangle.
    fn resolve(&mut self, memory: &GuestMemory, view: View) {
        let register = |index| read_register(memory, index);
        let control = register(registers::RB_COPY_CONTROL);
        let surface = self.surface(register(registers::RB_SURFACE_INFO));
        let size = surface.host_size();
        let window_offset = edram::window_offset(register(registers::PA_SC_WINDOW_OFFSET));
        let vertex_window_offset =
            if register(registers::PA_SU_SC_MODE_CNTL) & VTX_WINDOW_OFFSET_ENABLE != 0 {
                window_offset
            } else {
                (0, 0)
            };
        let rectangle = resolve_vertices(memory, view).and_then(|vertices| {
            edram::resolve_rectangle(
                vertices,
                register(registers::PA_SU_VTX_CNTL) & PIXEL_CENTER_HALF == 0,
                vertex_window_offset,
                edram::scissor(
                    register(registers::PA_SC_WINDOW_SCISSOR_TL),
                    register(registers::PA_SC_WINDOW_SCISSOR_BR),
                    window_offset,
                ),
                surface.pitch,
            )
        });
        let source = control & COPY_SOURCE_MASK;
        if let Some(&info_register) = COLOR_INFO_REGISTERS.get(source as usize) {
            self.transfer_depth_to_color(&RenderTargetKey {
                base: register(info_register) & COLOR_BASE_MASK,
                width: size.0,
            });
        }
        let mut batch = self.take_batch();
        let encoder = &mut batch.encoder;
        match (COLOR_INFO_REGISTERS.get(source as usize), rectangle) {
            (Some(&info_register), Some(rectangle)) => {
                let info = register(info_register);
                let key = RenderTargetKey {
                    base: info & COLOR_BASE_MASK,
                    width: size.0,
                };
                let format = (info >> COLOR_FORMAT_SHIFT) & COLOR_FORMAT_MASK;
                if let Some(format) = color_target_format(format) {
                    self.resolve_color(encoder, memory, (key, format), surface, rectangle);
                }
            }
            (None, Some(rectangle)) if source == COPY_SOURCE_DEPTH => {
                let key = RenderTargetKey {
                    base: register(registers::RB_DEPTH_INFO) & COLOR_BASE_MASK,
                    width: size.0,
                };
                self.resolve_depth(encoder, memory, key, surface, rectangle);
            }
            _ => {}
        }
        let color_info = register(registers::RB_COLOR_INFO);
        if control & COPY_COLOR_CLEAR != 0
            && let Some(format) =
                color_target_format((color_info >> COLOR_FORMAT_SHIFT) & COLOR_FORMAT_MASK)
            && let Some(target) = self
                .render_targets
                .get(&(
                    RenderTargetKey {
                        base: color_info & COLOR_BASE_MASK,
                        width: size.0,
                    },
                    format,
                ))
                .cloned()
        {
            let view = self.view(&target, wgpu::TextureAspect::All);
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("resolve colour clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(
                            register(registers::RB_COLOR_CLEAR),
                            format,
                        )),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        let depth_key = RenderTargetKey {
            base: register(registers::RB_DEPTH_INFO) & COLOR_BASE_MASK,
            width: size.0,
        };
        if control & COPY_DEPTH_CLEAR != 0
            && let Some(target) = self.depth_targets.get(&depth_key).cloned()
        {
            self.color_over_depth.remove(&depth_key);
            self.mark_depth_over_color(
                depth_key,
                edram::depth_is_float(register(registers::RB_DEPTH_INFO)),
            );
            let clear = register(registers::RB_DEPTH_CLEAR);
            let view = self.view(&target, wgpu::TextureAspect::All);
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("resolve depth clear"),
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(edram::depth_clear_value(
                            clear,
                            register(registers::RB_DEPTH_INFO),
                        )),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear & STENCIL_MASK),
                        store: wgpu::StoreOp::Store,
                    }),
                }),
                ..Default::default()
            });
        }
        self.batch = Some(batch);
    }

    fn resolve_color(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        memory: &GuestMemory,
        key: (RenderTargetKey, wgpu::TextureFormat),
        surface: Surface,
        rectangle: Rectangle,
    ) {
        let info = read_register(memory, registers::RB_COPY_DEST_INFO);
        let format = (info >> COPY_DEST_FORMAT_SHIFT) & COPY_DEST_FORMAT_MASK;
        let Some(format) = resolve_destination_format(format) else {
            return self.report(format!("resolves to format {format}"));
        };
        let Some(source) = self.render_targets.get(&key).cloned() else {
            return;
        };
        let device = &self.device;
        let pipeline = self
            .resolves
            .entry(format)
            .or_insert_with(|| create_resolve(device, RESOLVE_SHADER, format))
            .clone();
        let view = self.view(&source, wgpu::TextureAspect::All);
        let size = (source.width(), source.height());
        if let Some(destination) =
            self.resolve_into(encoder, memory, &pipeline, &view, size, surface, rectangle)
        {
            self.depth_resolves.remove(&destination);
        }
    }

    /// A depth resolve: one sample per pixel of the depth surface's depth, as a float texture
    /// the title can sample (filterable 32-bit if the device allows, otherwise 16-bit).
    fn resolve_depth(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        memory: &GuestMemory,
        key: RenderTargetKey,
        surface: Surface,
        rectangle: Rectangle,
    ) {
        let Some(source) = self.depth_targets.get(&key).cloned() else {
            return;
        };
        let format = self.resolved_depth_format();
        let pipeline = self
            .resolve_depth
            .get_or_insert_with(|| create_resolve(&self.device, RESOLVE_DEPTH_SHADER, format))
            .clone();
        let view = self.view(&source, wgpu::TextureAspect::DepthOnly);
        let size = (source.width(), source.height());
        let float_depth = edram::depth_is_float(read_register(memory, registers::RB_DEPTH_INFO));
        if let Some(destination) =
            self.resolve_into(encoder, memory, &pipeline, &view, size, surface, rectangle)
        {
            self.depth_resolves.insert(destination, float_depth);
        }
    }

    fn resolved_depth_format(&self) -> wgpu::TextureFormat {
        if self
            .device
            .features()
            .contains(wgpu::Features::FLOAT32_FILTERABLE)
        {
            wgpu::TextureFormat::R32Float
        } else {
            wgpu::TextureFormat::R16Float
        }
    }

    /// Draws a resolve pipeline over `rectangle` of the texture at the destination address,
    /// creating that texture in the pipeline's format if needed.
    #[allow(clippy::too_many_arguments)]
    fn resolve_into(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        memory: &GuestMemory,
        pipeline: &ResolvePipeline,
        source: &wgpu::TextureView,
        (source_width, source_height): (u32, u32),
        surface: Surface,
        rectangle: Rectangle,
    ) -> Option<wgpu::Texture> {
        let register = |index| read_register(memory, index);
        let pitch = register(registers::RB_COPY_DEST_PITCH);
        let (width, height) = (
            pitch & COPY_DEST_PITCH_MASK,
            (pitch >> COPY_DEST_HEIGHT_SHIFT) & COPY_DEST_PITCH_MASK,
        );
        let (sample_x, sample_y) = surface.sample_shift;
        let rectangle = rectangle
            .clamp(width, height)
            .clamp(source_width >> sample_x, source_height >> sample_y);
        if rectangle.is_empty() {
            return None;
        }
        let destination_address = register(registers::RB_COPY_DEST_BASE);
        if self.resolve_dump.is_some() {
            info!(target: "dump", "resolve: to {destination_address:#010x} {width}x{height}, rectangle {rectangle:?}, dest info {:#010x}, control {:#010x}, surface {:#010x}, color info {:#010x}, source {source_width}x{source_height}",
                register(registers::RB_COPY_DEST_INFO), register(registers::RB_COPY_CONTROL), register(registers::RB_SURFACE_INFO),
                register(registers::RB_COLOR_INFO));
        }
        // Titles resolve the screen's lower tile to the address of its first row, inside the
        // texture the upper tile was resolved to.
        let containing = self.resolved.iter().find_map(|(&base, texture)| {
            let size = (texture.width(), texture.height());
            let rows_down = rows_into(base, size, destination_address)?;
            (texture.format() == pipeline.format).then(|| (texture.clone(), rows_down))
        });
        let (destination, rows_down) = match (containing, self.resolved.get(&destination_address)) {
            (Some(containing), _) => containing,
            (None, Some(texture))
                if (texture.width(), texture.height(), texture.format())
                    == (width, height, pipeline.format) =>
            {
                (texture.clone(), 0)
            }
            _ => {
                let texture = surface_texture(
                    &self.device,
                    (width, height),
                    pipeline.format,
                    wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                );
                self.resolved.insert(destination_address, texture.clone());
                (texture, 0)
            }
        };
        let rectangle = Rectangle {
            top: rectangle.top + rows_down,
            bottom: (rectangle.bottom + rows_down).min(destination.height()),
            ..rectangle
        };
        let swap_red_blue =
            u32::from(register(registers::RB_COPY_DEST_INFO) & COPY_DEST_SWAP_RED_BLUE != 0);
        if swap_red_blue != 0 {
            self.swapped.insert(destination.clone());
        } else {
            self.swapped.remove(&destination);
        }
        let device = &self.device;
        let parameters = self
            .resolve_parameters
            .entry((sample_x, sample_y, rows_down, swap_red_blue))
            .or_insert_with(|| {
                device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("resolve parameters"),
                    contents: words_as_bytes(&[
                        sample_x,
                        sample_y,
                        0,
                        rows_down,
                        swap_red_blue,
                        0,
                        0,
                        0,
                    ]),
                    usage: wgpu::BufferUsages::UNIFORM,
                })
            })
            .clone();
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: parameters.as_entire_binding(),
                },
            ],
        });
        let destination_view = self.view(&destination, wgpu::TextureAspect::All);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("resolve"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &destination_view,
                depth_slice: None,
                resolve_target: None,
                ops: LOAD_AND_STORE_COLOR,
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline.pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_scissor_rect(
            rectangle.left,
            rectangle.top,
            rectangle.right - rectangle.left,
            rectangle.bottom - rectangle.top,
        );
        pass.draw(0..FULL_SCREEN_TRIANGLE, 0..1);
        drop(pass);
        if let Some(dump) = &mut self.resolve_dump {
            let mut name = dump.prefix.file_name().unwrap_or_default().to_os_string();
            name.push(format!(
                "-{:02}-{destination_address:08x}.png",
                dump.readbacks.len()
            ));
            let path = dump.prefix.with_file_name(name);
            dump.readbacks
                .extend(Readback::record(&self.device, encoder, &destination, path));
        }
        Some(destination)
    }
}

/// How many rows into a resolve destination of `size` texels at `base` the address `address`
/// starts, if it starts a row below the first.
fn rows_into(base: u32, (width, height): (u32, u32), address: u32) -> Option<u32> {
    let row_bytes = width * RESOLVE_DESTINATION_BYTES_PER_TEXEL;
    let offset = address.checked_sub(base).filter(|&offset| offset > 0)?;
    let rows_down = offset / row_bytes;
    (offset % row_bytes == 0 && rows_down < height).then_some(rows_down)
}

/// The three vertices of a resolve's rectangle, which Direct3D writes to vertex fetch constant
/// 0 as two floats each.
fn resolve_vertices(memory: &GuestMemory, view: View) -> Option<[f32; 6]> {
    const RECTANGLE_WORDS: u32 = 6;
    const ENDIAN_MASK: u32 = 0x3;
    let constant = read_register(memory, FETCH_CONSTANTS);
    let size_word = read_register(memory, FETCH_CONSTANTS + 1);
    if (size_word >> VERTEX_SIZE_SHIFT) & VERTEX_SIZE_MASK != RECTANGLE_WORDS {
        return None;
    }
    let address = view(constant & VERTEX_ADDRESS_MASK)?;
    Some(std::array::from_fn(|index| {
        // The GPU reads words little-endian, then applies the constant's byte order.
        let word = memory
            .read_u32(address + index as u32 * WORD_SIZE)
            .swap_bytes();
        f32::from_bits(gpu_byte_order(word, size_word & ENDIAN_MASK))
    }))
}

/// A word as the GPU sees it after its byte order (xenos::Endian) is applied.
fn gpu_byte_order(word: u32, endian: u32) -> u32 {
    const EIGHT_IN_16: u32 = 1;
    const EIGHT_IN_32: u32 = 2;
    const SIXTEEN_IN_32: u32 = 3;
    match endian {
        EIGHT_IN_16 => ((word & 0x00FF_00FF) << 8) | ((word >> 8) & 0x00FF_00FF),
        EIGHT_IN_32 => word.swap_bytes(),
        SIXTEEN_IN_32 => word.rotate_left(16),
        _ => word,
    }
}

/// A texture standing in for an EDRAM surface or a resolve destination.
fn surface_texture(
    device: &wgpu::Device,
    (width, height): (u32, u32),
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("EDRAM surface"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: usage | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    })
}

/// The host depth and stencil state for RB_DEPTHCONTROL and the stencil masks.
fn depth_stencil_state(depth: DepthState) -> wgpu::DepthStencilState {
    const STENCIL_ENABLE: u32 = 1 << 0;
    const Z_ENABLE: u32 = 1 << 1;
    const Z_WRITE_ENABLE: u32 = 1 << 2;
    const Z_FUNCTION_SHIFT: u32 = 4;
    const BACKFACE_ENABLE: u32 = 1 << 7;
    const FRONT_STENCIL_SHIFT: u32 = 8;
    const BACK_STENCIL_SHIFT: u32 = 20;
    const FIELD_MASK: u32 = 0x7;
    let control = depth.control;
    // Each face: function, then the operations on stencil fail, depth pass and depth fail.
    let face = |shift: u32| wgpu::StencilFaceState {
        compare: compare_function((control >> shift) & FIELD_MASK),
        fail_op: stencil_operation((control >> (shift + 3)) & FIELD_MASK),
        pass_op: stencil_operation((control >> (shift + 6)) & FIELD_MASK),
        depth_fail_op: stencil_operation((control >> (shift + 9)) & FIELD_MASK),
    };
    let z_enabled = control & Z_ENABLE != 0;
    let front = face(FRONT_STENCIL_SHIFT);
    wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Depth24PlusStencil8,
        depth_write_enabled: Some(z_enabled && control & Z_WRITE_ENABLE != 0),
        depth_compare: Some(if z_enabled {
            compare_function((control >> Z_FUNCTION_SHIFT) & FIELD_MASK)
        } else {
            wgpu::CompareFunction::Always
        }),
        stencil: if control & STENCIL_ENABLE != 0 {
            wgpu::StencilState {
                front,
                back: if control & BACKFACE_ENABLE != 0 {
                    face(BACK_STENCIL_SHIFT)
                } else {
                    front
                },
                read_mask: (depth.stencil_masks >> STENCIL_READ_MASK_SHIFT) & STENCIL_MASK,
                write_mask: (depth.stencil_masks >> STENCIL_WRITE_MASK_SHIFT) & STENCIL_MASK,
            }
        } else {
            wgpu::StencilState::default()
        },
        bias: Default::default(),
    }
}

/// xenos::CompareFunction.
fn compare_function(function: u32) -> wgpu::CompareFunction {
    use wgpu::CompareFunction::*;
    [
        Never,
        Less,
        Equal,
        LessEqual,
        Greater,
        NotEqual,
        GreaterEqual,
        Always,
    ][function as usize & 7]
}

/// xenos::StencilOp.
fn stencil_operation(operation: u32) -> wgpu::StencilOperation {
    use wgpu::StencilOperation::*;
    [
        Keep,
        Zero,
        Replace,
        IncrementClamp,
        DecrementClamp,
        Invert,
        IncrementWrap,
        DecrementWrap,
    ][operation as usize & 7]
}

const LOAD_AND_STORE: wgpu::Operations<f32> = wgpu::Operations {
    load: wgpu::LoadOp::Load,
    store: wgpu::StoreOp::Store,
};
const LOAD_AND_STORE_STENCIL: wgpu::Operations<u32> = wgpu::Operations {
    load: wgpu::LoadOp::Load,
    store: wgpu::StoreOp::Store,
};
const LOAD_AND_STORE_COLOR: wgpu::Operations<wgpu::Color> = wgpu::Operations {
    load: wgpu::LoadOp::Load,
    store: wgpu::StoreOp::Store,
};

/// Resolves average each destination pixel's samples (2^x by 2^y of them), from the rows the
/// destination offset is below, exchanging red and blue if asked.
const RESOLVE_SHADER: &str = "\
struct Parameters {
    sample_shift: vec2<u32>,
    destination_offset: vec2<u32>,
    swap_red_blue: u32,
}
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<uniform> parameters: Parameters;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let first = (vec2<u32>(position.xy) - parameters.destination_offset) << parameters.sample_shift;
    let count = vec2<u32>(1u) << parameters.sample_shift;
    var sum = vec4<f32>(0.0);
    for (var y = 0u; y < count.y; y += 1u) {
        for (var x = 0u; x < count.x; x += 1u) {
            sum += textureLoad(source, first + vec2<u32>(x, y), 0);
        }
    }
    let average = sum / f32(count.x * count.y);
    return select(average, average.bgra, parameters.swap_red_blue != 0u);
}
";
/// Depth resolves copy each pixel's first sample.
const RESOLVE_DEPTH_SHADER: &str = "\
struct Parameters {
    sample_shift: vec2<u32>,
    destination_offset: vec2<u32>,
    swap_red_blue: u32,
}
@group(0) @binding(0) var source: texture_depth_2d;
@group(0) @binding(1) var<uniform> parameters: Parameters;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let first = (vec2<u32>(position.xy) - parameters.destination_offset) << parameters.sample_shift;
    let depth = textureLoad(source, first, 0);
    return vec4<f32>(depth, 0.0, 0.0, 1.0);
}
";

/// A resolve pipeline and the format of the textures it writes.
#[derive(Clone)]
struct ResolvePipeline {
    pipeline: wgpu::RenderPipeline,
    format: wgpu::TextureFormat,
}

fn create_resolve(
    device: &wgpu::Device,
    shader: &'static str,
    format: wgpu::TextureFormat,
) -> ResolvePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("resolve"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(shader)),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("resolve"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fragment"),
            compilation_options: Default::default(),
            targets: &[Some(format.into())],
        }),
        multiview_mask: None,
        cache: None,
    });
    ResolvePipeline { pipeline, format }
}

/// The two directions depth moves between the host's depth surfaces and EDRAM's words.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum DepthAliasing {
    /// A depth resolve to the 8888 texture of its EDRAM words.
    Pack,
    /// EDRAM words drawn as colour to a depth surface.
    Unpack,
    /// A depth surface's EDRAM words, read as the colour surface sharing them.
    Repack,
}

fn create_depth_aliasing(
    device: &wgpu::Device,
    kind: DepthAliasing,
    float_depth: bool,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("depth aliasing"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(DEPTH_ALIASING_SHADER)),
    });
    let constants = [("FLOAT_DEPTH", if float_depth { 1.0 } else { 0.0 })];
    let compilation_options = wgpu::PipelineCompilationOptions {
        constants: &constants,
        ..Default::default()
    };
    let (entry_point, targets, depth_stencil): (_, &[_], _) = match kind {
        DepthAliasing::Repack => (
            "repack",
            &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
            None,
        ),
        DepthAliasing::Pack => (
            "pack",
            &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
            None,
        ),
        DepthAliasing::Unpack => (
            "unpack",
            &[],
            Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth24PlusStencil8,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: Default::default(),
            }),
        ),
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("depth aliasing"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vertex"),
            compilation_options: compilation_options.clone(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(entry_point),
            compilation_options,
            targets,
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Depth as EDRAM holds it: a word of 24 bits of depth above 8 of stencil, the depth a 24-bit
/// unorm or (D24FS8) a 20e4 float, converted as Xenia's Float32To20e4 and Float20e4To32 do.
/// As an 8888 surface or texture, the word's low byte is red. EDRAM tiles are 80 samples wide,
/// and depth surfaces keep the two 40-sample halves of each the other way round from colour, so
/// a depth sample is the colour sample 40 along in its tile (host surfaces are in samples,
/// starting at a tile's edge).
const DEPTH_ALIASING_SHADER: &str = "\
override FLOAT_DEPTH: bool;
@group(0) @binding(0) var source: texture_2d<f32>;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

fn encode_depth(depth: f32) -> u32 {
    if !FLOAT_DEPTH {
        return u32(round(saturate(depth) * 16777215.0));
    }
    var bits = bitcast<u32>(depth);
    if bitcast<i32>(bits) <= 0 {
        return 0u;
    }
    if bits >= 0x3FFFFFF8u {
        return 0xFFFFFFu;
    }
    if bits < 0x38800000u {
        // Below 20e4's smallest normal value, 2^-14: a denormal.
        bits = (0x800000u | (bits & 0x7FFFFFu)) >> min(113u - (bits >> 23u), 24u);
    } else {
        // Rebias the exponent from f32's 127 to 20e4's 15.
        bits += 0xC8000000u;
    }
    // Round to nearest, ties to even, as the mantissa drops from 23 bits to 20.
    bits += 3u + ((bits >> 3u) & 1u);
    return (bits >> 3u) & 0xFFFFFFu;
}

fn decode_depth(depth: u32) -> f32 {
    if !FLOAT_DEPTH {
        return f32(depth) / 16777215.0;
    }
    var exponent = depth >> 20u;
    var mantissa = depth & 0xFFFFFu;
    if exponent == 0u {
        if mantissa == 0u {
            return 0.0;
        }
        let shift = countLeadingZeros(mantissa) - 11u;
        exponent = 1u - shift;
        mantissa = (mantissa << shift) & 0xFFFFFu;
    }
    return bitcast<f32>(((exponent + 112u) << 23u) | (mantissa << 3u));
}

@fragment
fn pack(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let depth = textureLoad(source, vec2<u32>(position.xy), 0).x;
    return unpack4x8unorm(encode_depth(depth) << 8u);
}

const TILE_SAMPLES: u32 = 80u;

// The sample 40 along in its 80-sample EDRAM tile: where colour and depth exchange halves.
fn swapped_sample(sample: vec2<u32>) -> vec2<u32> {
    let tile_start = sample.x - sample.x % TILE_SAMPLES;
    return vec2<u32>(tile_start + (sample.x % TILE_SAMPLES + TILE_SAMPLES / 2u) % TILE_SAMPLES, sample.y);
}

@group(0) @binding(1) var depth_source: texture_depth_2d;

@fragment
fn repack(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let depth = textureLoad(depth_source, swapped_sample(vec2<u32>(position.xy)), 0);
    return unpack4x8unorm(encode_depth(depth) << 8u);
}

@fragment
fn unpack(@builtin(position) position: vec4<f32>) -> @builtin(frag_depth) f32 {
    let word = pack4x8unorm(textureLoad(source, swapped_sample(vec2<u32>(position.xy)), 0));
    return decode_depth(word >> 8u);
}
";

/// RB_MODECONTROL's modes for ordinary colour and depth rendering, and for depth alone.
const EDRAM_MODE_COLOR_DEPTH: u32 = 4;
const EDRAM_MODE_DEPTH_ONLY: u32 = 5;
/// RB_DEPTHCONTROL's stencil, depth test and depth write enables.
const DEPTH_ENABLES: u32 = 0x7;

/// The display shows the front buffer's colour; like the console's scan-out, it ignores alpha.
const BLIT_SHADER: &str = "\
@group(0) @binding(0) var source: texture_2d<f32>;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureLoad(source, vec2<i32>(position.xy), 0).rgb, 1.0);
}

@fragment
fn fragment_swapped(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4<f32>(textureLoad(source, vec2<i32>(position.xy), 0).bgr, 1.0);
}
";
/// One triangle covering the viewport.
const FULL_SCREEN_TRIANGLE: u32 = 3;

fn create_blit(device: &wgpu::Device, swapped: bool) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("present"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(BLIT_SHADER)),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("present"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(if swapped {
                "fragment_swapped"
            } else {
                "fragment"
            }),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// What storage buffer bindings' offsets must be multiples of.
fn storage_offset_alignment(device: &wgpu::Device) -> u64 {
    device.limits().min_storage_buffer_offset_alignment.into()
}

fn storage_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("batched draw data"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// The vertex entry point, host topology and host vertex count for a guest primitive type.
fn host_primitive(
    primitive: u32,
    count: u32,
) -> Option<(VertexEntry, wgpu::PrimitiveTopology, u32)> {
    use wgpu::PrimitiveTopology as Topology;
    const QUAD_VERTICES: u32 = 4;
    const RECTANGLE_VERTICES: u32 = 3;
    const TRIANGLE_VERTICES: u32 = 3;
    const TWO_TRIANGLES: u32 = 6;
    Some(match primitive {
        POINT_LIST => (VertexEntry::Direct, Topology::PointList, count),
        LINE_LIST => (VertexEntry::Direct, Topology::LineList, count),
        LINE_STRIP => (VertexEntry::Direct, Topology::LineStrip, count),
        TRIANGLE_LIST => (VertexEntry::Direct, Topology::TriangleList, count),
        TRIANGLE_STRIP => (VertexEntry::Direct, Topology::TriangleStrip, count),
        TRIANGLE_FAN => (
            VertexEntry::TriangleFan,
            Topology::TriangleList,
            count.saturating_sub(2) * TRIANGLE_VERTICES,
        ),
        RECTANGLE_LIST => (
            VertexEntry::RectangleList,
            Topology::TriangleList,
            count / RECTANGLE_VERTICES * TWO_TRIANGLES,
        ),
        QUAD_LIST => (
            VertexEntry::QuadList,
            Topology::TriangleList,
            count / QUAD_VERTICES * TWO_TRIANGLES,
        ),
        _ => return None,
    })
}

/// Whether RB_DEPTHCONTROL lets a draw write depth or stencil.
fn writes_depth_or_stencil(control: u32) -> bool {
    const STENCIL_ENABLE: u32 = 1 << 0;
    const Z_ENABLE_AND_WRITE: u32 = (1 << 1) | (1 << 2);
    control & Z_ENABLE_AND_WRITE == Z_ENABLE_AND_WRITE || control & STENCIL_ENABLE != 0
}

/// What a draw's PA_SU_SC_MODE_CNTL culls.
#[derive(Debug, PartialEq, Eq)]
enum Culling {
    Nothing,
    /// The faces culled, and which winding faces the front.
    Faces(wgpu::Face, wgpu::FrontFace),
    /// Front and back faces both: the draw draws nothing.
    Everything,
}

/// As on the console, only triangles and quads are culled; the title's rectangles
/// (screen-space quads) never are. Windings are in framebuffer space, Y down, on the console
/// and on the host alike.
fn culling(mode_control: u32, entry: VertexEntry, topology: wgpu::PrimitiveTopology) -> Culling {
    use wgpu::PrimitiveTopology as Topology;
    let polygonal = entry != VertexEntry::RectangleList
        && matches!(topology, Topology::TriangleList | Topology::TriangleStrip);
    let front_face = if mode_control & FRONT_FACE_CLOCKWISE != 0 {
        wgpu::FrontFace::Cw
    } else {
        wgpu::FrontFace::Ccw
    };
    match (
        mode_control & CULL_FRONT != 0,
        mode_control & CULL_BACK != 0,
    ) {
        _ if !polygonal => Culling::Nothing,
        (false, false) => Culling::Nothing,
        (true, true) => Culling::Everything,
        (true, false) => Culling::Faces(wgpu::Face::Front, front_face),
        (false, true) => Culling::Faces(wgpu::Face::Back, front_face),
    }
}

/// A draw's guest indices in host order: `count` of them from `start` bytes into `guest` (raw
/// guest memory, read as the GPU reads it: little-endian words, then the index buffer's byte
/// order), each 16 or 32 bits.
fn host_indices(
    guest: &[u8],
    start: usize,
    count: usize,
    is_32_bit: bool,
    endian: u32,
    restart: Option<u32>,
) -> Vec<u32> {
    // A 16-bit index buffer compares the reset index's low half.
    let restart = restart.map(|index| {
        if is_32_bit {
            index
        } else {
            index & u32::from(u16::MAX)
        }
    });
    let word = |byte: usize| {
        let first = byte & !(WORD_SIZE as usize - 1);
        guest
            .get(first..first + WORD_SIZE as usize)
            .map_or(0, |bytes| {
                gpu_byte_order(u32::from_le_bytes(bytes.try_into().unwrap()), endian)
            })
    };
    (0..count)
        .map(|index| {
            if is_32_bit {
                word(start + index * WORD_SIZE as usize)
            } else {
                let byte = start + index * size_of::<u16>();
                (word(byte) >> ((byte & size_of::<u16>()) * u8::BITS as usize))
                    & u32::from(u16::MAX)
            }
        })
        .map(|index| {
            if Some(index) == restart {
                HOST_RESTART_INDEX
            } else {
                index
            }
        })
        .collect()
}

/// The scale and offset taking a guest position (after undoing its W formats) to host clip
/// space: the guest viewport transform to pixels, then pixels to host normalized coordinates,
/// with Direct3D 9's integer pixel centers moved to the host's half-pixel ones and the window
/// offset applied.
fn position_transform(
    vte: u32,
    viewport: [f32; VIEWPORT_REGISTERS as usize],
    [width, height]: [f32; 2],
    half_pixel: bool,
    (window_x, window_y): (i32, i32),
) -> ([f32; 3], [f32; 3]) {
    const HALF_PIXEL: f32 = 0.5;
    let enabled = |bit: u32, value: f32, default: f32| if vte & bit != 0 { value } else { default };
    let [x_scale, x_offset, y_scale, y_offset, z_scale, z_offset] = viewport;
    let pixel_offset = if half_pixel { HALF_PIXEL } else { 0.0 };
    let x_scale = enabled(VTE_X_SCALE, x_scale, 1.0);
    let x_offset = enabled(VTE_X_OFFSET, x_offset, 0.0) + pixel_offset + window_x as f32;
    let y_scale = enabled(VTE_Y_SCALE, y_scale, 1.0);
    let y_offset = enabled(VTE_Y_OFFSET, y_offset, 0.0) + pixel_offset + window_y as f32;
    (
        [
            x_scale * 2.0 / width,
            -y_scale * 2.0 / height,
            enabled(VTE_Z_SCALE, z_scale, 1.0),
        ],
        [
            x_offset * 2.0 / width - 1.0,
            1.0 - y_offset * 2.0 / height,
            enabled(VTE_Z_OFFSET, z_offset, 0.0),
        ],
    )
}

/// `position_format` in the constants: PA_CL_VTE_CNTL's three W formats, and whether
/// PA_CL_CLIP_CNTL turns clipping off.
fn position_format(vte_control: u32, clip_control: u32) -> u32 {
    const CLIP_DISABLE: u32 = 1 << 16;
    const CLIPPING_OFF: u32 = 1 << 3;
    let clipping = if clip_control & CLIP_DISABLE != 0 {
        CLIPPING_OFF
    } else {
        0
    };
    ((vte_control >> VTE_FORMAT_SHIFT) & VTE_FORMAT_MASK) | clipping
}

/// The per-draw values after the register blocks in the constants buffer.
struct DrawWords {
    /// Each vertex fetch constant's buffer's byte offset in the batch's guest memory.
    vertex_offsets: Vec<(u32, u32)>,
    /// The index buffer's byte offset in the batch's guest memory, and its description.
    indices: Option<(u32, IndexBuffer)>,
    indices_are_32_bit: bool,
    index_bias: u32,
    position_format: u32,
    scale: [f32; 3],
    offset: [f32; 3],
    alpha_test: [u32; ALPHA_TEST_WORDS],
    pixel_parameters: u32,
}

/// The alpha test's words in the constants buffer, from RB_COLORCONTROL, RB_ALPHA_REF (a
/// float's bits) and the surface's samples per pixel (see `translate::CONSTANTS_LAYOUT`).
fn alpha_test_words(
    color_control: u32,
    reference: u32,
    (sample_x, sample_y): (u32, u32),
) -> [u32; ALPHA_TEST_WORDS] {
    const FUNCTION_MASK: u32 = 0x7;
    const TEST_ENABLE: u32 = 1 << 3;
    const TO_MASK_ENABLE: u32 = 1 << 4;
    const TO_MASK_OFFSETS_SHIFT: u32 = 24;
    const COMPARE_ALWAYS: u32 = 7;
    /// `alpha_to_mask`'s bit saying alpha to coverage is on, above its offsets.
    const TO_MASK_ON: u32 = 1 << 8;
    const SAMPLE_Y_SHIFT: u32 = 2;
    let function = if color_control & TEST_ENABLE != 0 {
        color_control & FUNCTION_MASK
    } else {
        COMPARE_ALWAYS
    };
    let to_mask = if color_control & TO_MASK_ENABLE != 0 {
        (color_control >> TO_MASK_OFFSETS_SHIFT) | TO_MASK_ON
    } else {
        0
    };
    [
        function,
        reference,
        to_mask,
        sample_x | (sample_y << SAMPLE_Y_SHIFT),
    ]
}

/// The register a pixel shader finds the pixel parameters in (Xenos ps_param_gen): if
/// SQ_PROGRAM_CNTL asks for them, the interpolator SQ_CONTEXT_MISC names; otherwise none, as
/// `translate::NO_PIXEL_PARAMETERS`.
fn pixel_parameters_register(program_control: u32, context_misc: u32) -> u32 {
    const PARAMETERS_ENABLE: u32 = 1 << 18;
    const POSITION_SHIFT: u32 = 8;
    const POSITION_MASK: u32 = 0xFF;
    let register = (context_misc >> POSITION_SHIFT) & POSITION_MASK;
    if program_control & PARAMETERS_ENABLE != 0 && register < translate::INTERPOLATORS {
        register
    } else {
        translate::NO_PIXEL_PARAMETERS
    }
}

/// The constants buffer's words, in the order of `translate::CONSTANTS_LAYOUT`.
fn constants_words(
    memory: &GuestMemory,
    draw: &DrawWords,
    swizzles: &[u32; TEXTURE_CONSTANTS],
) -> Vec<u32> {
    const INDEX_BITS_16: u32 = 16;
    const INDEX_BITS_32: u32 = 32;
    let mut words = Vec::with_capacity(CONSTANTS_WORDS);
    for (first, count) in [FLOAT_CONSTANTS, FETCH, BOOLS, LOOPS] {
        let start = words.len();
        words.resize(start + count, 0);
        read_registers(memory, first, &mut words[start..]);
    }
    // Shaders find vertex buffers in the batch's guest memory, not at their guest addresses.
    for &(constant, offset) in &draw.vertex_offsets {
        let word = &mut words[FLOAT_CONSTANTS.1 + constant as usize * 2];
        *word = offset | (*word & !VERTEX_ADDRESS_MASK);
    }
    let (index_offset, index_bits, index_endian) = match draw.indices {
        Some((offset, indices)) => (
            offset,
            if draw.indices_are_32_bit {
                INDEX_BITS_32
            } else {
                INDEX_BITS_16
            },
            indices.endian,
        ),
        None => (0, 0, 0),
    };
    words.extend([
        index_offset,
        index_bits,
        index_endian,
        draw.index_bias,
        draw.position_format,
    ]);
    words.extend(
        draw.scale
            .iter()
            .chain(&draw.offset)
            .map(|value| value.to_bits()),
    );
    words.extend(swizzles);
    words.extend(draw.alpha_test);
    words.push(draw.pixel_parameters);
    words.resize(CONSTANTS_SIZE as usize / WORD_SIZE as usize, 0);
    words
}

pub(crate) fn words_as_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and u8 has no alignment requirement.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), std::mem::size_of_val(words)) }
}

/// The host format standing in for an EDRAM colour format (xenos::ColorRenderTargetFormat).
fn color_target_format(format: u32) -> Option<wgpu::TextureFormat> {
    match format {
        COLOR_FORMAT_8_8_8_8 | COLOR_FORMAT_8_8_8_8_GAMMA => Some(wgpu::TextureFormat::Rgba8Unorm),
        COLOR_FORMAT_32_FLOAT => Some(wgpu::TextureFormat::R32Float),
        _ => None,
    }
}

/// The host format of a resolve's destination texture (RB_COPY_DEST_INFO's format).
fn resolve_destination_format(format: u32) -> Option<wgpu::TextureFormat> {
    match format {
        COPY_FORMAT_8_8_8_8 => Some(wgpu::TextureFormat::Rgba8Unorm),
        COPY_FORMAT_32_FLOAT => Some(wgpu::TextureFormat::R32Float),
        _ => None,
    }
}

/// RB_COLOR_CLEAR holds the clear colour in the surface's format: ARGB bytes, or a float.
fn clear_color(value: u32, format: wgpu::TextureFormat) -> wgpu::Color {
    if format == wgpu::TextureFormat::R32Float {
        return wgpu::Color {
            r: f64::from(f32::from_bits(value)),
            ..wgpu::Color::TRANSPARENT
        };
    }
    let channel = |shift: u32| f64::from((value >> shift) & 0xFF) / f64::from(u8::MAX);
    wgpu::Color {
        r: channel(16),
        g: channel(8),
        b: channel(0),
        a: channel(24),
    }
}

fn address_mode(clamp: u32) -> wgpu::AddressMode {
    const REPEAT: u32 = 0;
    const MIRRORED_REPEAT: u32 = 1;
    match clamp {
        REPEAT => wgpu::AddressMode::Repeat,
        MIRRORED_REPEAT => wgpu::AddressMode::MirrorRepeat,
        _ => wgpu::AddressMode::ClampToEdge,
    }
}

fn filter(filter: u32) -> wgpu::FilterMode {
    const LINEAR: u32 = 1;
    if filter == LINEAR {
        wgpu::FilterMode::Linear
    } else {
        wgpu::FilterMode::Nearest
    }
}

/// The host blend state for RB_BLENDCONTROL, or None to replace (or if it cannot be mapped).
fn blend_state(control: u32) -> Option<wgpu::BlendState> {
    if control == BLEND_REPLACE {
        return None;
    }
    let component = |shift: u32| -> Option<wgpu::BlendComponent> {
        let value = control >> shift;
        Some(wgpu::BlendComponent {
            src_factor: blend_factor(value & BLEND_FACTOR_MASK)?,
            operation: blend_operation((value >> BLEND_OPERATION_SHIFT) & BLEND_OPERATION_MASK)?,
            dst_factor: blend_factor((value >> BLEND_DESTINATION_SHIFT) & BLEND_FACTOR_MASK)?,
        })
    };
    let normalize = |component: wgpu::BlendComponent| match component.operation {
        // Min and max ignore the factors, and wgpu requires them to be one.
        wgpu::BlendOperation::Min | wgpu::BlendOperation::Max => wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            ..component
        },
        _ => component,
    };
    Some(wgpu::BlendState {
        color: normalize(component(0)?),
        alpha: normalize(component(BLEND_ALPHA_SHIFT)?),
    })
}

/// xenos::BlendFactor; constant alpha factors use the constant colour.
fn blend_factor(factor: u32) -> Option<wgpu::BlendFactor> {
    use wgpu::BlendFactor::*;
    Some(match factor {
        0 => Zero,
        1 => One,
        4 => Src,
        5 => OneMinusSrc,
        6 => SrcAlpha,
        7 => OneMinusSrcAlpha,
        8 => Dst,
        9 => OneMinusDst,
        10 => DstAlpha,
        11 => OneMinusDstAlpha,
        12 | 14 => Constant,
        13 | 15 => OneMinusConstant,
        16 => SrcAlphaSaturated,
        _ => return None,
    })
}

/// xenos::BlendOp.
fn blend_operation(operation: u32) -> Option<wgpu::BlendOperation> {
    use wgpu::BlendOperation::*;
    Some(match operation {
        0 => Add,
        1 => Subtract,
        2 => Min,
        3 => Max,
        4 => ReverseSubtract,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_words_match_the_wgsl_layout() {
        let module = naga::front::wgsl::parse_str(translate::CONSTANTS_LAYOUT).unwrap();
        let mut layouter = naga::proc::Layouter::default();
        layouter.update(module.to_ctx()).unwrap();
        let (handle, constants) = module
            .types
            .iter()
            .find(|(_, ty)| ty.name.as_deref() == Some("Constants"))
            .unwrap();
        let naga::TypeInner::Struct { members, .. } = &constants.inner else {
            panic!("Constants is a struct");
        };
        let offset = |name: &str| {
            members
                .iter()
                .find(|member| member.name.as_deref() == Some(name))
                .unwrap()
                .offset as usize
        };
        let register_words = FLOAT_CONSTANTS.1 + FETCH.1 + BOOLS.1 + LOOPS.1;
        assert_eq!(offset("index_offset"), register_words * 4);
        assert_eq!(
            offset("texture_swizzles"),
            (register_words + DRAW_WORDS) * 4
        );
        assert_eq!(
            offset("alpha_function"),
            (register_words + DRAW_WORDS + TEXTURE_CONSTANTS) * 4
        );
        assert_eq!(
            offset("pixel_parameters"),
            (register_words + DRAW_WORDS + TEXTURE_CONSTANTS + ALPHA_TEST_WORDS) * 4
        );
        assert_eq!(u64::from(layouter[handle].size), CONSTANTS_SIZE);
    }

    #[test]
    fn the_depth_aliasing_shader_is_valid_wgsl() {
        if let Err(error) = translate::validate(DEPTH_ALIASING_SHADER) {
            panic!("{error}");
        }
    }

    #[test]
    fn culling_follows_pa_su_sc_mode_cntl_for_triangles_only() {
        use wgpu::PrimitiveTopology::TriangleList;
        // Back faces culled, clockwise in front, as most of Pure's 3D draws ask.
        let cull_back_clockwise = 0x0021_8006;
        assert_eq!(
            culling(cull_back_clockwise, VertexEntry::Indexed, TriangleList),
            Culling::Faces(wgpu::Face::Back, wgpu::FrontFace::Cw)
        );
        assert_eq!(
            culling(
                cull_back_clockwise,
                VertexEntry::RectangleList,
                TriangleList
            ),
            Culling::Nothing
        );
        assert_eq!(
            culling(CULL_FRONT | CULL_BACK, VertexEntry::Direct, TriangleList),
            Culling::Everything
        );
    }

    #[test]
    fn pixel_parameters_go_to_the_register_sq_context_misc_names_when_enabled() {
        const PARAMETERS_ENABLE: u32 = 1 << 18;
        const REGISTER_1: u32 = 1 << 8;
        assert_eq!(pixel_parameters_register(PARAMETERS_ENABLE, REGISTER_1), 1);
        assert_eq!(
            pixel_parameters_register(0, REGISTER_1),
            translate::NO_PIXEL_PARAMETERS
        );
    }

    #[test]
    fn disabled_clipping_is_passed_with_the_w_formats() {
        const CLIP_DISABLE: u32 = 1 << 16;
        // Direct3D's clears: XY and Z already divided by W, clipping off.
        assert_eq!(position_format(0x300, CLIP_DISABLE), 0b1011);
        assert_eq!(position_format(0x300, 0), 0b0011);
    }

    #[test]
    fn alpha_test_words_follow_rb_colorcontrol() {
        const HALF: u32 = 0x3F00_0000;
        const COMPARE_ALWAYS: u32 = 7;
        const COMPARE_GREATER: u32 = 4;
        // Pure's foliage: greater than the reference, with alpha to coverage, 4x samples.
        assert_eq!(
            alpha_test_words(0xFF00_001C, HALF, (1, 1)),
            [COMPARE_GREATER, HALF, 0x1FF, 0b101]
        );
        // A comparison is set, but the test is off.
        assert_eq!(
            alpha_test_words(0x8700_0004, HALF, (0, 0)),
            [COMPARE_ALWAYS, HALF, 0, 0]
        );
    }

    #[test]
    fn indices_are_read_as_the_gpu_reads_them() {
        const EIGHT_IN_16: u32 = 1;
        // Big-endian 16-bit indices 1, 2, 3 after two bytes of something else.
        let guest = [0xAA, 0xBB, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03];
        assert_eq!(
            host_indices(&guest, 2, 3, false, EIGHT_IN_16, None),
            [1, 2, 3]
        );
    }

    #[test]
    fn a_strips_reset_index_becomes_the_hosts_restart_index() {
        const EIGHT_IN_16: u32 = 1;
        const RESET_INDEX: u32 = 0xFFFF;
        // Big-endian 16-bit indices 1, 0xFFFF, 3.
        let guest = [0x00, 0x01, 0xFF, 0xFF, 0x00, 0x03, 0x00, 0x00];
        assert_eq!(
            host_indices(&guest, 0, 3, false, EIGHT_IN_16, Some(RESET_INDEX)),
            [1, HOST_RESTART_INDEX, 3]
        );
        assert_eq!(
            host_indices(&guest, 0, 3, false, EIGHT_IN_16, None),
            [1, RESET_INDEX, 3]
        );
    }

    #[test]
    fn a_lower_tile_resolves_into_the_texture_above_it() {
        const BASE: u32 = 0x01CE_8000;
        const SCREEN: (u32, u32) = (1280, 720);
        const TILE_ROWS: u32 = 512;
        let lower_tile = BASE + SCREEN.0 * TILE_ROWS * RESOLVE_DESTINATION_BYTES_PER_TEXEL;
        assert_eq!(rows_into(BASE, SCREEN, lower_tile), Some(TILE_ROWS));
        assert_eq!(rows_into(BASE, SCREEN, BASE), None);
        assert_eq!(rows_into(BASE, SCREEN, lower_tile + 4), None);
        let past_the_end = BASE + SCREEN.0 * SCREEN.1 * RESOLVE_DESTINATION_BYTES_PER_TEXEL;
        assert_eq!(rows_into(BASE, SCREEN, past_the_end), None);
    }

    #[test]
    fn a_range_read_by_several_draws_is_placed_once() {
        const VERTICES: u32 = 0x1000;
        const INDICES: u32 = 0x1000_0000;
        const CAPACITY: usize = 0x100;
        let memory = GuestMemory::reserve().unwrap();
        let identity = |physical| Some(physical);
        let mut batch = BatchMemory::default();
        // The index buffer starts mid-word, as 16-bit index buffers may.
        let first = batch
            .place(
                &memory,
                &identity,
                &[(VERTICES, 8), (INDICES + 2, 2)],
                CAPACITY,
            )
            .unwrap();
        let second = batch
            .place(&memory, &identity, &[(VERTICES, 8)], CAPACITY)
            .unwrap();
        assert_eq!(first, [0, 10]);
        assert_eq!(second, [0]);
        assert_eq!(batch.bytes.len(), 12);
    }

    #[test]
    fn ranges_inside_a_placed_range_share_it_until_commands_are_submitted() {
        const BUFFER: u32 = 0x2000;
        const BUFFER_SIZE: u32 = 0x100;
        const STEP: u32 = 0x40;
        const CAPACITY: usize = 0x1000;
        let memory = GuestMemory::reserve().unwrap();
        let identity = |physical| Some(physical);
        let mut batch = BatchMemory::default();
        let place = |batch: &mut BatchMemory, offset: u32| {
            let range = (BUFFER + offset, BUFFER_SIZE - offset);
            batch.place(&memory, &identity, &[range], CAPACITY).unwrap()[0]
        };
        assert_eq!(place(&mut batch, 0), 0);
        assert_eq!(place(&mut batch, STEP), STEP);
        assert_eq!(place(&mut batch, 2 * STEP + 2), 2 * STEP + 2);
        assert_eq!(batch.bytes.len(), BUFFER_SIZE as usize);
        batch.forget_placed();
        assert_eq!(place(&mut batch, STEP), BUFFER_SIZE);
        assert_eq!(batch.bytes.len(), (2 * BUFFER_SIZE - STEP) as usize);
    }

    #[test]
    fn the_standard_viewport_maps_guest_clip_space_to_host_clip_space() {
        // Direct3D's 1280x720 viewport: x * 640 + 640, y * -360 + 360, z unchanged.
        const ALL_ENABLED: u32 = 0x3F;
        let viewport = [640.0, 640.0, -360.0, 360.0, 1.0, 0.0];
        let (scale, offset) =
            position_transform(ALL_ENABLED, viewport, [1280.0, 720.0], false, (0, 0));
        assert_eq!(scale, [1.0, 1.0, 1.0]);
        assert_eq!(offset, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn screen_space_positions_are_pixels() {
        // No viewport transform: guest X and Y are pixels; pixel (0, 0)'s centre is at 0.5.
        let (scale, offset) = position_transform(0, [0.0; 6], [1280.0, 720.0], true, (0, 0));
        let host = |pixel: [f32; 2]| {
            [
                pixel[0] * scale[0] + offset[0],
                pixel[1] * scale[1] + offset[1],
            ]
        };
        let close =
            |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6;
        assert!(close(host([-0.5, -0.5]), [-1.0, 1.0]));
        assert!(close(host([1279.5, 719.5]), [1.0, -1.0]));
    }

    #[test]
    fn guest_primitives_expand_to_host_triangles() {
        assert_eq!(
            host_primitive(QUAD_LIST, 8),
            Some((
                VertexEntry::QuadList,
                wgpu::PrimitiveTopology::TriangleList,
                12
            ))
        );
        assert_eq!(
            host_primitive(RECTANGLE_LIST, 3),
            Some((
                VertexEntry::RectangleList,
                wgpu::PrimitiveTopology::TriangleList,
                6
            ))
        );
        assert_eq!(
            host_primitive(TRIANGLE_FAN, 5),
            Some((
                VertexEntry::TriangleFan,
                wgpu::PrimitiveTopology::TriangleList,
                9
            ))
        );
    }

    #[test]
    fn replacing_blend_needs_no_host_blending() {
        // Source alpha, add, one minus source alpha; the same for alpha.
        const ALPHA_BLEND: u32 = 0x0706_0706;
        assert_eq!(blend_state(BLEND_REPLACE), None);
        let blend = blend_state(ALPHA_BLEND).unwrap();
        assert_eq!(blend.color.src_factor, wgpu::BlendFactor::SrcAlpha);
        assert_eq!(blend.color.dst_factor, wgpu::BlendFactor::OneMinusSrcAlpha);
    }
}
