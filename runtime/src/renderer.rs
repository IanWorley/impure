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

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use bevy::log::{info, warn};

use crate::command_processor::read_register;
use crate::edram::{self, Rectangle, Surface};
use crate::gpu::{
    Draw, EDRAM_MODE_COPY, EDRAM_MODE_MASK, FETCH_CONSTANT_WORDS, FETCH_CONSTANTS, IndexBuffer,
    ShaderStage, TextureDescriptor, VIEWPORT_REGISTERS, registers,
};
use crate::memory::GuestMemory;
use crate::physical_memory;
use crate::shader::{Instruction, Program};
use crate::texture::{self, HostFormat};
use crate::translate::{self, VertexEntry};

/// The renderer, once Bevy's device is available.
static RENDERER: Mutex<Option<Renderer>> = Mutex::new(None);

/// Starts rendering on the device Bevy created.
pub fn install(device: wgpu::Device, queue: wgpu::Queue) {
    *RENDERER.lock().unwrap() = Some(Renderer::new(device, queue));
}

/// The texture the window shows; swaps copy the front buffer into it.
static DISPLAY: Mutex<Option<wgpu::Texture>> = Mutex::new(None);

pub fn set_display(texture: wgpu::Texture) {
    *DISPLAY.lock().unwrap() = Some(texture);
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
/// The per-draw words after the register blocks (`memory_base` to `position_offset_z`).
const DRAW_WORDS: usize = 12;
const CONSTANTS_WORDS: usize =
    FLOAT_CONSTANTS.1 + FETCH.1 + BOOLS.1 + LOOPS.1 + DRAW_WORDS + TEXTURE_CONSTANTS;
/// WGSL rounds the struct's size up to its 16-byte alignment.
const CONSTANTS_SIZE: u64 = (CONSTANTS_WORDS * 4).next_multiple_of(16) as u64;
const WORD_SIZE: u32 = 4;
/// How long a swap waits for the previous frame's GPU work before going on regardless.
const FRAME_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// The largest window of guest memory one draw uploads; draws needing more are skipped.
const MAXIMUM_MEMORY_WINDOW: u32 = 64 << 20;
/// Render target and copy register fields.
const COLOR_BASE_MASK: u32 = 0xFFF;
const COLOR_FORMAT_SHIFT: u32 = 16;
const COLOR_FORMAT_MASK: u32 = 0xF;
/// xenos::ColorRenderTargetFormat values stored as 8-bit RGBA.
const COLOR_FORMAT_8_8_8_8: u32 = 0;
const COLOR_FORMAT_8_8_8_8_GAMMA: u32 = 1;
const COPY_SOURCE_MASK: u32 = 0x7;
/// RB_COPY_CONTROL's source for depth; 0-3 are the colour targets.
const COPY_SOURCE_DEPTH: u32 = 4;
const COPY_COLOR_CLEAR: u32 = 1 << 8;
const COPY_DEPTH_CLEAR: u32 = 1 << 9;
/// RB_DEPTH_CLEAR: 24-bit depth above 8-bit stencil.
const DEPTH_CLEAR_SHIFT: u32 = 8;
const DEPTH_CLEAR_MAXIMUM: f32 = 0xFF_FFFF as f32;
const STENCIL_MASK: u32 = 0xFF;
/// RB_STENCILREFMASK: reference, read mask and write mask, a byte each.
const STENCIL_READ_MASK_SHIFT: u32 = 8;
const STENCIL_WRITE_MASK_SHIFT: u32 = 16;
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
/// xenos::ColorFormat k_8_8_8_8, the only resolve destination format handled.
const COPY_FORMAT_8_8_8_8: u32 = 6;
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
    format: u32,
    width: u32,
    height: u32,
    pitch: u32,
    tiled: bool,
    endian: u32,
}

impl From<&TextureDescriptor> for TextureKey {
    fn from(descriptor: &TextureDescriptor) -> Self {
        Self {
            base: descriptor.base,
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
    /// A hash of the guest bytes it was decoded from.
    content: u64,
    /// The frame it was last compared with guest memory.
    checked_frame: u64,
}

/// A translated shader, ready for pipelines.
struct CompiledShader {
    module: wgpu::ShaderModule,
    textures: BTreeSet<u32>,
    vertex_fetch_constants: BTreeSet<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipelineKey {
    vertex_shader: u64,
    pixel_shader: u64,
    entry: VertexEntry,
    topology: wgpu::PrimitiveTopology,
    blend_control: u32,
    color_mask: u32,
    /// Depth and stencil testing, if the draw does any.
    depth: Option<DepthState>,
}

/// RB_DEPTHCONTROL and the stencil masks of RB_STENCILREFMASK (its reference is set per draw).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DepthState {
    control: u32,
    stencil_masks: u32,
}

/// An EDRAM surface: where it starts in EDRAM (in tiles) and its width in samples. Every
/// colour surface is 8-bit RGBA on the host so far.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RenderTargetKey {
    base: u32,
    width: u32,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    constants: wgpu::Buffer,
    memory: wgpu::Buffer,
    constants_layout: wgpu::BindGroupLayout,
    /// None for shaders that did not translate to valid WGSL.
    shaders: HashMap<(ShaderStage, u64), Option<CompiledShader>>,
    /// Pipelines and the texture bind group layout they were made with; None if creation failed.
    pipelines: HashMap<PipelineKey, Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout)>>,
    render_targets: HashMap<RenderTargetKey, wgpu::Texture>,
    depth_targets: HashMap<RenderTargetKey, wgpu::Texture>,
    /// Resolved textures, by their physical destination address.
    resolved: HashMap<u32, wgpu::Texture>,
    textures: HashMap<TextureKey, CachedTexture>,
    placeholder: Option<wgpu::Texture>,
    samplers: HashMap<(u32, u32, u32, u32), wgpu::Sampler>,
    /// What has been reported as not reproduced, so each is reported once.
    reported: BTreeSet<String>,
    presented: u64,
    /// The last presented frame's work, which the next swap waits for.
    frame_in_flight: Option<wgpu::SubmissionIndex>,
    /// Copies a front buffer to the display, created on first use.
    blit: Option<wgpu::RenderPipeline>,
    /// Averages EDRAM samples into a resolve destination, and its sample shifts.
    resolve: Option<ResolvePipeline>,
    resolve_depth: Option<ResolvePipeline>,
    resolve_parameters: wgpu::Buffer,
}

impl Renderer {
    fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let storage = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let constants_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("guest constants and memory"),
            entries: &[storage(0), storage(1)],
        });
        let constants = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("guest constants"),
            size: CONSTANTS_SIZE,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let memory = storage_buffer(&device, WORD_SIZE.into());
        let resolve_parameters = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("resolve parameters"),
            size: RESOLVE_PARAMETERS_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        info!(target: "renderer", "rendering guest draws with wgpu");
        Self {
            device,
            queue,
            constants,
            memory,
            constants_layout,
            shaders: HashMap::new(),
            pipelines: HashMap::new(),
            render_targets: HashMap::new(),
            depth_targets: HashMap::new(),
            resolved: HashMap::new(),
            textures: HashMap::new(),
            placeholder: None,
            samplers: HashMap::new(),
            reported: BTreeSet::new(),
            presented: 0,
            frame_in_flight: None,
            blit: None,
            resolve: None,
            resolve_depth: None,
            resolve_parameters,
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

    fn present(&mut self, front_buffer: u32) {
        let display = DISPLAY.lock().unwrap().clone();
        let (Some(display), Some(source)) = (display, self.resolved.get(&front_buffer)) else {
            return;
        };
        let (width, height) = (
            source.width().min(display.width()),
            source.height().min(display.height()),
        );
        let source_view = source.create_view(&Default::default());
        // Rendered through a plain view: the bytes are already gamma-encoded.
        let display_view = display.create_view(&wgpu::TextureViewDescriptor {
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            ..Default::default()
        });
        let blit = self.blit.get_or_insert_with(|| create_blit(&self.device));
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
        // behind the title, and the window's own frames queue up behind the backlog.
        let submission = self.queue.submit([encoder.finish()]);
        if let Some(previous) = self.frame_in_flight.replace(submission) {
            let _ = self.device.poll(wgpu::PollType::Wait {
                submission_index: Some(previous),
                timeout: Some(FRAME_WAIT_LIMIT),
            });
        }
        self.presented += 1;
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
        let surface = Surface::decode(register(registers::RB_SURFACE_INFO));
        let color_info = register(registers::RB_COLOR_INFO);
        let format = (color_info >> COLOR_FORMAT_SHIFT) & COLOR_FORMAT_MASK;
        if !depth_only && !matches!(format, COLOR_FORMAT_8_8_8_8 | COLOR_FORMAT_8_8_8_8_GAMMA) {
            return self.report(format!("render target format {format}"));
        }
        let Some((entry, topology, host_vertices)) =
            host_primitive(draw.initiator.primitive, draw.initiator.index_count)
        else {
            return self.report(format!("primitive type {:#x}", draw.initiator.primitive));
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
            &RenderTargetKey {
                base: color_info & COLOR_BASE_MASK,
                width: size.0,
            },
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
            color_mask: if depth_only {
                0
            } else {
                register(registers::RB_COLOR_MASK) & COLOR_MASK_TARGET_0
            },
            depth,
        };
        let textures: BTreeSet<u32> = self.shaders[&(ShaderStage::Vertex, vertex)]
            .as_ref()
            .unwrap()
            .textures
            .union(
                &self.shaders[&(ShaderStage::Pixel, pixel)]
                    .as_ref()
                    .unwrap()
                    .textures,
            )
            .copied()
            .collect();
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
        let indices = draw.indices.map(|indices| {
            let physical = physical_memory::physical_address(indices.address);
            ranges.push((physical, indices.words * WORD_SIZE));
            (physical, indices)
        });
        let Some((window_base, window)) = memory_window(memory, view, &ranges) else {
            return self.report("draws reading more than 64 MiB of guest memory".into());
        };
        self.upload_memory(&window);

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
            memory_base: window_base / WORD_SIZE,
            indices: indices.map(|(physical, indices)| (physical - window_base, indices)),
            index_bias: register(registers::VGT_INDX_OFFSET),
            position_format: (register(registers::PA_CL_VTE_CNTL) >> VTE_FORMAT_SHIFT)
                & VTE_FORMAT_MASK,
            scale,
            offset,
            indices_are_32_bit: draw.initiator.indices_are_32_bit,
        };
        let mut swizzles = [0; TEXTURE_CONSTANTS];
        let mut texture_entries = Vec::new();
        for &constant in &textures {
            let first = FETCH_CONSTANTS + constant * FETCH_CONSTANT_WORDS;
            let words = std::array::from_fn(|word| register(first + word as u32));
            let descriptor = TextureDescriptor::decode(words);
            swizzles[constant as usize] = descriptor.swizzle;
            let texture = self.upload_texture(memory, view, &descriptor);
            let sampler = self.sampler(&descriptor);
            texture_entries.push((constant, texture, sampler));
        }
        let words = constants_words(memory, &draw_words, &swizzles);
        self.queue
            .write_buffer(&self.constants, 0, words_as_bytes(&words));

        let constants_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.constants_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.constants.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.memory.as_entire_binding(),
                },
            ],
        });
        let views: Vec<(u32, wgpu::TextureView, wgpu::Sampler)> = texture_entries
            .into_iter()
            .map(|(constant, texture, sampler)| {
                (constant, texture.create_view(&Default::default()), sampler)
            })
            .collect();
        let mut entries = Vec::new();
        for (constant, view, sampler) in &views {
            entries.push(wgpu::BindGroupEntry {
                binding: constant * 2,
                resource: wgpu::BindingResource::TextureView(view),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: constant * 2 + 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            });
        }
        let texture_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &texture_layout,
            entries: &entries,
        });

        let target_view = target.create_view(&Default::default());
        let depth_view = depth_target.map(|texture| texture.create_view(&Default::default()));
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            // The same four slots as the pipeline's targets.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("guest draw"),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
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
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &constants_group, &[]);
            pass.set_bind_group(1, &texture_group, &[]);
            pass.set_scissor_rect(
                scissor.left,
                scissor.top,
                scissor.right - scissor.left,
                scissor.bottom - scissor.top,
            );
            pass.set_stencil_reference(stencil & STENCIL_MASK);
            pass.draw(0..host_vertices, 0..1);
        }
        self.queue.submit([encoder.finish()]);
    }

    /// The host texture standing in for an EDRAM colour surface.
    fn render_target(&mut self, key: &RenderTargetKey, size: (u32, u32)) -> wgpu::Texture {
        let device = &self.device;
        self.render_targets
            .entry(*key)
            .or_insert_with(|| {
                surface_texture(
                    device,
                    size,
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureUsages::TEXTURE_BINDING,
                )
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
        textures: &BTreeSet<u32>,
    ) -> Option<(wgpu::RenderPipeline, wgpu::BindGroupLayout)> {
        if let Some(pipeline) = self.pipelines.get(&key) {
            return pipeline.clone();
        }
        let blend = blend_state(key.blend_control);
        if blend.is_none() && key.blend_control != BLEND_REPLACE {
            self.report(format!("blend control {:#x}", key.blend_control));
        }
        let mut entries = Vec::new();
        for &constant in textures {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: constant * 2,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
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
                format: wgpu::TextureFormat::Rgba8Unorm,
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

    fn upload_memory(&mut self, window: &[u8]) {
        let size = window.len() as u64;
        if self.memory.size() < size {
            self.memory = storage_buffer(&self.device, size.next_power_of_two());
        }
        self.queue.write_buffer(&self.memory, 0, window);
    }

    /// A texture's base level as it is in guest memory now. Each texture is read at most once
    /// a frame, and decoded and uploaded again only when its bytes have changed.
    fn upload_texture(
        &mut self,
        memory: &GuestMemory,
        view: View,
        descriptor: &TextureDescriptor,
    ) -> wgpu::Texture {
        // Render to texture: what a resolve wrote exists only on the host.
        if let Some(resolved) = self.resolved.get(&descriptor.base)
            && (resolved.width(), resolved.height()) == (descriptor.width, descriptor.height)
        {
            return resolved.clone();
        }
        let key = TextureKey::from(descriptor);
        if let Some(cached) = self.textures.get(&key)
            && cached.checked_frame == self.presented
        {
            return cached.texture.clone();
        }
        let Some(size) = texture::guest_size(descriptor) else {
            self.report(format!("texture format {}", descriptor.format));
            return self.placeholder_texture();
        };
        let Some(address) = view(descriptor.base) else {
            self.report("textures outside allocated memory".into());
            return self.placeholder_texture();
        };
        let mut guest = vec![0; size as usize];
        memory.read_bytes(address, &mut guest);
        let mut hasher = DefaultHasher::new();
        guest.hash(&mut hasher);
        let content = hasher.finish();
        if let Some(cached) = self.textures.get_mut(&key)
            && cached.content == content
        {
            cached.checked_frame = self.presented;
            return cached.texture.clone();
        }
        let Some(decoded) = texture::decode(descriptor, &guest) else {
            self.report(format!("texture format {}", descriptor.format));
            return self.placeholder_texture();
        };
        let (format, bytes_per_texel) = match decoded.format {
            HostFormat::R8 => (wgpu::TextureFormat::R8Unorm, 1),
            HostFormat::Rg8 => (wgpu::TextureFormat::Rg8Unorm, 2),
            HostFormat::Rgba8 => (wgpu::TextureFormat::Rgba8Unorm, 4),
        };
        let texture = match self.textures.get(&key) {
            Some(cached) if cached.texture.format() == format => cached.texture.clone(),
            _ => self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("guest texture"),
                size: wgpu::Extent3d {
                    width: descriptor.width,
                    height: descriptor.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }),
        };
        self.queue.write_texture(
            texture.as_image_copy(),
            &decoded.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(descriptor.width * bytes_per_texel),
                rows_per_image: Some(descriptor.height),
            },
            texture.size(),
        );
        self.textures.insert(
            key,
            CachedTexture {
                texture: texture.clone(),
                content,
                checked_frame: self.presented,
            },
        );
        texture
    }

    /// Opaque white, for textures that cannot be uploaded.
    fn placeholder_texture(&mut self) -> wgpu::Texture {
        if let Some(texture) = &self.placeholder {
            return texture.clone();
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("placeholder texture"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            texture.as_image_copy(),
            &[u8::MAX; 4],
            wgpu::TexelCopyBufferLayout::default(),
            texture.size(),
        );
        self.placeholder = Some(texture.clone());
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
        let surface = Surface::decode(register(registers::RB_SURFACE_INFO));
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
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let source = control & COPY_SOURCE_MASK;
        match (COLOR_INFO_REGISTERS.get(source as usize), rectangle) {
            (Some(&info_register), Some(rectangle)) => {
                let key = RenderTargetKey {
                    base: register(info_register) & COLOR_BASE_MASK,
                    width: size.0,
                };
                self.resolve_color(&mut encoder, memory, key, surface, rectangle);
            }
            (None, Some(rectangle)) if source == COPY_SOURCE_DEPTH => {
                let key = RenderTargetKey {
                    base: register(registers::RB_DEPTH_INFO) & COLOR_BASE_MASK,
                    width: size.0,
                };
                self.resolve_depth(&mut encoder, memory, key, surface, rectangle);
            }
            _ => {}
        }
        if control & COPY_COLOR_CLEAR != 0
            && let Some(target) = self.render_targets.get(&RenderTargetKey {
                base: register(registers::RB_COLOR_INFO) & COLOR_BASE_MASK,
                width: size.0,
            })
        {
            let view = target.create_view(&Default::default());
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("resolve colour clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(register(registers::RB_COLOR_CLEAR))),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        if control & COPY_DEPTH_CLEAR != 0
            && let Some(target) = self.depth_targets.get(&RenderTargetKey {
                base: register(registers::RB_DEPTH_INFO) & COLOR_BASE_MASK,
                width: size.0,
            })
        {
            let clear = register(registers::RB_DEPTH_CLEAR);
            let view = target.create_view(&Default::default());
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("resolve depth clear"),
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(
                            (clear >> DEPTH_CLEAR_SHIFT) as f32 / DEPTH_CLEAR_MAXIMUM,
                        ),
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
        self.queue.submit([encoder.finish()]);
    }

    fn resolve_color(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        memory: &GuestMemory,
        key: RenderTargetKey,
        surface: Surface,
        rectangle: Rectangle,
    ) {
        let info = read_register(memory, registers::RB_COPY_DEST_INFO);
        let format = (info >> COPY_DEST_FORMAT_SHIFT) & COPY_DEST_FORMAT_MASK;
        if format != COPY_FORMAT_8_8_8_8 {
            return self.report(format!("resolves to format {format}"));
        }
        let Some(source) = self.render_targets.get(&key).cloned() else {
            return;
        };
        let pipeline = self
            .resolve
            .get_or_insert_with(|| {
                create_resolve(
                    &self.device,
                    RESOLVE_SHADER,
                    wgpu::TextureFormat::Rgba8Unorm,
                )
            })
            .clone();
        let view = source.create_view(&Default::default());
        let size = (source.width(), source.height());
        self.resolve_into(encoder, memory, &pipeline, &view, size, surface, rectangle);
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
        let view = source.create_view(&wgpu::TextureViewDescriptor {
            aspect: wgpu::TextureAspect::DepthOnly,
            ..Default::default()
        });
        let size = (source.width(), source.height());
        self.resolve_into(encoder, memory, &pipeline, &view, size, surface, rectangle);
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
    ) {
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
            return;
        }
        let destination_address = register(registers::RB_COPY_DEST_BASE);
        let destination = match self.resolved.get(&destination_address) {
            Some(texture)
                if (texture.width(), texture.height(), texture.format())
                    == (width, height, pipeline.format) =>
            {
                texture.clone()
            }
            _ => {
                let texture = surface_texture(
                    &self.device,
                    (width, height),
                    pipeline.format,
                    wgpu::TextureUsages::TEXTURE_BINDING,
                );
                self.resolved.insert(destination_address, texture.clone());
                texture
            }
        };
        self.queue.write_buffer(
            &self.resolve_parameters,
            0,
            words_as_bytes(&[sample_x, sample_y, 0, 0]),
        );
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
                    resource: self.resolve_parameters.as_entire_binding(),
                },
            ],
        });
        let destination_view = destination.create_view(&Default::default());
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
    }
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

/// Resolves average each destination pixel's samples: 2^x by 2^y of them.
const RESOLVE_SHADER: &str = "\
struct Parameters {
    sample_shift: vec2<u32>,
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
    let first = vec2<u32>(position.xy) << parameters.sample_shift;
    let count = vec2<u32>(1u) << parameters.sample_shift;
    var sum = vec4<f32>(0.0);
    for (var y = 0u; y < count.y; y += 1u) {
        for (var x = 0u; x < count.x; x += 1u) {
            sum += textureLoad(source, first + vec2<u32>(x, y), 0);
        }
    }
    return sum / f32(count.x * count.y);
}
";
/// Depth resolves copy each pixel's first sample.
const RESOLVE_DEPTH_SHADER: &str = "\
struct Parameters {
    sample_shift: vec2<u32>,
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
    let depth = textureLoad(source, vec2<u32>(position.xy) << parameters.sample_shift, 0);
    return vec4<f32>(depth, 0.0, 0.0, 1.0);
}
";
/// The parameters' uniform size, rounded up to 16 bytes.
const RESOLVE_PARAMETERS_SIZE: u64 = 16;

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
";
/// One triangle covering the viewport.
const FULL_SCREEN_TRIANGLE: u32 = 3;

fn create_blit(device: &wgpu::Device) -> wgpu::RenderPipeline {
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
            entry_point: Some("fragment"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn storage_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("guest memory window"),
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

/// The per-draw values after the register blocks in the constants buffer.
struct DrawWords {
    /// Physical dword address of the memory window.
    memory_base: u32,
    /// The index buffer's byte offset in the window, and its description.
    indices: Option<(u32, IndexBuffer)>,
    indices_are_32_bit: bool,
    index_bias: u32,
    position_format: u32,
    scale: [f32; 3],
    offset: [f32; 3],
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
        words.extend((0..count as u32).map(|index| read_register(memory, first + index)));
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
        draw.memory_base,
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
    words.resize(CONSTANTS_SIZE as usize / WORD_SIZE as usize, 0);
    words
}

fn words_as_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and u8 has no alignment requirement.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), std::mem::size_of_val(words)) }
}

/// Guest memory covering every (physical address, length) range, as raw bytes in guest order
/// (which the shaders byte-swap as the GPU does), with its physical base address. None if the
/// ranges span more than the window allows.
fn memory_window(
    memory: &GuestMemory,
    view: View,
    ranges: &[(u32, u32)],
) -> Option<(u32, Vec<u8>)> {
    let ranges: Vec<(u32, u32)> = ranges
        .iter()
        .copied()
        .filter(|&(_, length)| length > 0)
        .collect();
    let Some(base) = ranges.iter().map(|&(start, _)| start).min() else {
        return Some((0, vec![0; WORD_SIZE as usize]));
    };
    let base = base & !(WORD_SIZE - 1);
    let end = ranges.iter().map(|&(start, length)| start + length).max()?;
    let size = (end - base).next_multiple_of(WORD_SIZE);
    if size > MAXIMUM_MEMORY_WINDOW {
        return None;
    }
    let mut window = vec![0; size as usize];
    for (start, length) in ranges {
        if let Some(address) = view(start) {
            let offset = (start - base) as usize;
            memory.read_bytes(address, &mut window[offset..offset + length as usize]);
        }
    }
    Some((base, window))
}

/// RB_COLOR_CLEAR holds the clear colour as ARGB bytes.
fn clear_color(value: u32) -> wgpu::Color {
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
        assert_eq!(offset("memory_base"), register_words * 4);
        assert_eq!(
            offset("texture_swizzles"),
            (register_words + DRAW_WORDS) * 4
        );
        assert_eq!(u64::from(layouter[handle].size), CONSTANTS_SIZE);
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
