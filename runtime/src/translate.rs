//! Translates Xenos shader microcode to WGSL.
//!
//! The generated shader interprets the Xenos control flow as a `switch` on a control-flow
//! index inside a loop, so jumps, calls and loops need no structuring. Constants come from one
//! storage buffer laid out as [`CONSTANTS_LAYOUT`] describes, and vertex fetches read guest
//! memory as raw little-endian words from a second, byte-swapping them as the fetch constant
//! says, as the console's GPU does. Semantics follow Xenia's `ucode.h`.
//!
//! Anything not translated faithfully (texture fetches, for now) is listed in
//! [`Translation::unsupported`] so the log says why a draw may look wrong.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;

use crate::gpu::ShaderStage;
use crate::shader::{
    Alu, Condition, ControlFlowKind, Instruction, Program, Source, TextureFetch, VertexFetch,
};

const TEMPORARY_REGISTERS: u32 = 64;
const INTERPOLATORS: u32 = 16;
const COLOR_TARGETS: u32 = 4;
/// Pixel shaders read float constants 256-511.
const FLOAT_CONSTANTS_PER_STAGE: u32 = 256;
const LOOP_DEPTH: u32 = 4;
const CALL_DEPTH: u32 = 4;

/// Export registers an ALU instruction can write.
const EXPORT_DEPTH: u32 = 61;
const EXPORT_POSITION: u32 = 62;
const EXPORT_POINT_SIZE: u32 = 63;

/// Vertex formats (xenos::VertexFormat).
const FORMAT_8_8_8_8: u32 = 6;
const FORMAT_2_10_10_10: u32 = 7;
const FORMAT_10_11_11: u32 = 16;
const FORMAT_11_11_10: u32 = 17;
const FORMAT_16_16: u32 = 25;
const FORMAT_16_16_16_16: u32 = 26;
const FORMAT_16_16_FLOAT: u32 = 31;
const FORMAT_16_16_16_16_FLOAT: u32 = 32;
const FORMAT_32: u32 = 33;
const FORMAT_32_32: u32 = 34;
const FORMAT_32_32_32_32: u32 = 35;
const FORMAT_32_FLOAT: u32 = 36;
const FORMAT_32_32_FLOAT: u32 = 37;
const FORMAT_32_32_32_32_FLOAT: u32 = 38;
const FORMAT_32_32_32_FLOAT: u32 = 57;

/// A fetch destination selector that leaves the component unwritten.
const FETCH_KEEP: u32 = 7;
const FETCH_ZERO: u32 = 4;
const FETCH_ONE: u32 = 5;

/// The bindings every translated shader declares, and the layout the renderer fills (see
/// `renderer::Constants`): float constants (vertex 0-255, pixel 256-511), the fetch constants
/// (six words per slot; the renderer replaces vertex fetch addresses with byte offsets in
/// `memory`), the bool and loop constants, then per-draw values. Textures are in
/// group 1: texture constant `n` at binding `2n`, its sampler at `2n + 1`.
pub const CONSTANTS_LAYOUT: &str = "\
struct Constants {
    float_constants: array<vec4<f32>, 512>,
    fetch: array<u32, 192>,
    bools: array<u32, 8>,
    loops: array<u32, 32>,
    // The index buffer: its byte offset in `memory`, its index size in bits (0 without one), its
    // byte order, and the bias added to every index.
    index_offset: u32,
    index_bits: u32,
    index_endian: u32,
    index_bias: u32,
    // PA_CL_VTE_CNTL's formats: XY divided by W (1), Z divided by W (2), W is not 1/W (4).
    position_format: u32,
    // Guest position to host clip space: xyz * scale + w * offset.
    position_scale_x: f32,
    position_scale_y: f32,
    position_scale_z: f32,
    position_offset_x: f32,
    position_offset_y: f32,
    position_offset_z: f32,
    // Each texture constant's swizzle: three bits per component, a component or 0 (4) or 1 (5).
    texture_swizzles: array<u32, 32>,
}
@group(0) @binding(0) var<storage, read> constants: Constants;
@group(0) @binding(1) var<storage, read> memory: array<u32>;
";

/// Helpers the generated code calls.
const HELPERS: &str = "\
const FLT_MAX: f32 = 3.40282347e38;

// The GPU's byte orders: none, 8-in-16, 8-in-32 and 16-in-32.
fn endian_swap(value: u32, mode: u32) -> u32 {
    var swapped = value;
    if mode == 1u || mode == 2u {
        swapped = ((swapped & 0x00FF00FFu) << 8u) | ((swapped >> 8u) & 0x00FF00FFu);
    }
    if mode == 2u || mode == 3u {
        swapped = (swapped << 16u) | (swapped >> 16u);
    }
    return swapped;
}

fn is_finite(value: f32) -> bool {
    return abs(value) <= FLT_MAX;
}

fn clamp_infinity(value: f32, replacement: f32) -> f32 {
    return select(value, sign(value) * replacement, !is_finite(value) && value == value);
}

// The console's half floats have no infinities or NaNs: exponent 31 extends the range.
fn half_to_float(half: u32) -> f32 {
    let negative = (half & 0x8000u) != 0u;
    let exponent = (half >> 10u) & 0x1Fu;
    let mantissa = half & 0x3FFu;
    if exponent == 0u {
        // Denormals: mantissa * 2^-24.
        return select(1.0, -1.0, negative) * f32(mantissa) * 5.9604645e-8;
    }
    return bitcast<f32>(((half & 0x8000u) << 16u) | ((exponent + 112u) << 23u) | (mantissa << 13u));
}

fn unpack_halves(word: u32) -> vec2<f32> {
    return vec2<f32>(half_to_float(word & 0xFFFFu), half_to_float(word >> 16u));
}

fn swizzle_texel(texel: vec4<f32>, swizzle: u32) -> vec4<f32> {
    var result: vec4<f32>;
    for (var component = 0u; component < 4u; component += 1u) {
        let selector = (swizzle >> (3u * component)) & 7u;
        result[component] = select(select(texel[min(selector, 3u)], 0.0, selector == 4u), 1.0, selector == 5u);
    }
    return result;
}

// The vertex index the guest sees: from the index buffer if there is one, plus the bias.
fn guest_index(index: u32) -> u32 {
    switch constants.index_bits {
        case 16u: {
            let byte = constants.index_offset + index * 2u;
            let word = endian_swap(memory[byte / 4u], constants.index_endian);
            return ((word >> ((byte & 2u) * 8u)) & 0xFFFFu) + constants.index_bias;
        }
        case 32u: {
            let word = memory[constants.index_offset / 4u + index];
            return endian_swap(word, constants.index_endian) + constants.index_bias;
        }
        default: {
            return index + constants.index_bias;
        }
    }
}

// The guest position in host clip space, after undoing the formats PA_CL_VTE_CNTL describes.
fn host_position(guest: vec4<f32>) -> vec4<f32> {
    var position = guest;
    if (constants.position_format & 4u) == 0u {
        position.w = 1.0 / position.w;
    }
    if (constants.position_format & 1u) != 0u {
        position.x *= position.w;
        position.y *= position.w;
    }
    if (constants.position_format & 2u) != 0u {
        position.z *= position.w;
    }
    let scale = vec3<f32>(constants.position_scale_x, constants.position_scale_y, constants.position_scale_z);
    let offset = vec3<f32>(constants.position_offset_x, constants.position_offset_y, constants.position_offset_z);
    return vec4<f32>(position.xyz * scale + position.w * offset, position.w);
}

fn address_register(value: f32) -> i32 {
    return i32(clamp(value, -256.0, 255.0));
}

// cube: face coordinates of the direction (x, y, z), read as source 0 .zzxy and source 1 .yxzz.
fn cube(a: vec4<f32>) -> vec4<f32> {
    let x = a.z;
    let y = a.w;
    let z = a.x;
    if abs(z) >= abs(x) && abs(z) >= abs(y) {
        return vec4<f32>(-y, select(x, -x, z < 0.0), 2.0 * z, select(4.0, 5.0, z < 0.0));
    }
    if abs(y) >= abs(x) {
        return vec4<f32>(select(z, -z, y < 0.0), x, 2.0 * y, select(2.0, 3.0, y < 0.0));
    }
    return vec4<f32>(-y, select(-z, z, x < 0.0), 2.0 * x, select(0.0, 1.0, x < 0.0));
}
";

/// Vertex shader entry points, one per way of turning the guest's primitives into host
/// triangles, lines or points. Each draws a different number of host vertices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VertexEntry {
    /// One host vertex per guest vertex.
    Direct,
    /// Six host vertices (two triangles) per four guest vertices.
    QuadList,
    /// Three host vertices per triangle of the fan.
    TriangleFan,
    /// Six host vertices per three guest vertices, completing each rectangle's fourth corner.
    RectangleList,
    /// One host vertex per guest index, with the host's index buffer holding the guest's.
    Indexed,
}

impl VertexEntry {
    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "main",
            Self::QuadList => "main_quad_list",
            Self::TriangleFan => "main_triangle_fan",
            Self::RectangleList => "main_rectangle_list",
            Self::Indexed => "main_indexed",
        }
    }
}

/// A translated shader.
pub struct Translation {
    pub wgsl: String,
    /// What the WGSL does not reproduce, one entry per kind.
    pub unsupported: BTreeSet<String>,
    /// The 2D texture constants it samples.
    pub textures: BTreeSet<u32>,
}

/// Checks WGSL the way wgpu will before creating a pipeline.
pub fn validate(wgsl: &str) -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(wgsl).map_err(|error| error.emit_to_string(wgsl))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    )
    .validate(&module)
    .map(drop)
    .map_err(|error| error.emit_to_string(wgsl))
}

pub fn translate(stage: ShaderStage, words: &[u32]) -> Translation {
    let program = Program::decode(words);
    let textures = program
        .instructions
        .iter()
        .filter_map(|(_, instruction)| match instruction {
            Instruction::TextureFetch(fetch) if fetch.dimension() == DIMENSION_2D => {
                Some(fetch.constant())
            }
            _ => None,
        })
        .collect();
    let mut translator = Translator {
        stage,
        code: String::new(),
        unsupported: BTreeSet::new(),
        instructions: program.instructions.iter().copied().collect(),
        scalar_registers: !program
            .instructions
            .iter()
            .any(|(_, instruction)| indexes_registers_relatively(*instruction)),
    };
    translator.translate(&program.control_flow, &textures);
    Translation {
        wgsl: translator.code,
        unsupported: translator.unsupported,
        textures,
    }
}

const COMPONENTS: [char; 4] = ['x', 'y', 'z', 'w'];

/// Writes a line of generated code; formatting into a String cannot fail.
macro_rules! emit {
    ($translator:expr, $($argument:tt)*) => {
        {
            let _ = writeln!($translator.code, $($argument)*);
        }
    };
}

struct Translator {
    stage: ShaderStage,
    code: String,
    unsupported: BTreeSet<String>,
    instructions: HashMap<u32, Instruction>,
    /// Registers are separate variables (`r0`, `r1`, ...) unless something indexes them
    /// relative to aL, which needs an array. Separate variables are much easier for GPU
    /// compilers to keep in registers.
    scalar_registers: bool,
}

impl Translator {
    fn translate(&mut self, control_flow: &[ControlFlowKind], textures: &BTreeSet<u32>) {
        self.code.push_str(CONSTANTS_LAYOUT);
        self.code.push_str(HELPERS);
        for constant in textures {
            emit!(
                self,
                "@group(1) @binding({}) var texture{constant}: texture_2d<f32>;",
                constant * 2
            );
            emit!(
                self,
                "@group(1) @binding({}) var sampler{constant}: sampler;",
                constant * 2 + 1
            );
        }
        self.entry_point_start();
        if is_straight_line(control_flow) {
            self.straight_line(control_flow);
        } else {
            self.interpreted(control_flow);
        }
        self.entry_point_end();
    }

    /// Control flow that only runs execs in order becomes nested blocks: an exec that may end
    /// the shader puts everything after it in its `else`.
    fn straight_line(&mut self, control_flow: &[ControlFlowKind]) {
        let mut open_blocks = 0;
        for kind in control_flow {
            let ControlFlowKind::Exec {
                address,
                count,
                end,
                condition,
                ..
            } = *kind
            else {
                continue;
            };
            emit!(self, "if {} {{", Self::condition(condition));
            self.exec_body(address, count);
            match (end, condition) {
                (false, _) => emit!(self, "}}"),
                (true, Condition::Always) => {
                    emit!(self, "}}");
                    break;
                }
                (true, _) => {
                    emit!(self, "}} else {{");
                    open_blocks += 1;
                }
            }
        }
        for _ in 0..open_blocks {
            emit!(self, "}}");
        }
    }

    /// Any other control flow runs as a `switch` on the control-flow index inside a loop.
    fn interpreted(&mut self, control_flow: &[ControlFlowKind]) {
        emit!(self, "    var pc: u32 = 0u;");
        emit!(self, "    loop {{");
        emit!(
            self,
            "        if pc >= {}u {{ break; }}",
            control_flow.len()
        );
        emit!(self, "        switch pc {{");
        let end = control_flow.len() as u32;
        for (index, kind) in control_flow.iter().enumerate() {
            let index = index as u32;
            emit!(self, "        case {index}u: {{");
            self.control_flow(index, *kind, end);
            emit!(self, "        }}");
        }
        emit!(self, "        default: {{ pc = {end}u; }}");
        emit!(self, "        }}");
        emit!(self, "    }}");
    }

    /// A temporary register by absolute index.
    fn temporary(&self, index: u32) -> String {
        if self.scalar_registers {
            format!("r{index}")
        } else {
            format!("r[{index}]")
        }
    }

    /// A temporary register, relative to aL if `relative` (only with an array of registers).
    fn addressed_temporary(&self, index: u32, relative: bool) -> String {
        if relative {
            format!("r[(u32(aL) + {index}u) % {TEMPORARY_REGISTERS}u]")
        } else {
            self.temporary(index)
        }
    }

    fn exec_body(&mut self, address: u32, count: u32) {
        for slot in 0..count {
            match self.instructions.get(&(address + slot)).copied() {
                Some(instruction) => self.instruction(instruction),
                None => {
                    self.unsupported
                        .insert("exec past the end of the microcode".into());
                }
            }
        }
    }

    /// The entry point's signature and state: registers, the predicate, the address
    /// registers, loop and call stacks, and the inputs loaded into registers.
    fn entry_point_start(&mut self) {
        let interpolators = |code: &mut String, direction: &str| {
            for index in 0..INTERPOLATORS {
                let _ = writeln!(
                    code,
                    "    @location({index}) interpolator{index}: vec4<f32>,{direction}"
                );
            }
        };
        match self.stage {
            ShaderStage::Vertex => {
                emit!(self, "struct VertexOutput {{");
                emit!(self, "    @builtin(position) position: vec4<f32>,");
                interpolators(&mut self.code, "");
                emit!(self, "}}");
                emit!(
                    self,
                    "fn guest_vertex(vertex_index: u32) -> VertexOutput {{"
                );
            }
            ShaderStage::Pixel => {
                emit!(self, "struct FragmentInput {{");
                emit!(self, "    @builtin(position) position: vec4<f32>,");
                interpolators(&mut self.code, "");
                emit!(self, "}}");
                emit!(self, "struct FragmentOutput {{");
                for index in 0..COLOR_TARGETS {
                    emit!(self, "    @location({index}) color{index}: vec4<f32>,");
                }
                emit!(self, "}}");
                emit!(self, "@fragment");
                emit!(self, "fn main(input: FragmentInput) -> FragmentOutput {{");
            }
        }
        if self.scalar_registers {
            for index in 0..TEMPORARY_REGISTERS {
                emit!(self, "    var r{index}: vec4<f32>;");
            }
        } else {
            emit!(self, "    var r: array<vec4<f32>, {TEMPORARY_REGISTERS}>;");
        }
        emit!(self, "    var ps: f32 = 0.0;");
        emit!(self, "    var p: bool = false;");
        emit!(self, "    var a0: i32 = 0;");
        emit!(self, "    var aL: i32 = 0;");
        emit!(self, "    var loop_iteration: array<u32, {LOOP_DEPTH}>;");
        emit!(self, "    var loop_saved_aL: array<i32, {LOOP_DEPTH}>;");
        emit!(self, "    var loop_depth: u32 = 0u;");
        emit!(self, "    var call_return: array<u32, {CALL_DEPTH}>;");
        emit!(self, "    var call_depth: u32 = 0u;");
        emit!(self, "    var vfetch_address: i32 = 0;");
        match self.stage {
            ShaderStage::Vertex => {
                // The vertex index arrives in r0.x.
                emit!(self, "    {}.x = f32(vertex_index);", self.temporary(0));
                emit!(
                    self,
                    "    var out_position = vec4<f32>(0.0, 0.0, 0.0, 1.0);"
                );
                emit!(
                    self,
                    "    var out_interpolators: array<vec4<f32>, {INTERPOLATORS}>;"
                );
            }
            ShaderStage::Pixel => {
                // Interpolators arrive in the first registers.
                for index in 0..INTERPOLATORS {
                    emit!(
                        self,
                        "    {} = input.interpolator{index};",
                        self.temporary(index)
                    );
                }
                emit!(
                    self,
                    "    var out_colors: array<vec4<f32>, {COLOR_TARGETS}>;"
                );
            }
        }
    }

    fn entry_point_end(&mut self) {
        match self.stage {
            ShaderStage::Vertex => {
                emit!(self, "    var output: VertexOutput;");
                emit!(self, "    output.position = host_position(out_position);");
                for index in 0..INTERPOLATORS {
                    emit!(
                        self,
                        "    output.interpolator{index} = out_interpolators[{index}];"
                    );
                }
                emit!(self, "    return output;");
                emit!(self, "}}");
                self.vertex_entry_points();
                return;
            }
            ShaderStage::Pixel => {
                emit!(self, "    var output: FragmentOutput;");
                for index in 0..COLOR_TARGETS {
                    emit!(self, "    output.color{index} = out_colors[{index}];");
                }
            }
        }
        emit!(self, "    return output;");
        emit!(self, "}}");
    }

    fn condition(condition: Condition) -> String {
        const BITS_PER_WORD: u32 = 32;
        match condition {
            Condition::Always => "true".into(),
            Condition::Bool { constant, expected } => format!(
                "(((constants.bools[{}] >> {}u) & 1u) != 0u) == {expected}",
                constant / BITS_PER_WORD,
                constant % BITS_PER_WORD
            ),
            Condition::Predicate { expected } => format!("p == {expected}"),
        }
    }

    fn control_flow(&mut self, index: u32, kind: ControlFlowKind, end: u32) {
        let next = index + 1;
        match kind {
            ControlFlowKind::Exec {
                address,
                count,
                end: is_end,
                condition,
                ..
            } => {
                emit!(self, "if {} {{", Self::condition(condition));
                self.exec_body(address, count);
                emit!(self, "pc = {}u;", if is_end { end } else { next });
                emit!(self, "}} else {{ pc = {next}u; }}");
            }
            ControlFlowKind::LoopStart {
                address,
                loop_id,
                repeat,
            } => {
                // Loop constants: the count in bits 0-7, aL's start in 8-15, its step in 16-23.
                emit!(self, "let level = loop_depth % {LOOP_DEPTH}u;");
                emit!(self, "loop_saved_aL[level] = aL;");
                emit!(self, "loop_iteration[level] = 0u;");
                emit!(self, "loop_depth += 1u;");
                if !repeat {
                    emit!(
                        self,
                        "aL = i32(extractBits(constants.loops[{loop_id}], 8u, 8u));"
                    );
                }
                emit!(self, "if (constants.loops[{loop_id}] & 0xFFu) == 0u {{");
                emit!(self, "loop_depth -= 1u;");
                emit!(self, "aL = loop_saved_aL[level];");
                emit!(self, "pc = {address}u;");
                emit!(self, "}} else {{ pc = {next}u; }}");
            }
            ControlFlowKind::LoopEnd {
                address,
                loop_id,
                break_condition,
            } => {
                emit!(
                    self,
                    "let level = (loop_depth + {}u) % {LOOP_DEPTH}u;",
                    LOOP_DEPTH - 1
                );
                emit!(self, "loop_iteration[level] += 1u;");
                emit!(
                    self,
                    "aL += extractBits(bitcast<i32>(constants.loops[{loop_id}]), 16u, 8u);"
                );
                let break_check = break_condition
                    .map(|expected| format!(" && p != {expected}"))
                    .unwrap_or_default();
                emit!(
                    self,
                    "if loop_iteration[level] < (constants.loops[{loop_id}] & 0xFFu){break_check} {{"
                );
                emit!(self, "pc = {address}u;");
                emit!(self, "}} else {{");
                emit!(self, "loop_depth -= 1u;");
                emit!(self, "aL = loop_saved_aL[level];");
                emit!(self, "pc = {next}u;");
                emit!(self, "}}");
            }
            ControlFlowKind::Call { address, condition } => {
                emit!(self, "if {} {{", Self::condition(condition));
                emit!(self, "call_return[call_depth % {CALL_DEPTH}u] = {next}u;");
                emit!(self, "call_depth += 1u;");
                emit!(self, "pc = {address}u;");
                emit!(self, "}} else {{ pc = {next}u; }}");
            }
            // Outside a call, return does nothing.
            ControlFlowKind::Return => {
                emit!(self, "if call_depth == 0u {{ pc = {next}u; }} else {{");
                emit!(self, "call_depth -= 1u;");
                emit!(self, "pc = call_return[call_depth % {CALL_DEPTH}u];");
                emit!(self, "}}");
            }
            ControlFlowKind::Jump { address, condition } => {
                emit!(
                    self,
                    "if {} {{ pc = {address}u; }} else {{ pc = {next}u; }}",
                    Self::condition(condition)
                );
            }
            ControlFlowKind::Nop
            | ControlFlowKind::Alloc { .. }
            | ControlFlowKind::MarkVertexFetchDone => {
                emit!(self, "pc = {next}u;");
            }
        }
    }

    fn instruction(&mut self, instruction: Instruction) {
        let predicate = match instruction {
            Instruction::Alu(alu) => alu.predicate(),
            Instruction::VertexFetch(fetch) => fetch.predicate(),
            Instruction::TextureFetch(fetch) => fetch.predicate(),
            Instruction::OtherFetch(_) => None,
        };
        match predicate {
            Some(expected) => emit!(self, "if p == {expected} {{"),
            None => emit!(self, "{{"),
        }
        match instruction {
            Instruction::Alu(alu) => self.alu(alu),
            Instruction::VertexFetch(fetch) => self.vertex_fetch(fetch),
            Instruction::TextureFetch(fetch) => self.texture_fetch(fetch),
            Instruction::OtherFetch(words) => {
                self.unsupported
                    .insert(format!("fetch opcode {}", words[0] & 0x1F));
            }
        }
        emit!(self, "}}");
    }

    /// A source register or constant, as a vec4 or (with `component`) one component of it.
    fn operand(&self, alu: Alu, source: Source, component: Option<u32>) -> String {
        let register = source.register;
        let mut value = if source.is_temporary {
            self.addressed_temporary(register, source.relative)
        } else {
            let base = match self.stage {
                ShaderStage::Vertex => 0,
                ShaderStage::Pixel => FLOAT_CONSTANTS_PER_STAGE,
            };
            let index = if source.relative {
                let address = if alu.constant_relative_to_a0() {
                    "a0"
                } else {
                    "aL"
                };
                format!("(u32({address}) + {register}u) % {FLOAT_CONSTANTS_PER_STAGE}u")
            } else {
                format!("{register}u")
            };
            format!("constants.float_constants[{base}u + {index}]")
        };
        match component {
            Some(component) => {
                let _ = write!(
                    value,
                    ".{}",
                    COMPONENTS[source.component(component) as usize]
                );
            }
            None if source.swizzle != 0 => {
                value.push('.');
                value.extend((0..4).map(|c| COMPONENTS[source.component(c) as usize]));
            }
            None => {}
        }
        if source.absolute {
            value = format!("abs({value})");
        }
        if source.negate {
            value = format!("-({value})");
        }
        value
    }

    fn alu(&mut self, alu: Alu) {
        // Every source is read before anything is written.
        let vector_operands = VECTOR_OPERAND_COUNTS
            .get(alu.vector_opcode() as usize)
            .copied()
            .unwrap_or(0);
        for n in 1..=vector_operands {
            emit!(
                self,
                "let s{n} = {};",
                self.operand(alu, alu.source(n), None)
            );
        }
        // Scalar operations take `a` from source 3's W swizzle slot and `b` from its X slot.
        const SLOT_A: u32 = 3;
        const SLOT_B: u32 = 0;
        let scalar_opcode = alu.scalar_opcode();
        match scalar_operands(scalar_opcode) {
            ScalarOperands::None => {}
            ScalarOperands::One => {
                emit!(
                    self,
                    "let sa = {};",
                    self.operand(alu, alu.source(3), Some(SLOT_A))
                );
            }
            ScalarOperands::Two => {
                emit!(
                    self,
                    "let sa = {};",
                    self.operand(alu, alu.source(3), Some(SLOT_A))
                );
                emit!(
                    self,
                    "let sb = {};",
                    self.operand(alu, alu.source(3), Some(SLOT_B))
                );
            }
            ScalarOperands::ConstantAndTemporary => {
                let (constant, temporary) = alu.scalar_constant_operands();
                emit!(
                    self,
                    "let sa = {};",
                    self.operand(alu, constant, Some(SLOT_A))
                );
                emit!(
                    self,
                    "let sb = {};",
                    self.operand(alu, temporary, Some(SLOT_B))
                );
            }
        }
        let vector = self.vector_operation(alu.vector_opcode());
        emit!(self, "let v = {vector};");
        let scalar = self.scalar_operation(scalar_opcode);
        emit!(self, "let s = {scalar};");
        self.alu_writes(alu);
        emit!(self, "ps = s;");
    }

    fn discard(&mut self, condition: &str) {
        match self.stage {
            ShaderStage::Pixel => {
                emit!(self, "if {condition} {{ discard; }}");
            }
            ShaderStage::Vertex => {
                self.unsupported.insert("kill in a vertex shader".into());
            }
        }
    }

    /// Emits any side effects and returns the vector result's expression.
    fn vector_operation(&mut self, opcode: u32) -> String {
        let compare =
            |operator: &str| format!("select(vec4<f32>(0.0), vec4<f32>(1.0), s1 {operator} s2)");
        let conditional = |operator: &str| format!("select(s3, s2, s1 {operator} vec4<f32>(0.0))");
        match opcode {
            0 => "s1 + s2".into(),
            1 => "s1 * s2".into(),
            2 => "select(s2, s1, s1 >= s2)".into(),
            3 => "select(s2, s1, s1 < s2)".into(),
            4 => compare("=="),
            5 => compare(">"),
            6 => compare(">="),
            7 => compare("!="),
            8 => "fract(s1)".into(),
            9 => "trunc(s1)".into(),
            10 => "floor(s1)".into(),
            11 => "fma(s1, s2, s3)".into(),
            12 => conditional("=="),
            13 => conditional(">="),
            14 => conditional(">"),
            15 => "vec4<f32>(dot(s1, s2))".into(),
            16 => "vec4<f32>(dot(s1.xyz, s2.xyz))".into(),
            17 => "vec4<f32>(dot(s1.xy, s2.xy) + s3.x)".into(),
            18 => "cube(s1)".into(),
            19 => "vec4<f32>(max(max(s1.x, s1.y), max(s1.z, s1.w)))".into(),
            // setp_*_push: set the predicate from W, count from X.
            20..=23 => {
                let operator = ["==", "!=", ">", ">="][opcode as usize - 20];
                emit!(self, "p = s1.w == 0.0 && s2.w {operator} 0.0;");
                format!(
                    "select(vec4<f32>(s1.x + 1.0), vec4<f32>(0.0), s1.x == 0.0 && s2.x {operator} 0.0)"
                )
            }
            24..=27 => {
                let operator = ["==", ">", ">=", "!="][opcode as usize - 24];
                emit!(self, "let killed = any(s1 {operator} s2);");
                self.discard("killed");
                "select(vec4<f32>(0.0), vec4<f32>(1.0), killed)".into()
            }
            28 => "vec4<f32>(1.0, s1.y * s2.y, s1.z, s2.w)".into(),
            29 => {
                emit!(self, "a0 = address_register(floor(s1.w + 0.5));");
                "select(s2, s1, s1 >= s2)".into()
            }
            _ => {
                self.unsupported.insert(format!("vector opcode {opcode}"));
                "vec4<f32>(0.0)".into()
            }
        }
    }

    /// Emits any side effects and returns the scalar result's expression.
    fn scalar_operation(&mut self, opcode: u32) -> String {
        let setp = |translator: &mut Self, condition: &str| {
            emit!(translator, "p = {condition};");
            "select(1.0, 0.0, p)".to_string()
        };
        let reciprocal = "select(1.0 / sa, 1.0, sa == 1.0)";
        let reciprocal_root = "select(inverseSqrt(sa), 1.0, sa == 1.0)";
        let log = "select(log2(sa), 0.0, sa == 1.0)";
        match opcode {
            0 => "sa + sb".into(),
            1 => "sa + ps".into(),
            2 => "sa * sb".into(),
            3 => "sa * ps".into(),
            4 => "select(sa * ps, -FLT_MAX, ps == -FLT_MAX || !is_finite(ps) || !is_finite(sb) || sb <= 0.0)".into(),
            5 => "select(sb, sa, sa >= sb)".into(),
            6 => "select(sb, sa, sa < sb)".into(),
            7 => "select(0.0, 1.0, sa == 0.0)".into(),
            8 => "select(0.0, 1.0, sa > 0.0)".into(),
            9 => "select(0.0, 1.0, sa >= 0.0)".into(),
            10 => "select(0.0, 1.0, sa != 0.0)".into(),
            11 => "fract(sa)".into(),
            12 => "trunc(sa)".into(),
            13 => "floor(sa)".into(),
            14 => "exp2(sa)".into(),
            15 => format!("max({log}, -FLT_MAX)"),
            16 => log.into(),
            17 => format!("clamp_infinity({reciprocal}, FLT_MAX)"),
            18 => format!("clamp_infinity({reciprocal}, 0.0)"),
            19 => reciprocal.into(),
            20 => format!("clamp_infinity({reciprocal_root}, FLT_MAX)"),
            21 => format!("clamp_infinity({reciprocal_root}, 0.0)"),
            22 => reciprocal_root.into(),
            23 | 24 => {
                let rounding = if opcode == 23 { " + 0.5" } else { "" };
                emit!(self, "a0 = address_register(floor(sa{rounding}));");
                "select(sb, sa, sa >= sb)".into()
            }
            25 => "sa - sb".into(),
            26 => "sa - ps".into(),
            27 => setp(self, "sa == 0.0"),
            28 => setp(self, "sa != 0.0"),
            29 => setp(self, "sa > 0.0"),
            30 => setp(self, "sa >= 0.0"),
            31 => {
                emit!(self, "p = sa == 1.0;");
                "select(select(sa, 1.0, sa == 0.0), 0.0, p)".into()
            }
            32 => {
                emit!(self, "p = sa - 1.0 <= 0.0;");
                "select(sa - 1.0, 0.0, p)".into()
            }
            33 => {
                emit!(self, "p = false;");
                "FLT_MAX".into()
            }
            34 => {
                emit!(self, "p = sa == 0.0;");
                "select(sa, 0.0, p)".into()
            }
            35..=39 => {
                let condition = ["sa == 0.0", "sa > 0.0", "sa >= 0.0", "sa != 0.0", "sa == 1.0"]
                    [opcode as usize - 35];
                emit!(self, "let scalar_killed = {condition};");
                self.discard("scalar_killed");
                "select(0.0, 1.0, scalar_killed)".into()
            }
            40 => "sqrt(sa)".into(),
            42 | 43 => "sa * sb".into(),
            44 | 45 => "sa + sb".into(),
            46 | 47 => "sa - sb".into(),
            48 => "sin(sa)".into(),
            49 => "cos(sa)".into(),
            50 => "ps".into(),
            _ => {
                self.unsupported.insert(format!("scalar opcode {opcode}"));
                "0.0".into()
            }
        }
    }

    /// Where an export register's value goes, if anywhere.
    fn export_target(&mut self, register: u32) -> Option<String> {
        match (self.stage, register) {
            (ShaderStage::Vertex, 0..INTERPOLATORS) => {
                Some(format!("out_interpolators[{register}]"))
            }
            (ShaderStage::Vertex, EXPORT_POSITION) => Some("out_position".into()),
            (ShaderStage::Pixel, 0..COLOR_TARGETS) => Some(format!("out_colors[{register}]")),
            (ShaderStage::Vertex, EXPORT_POINT_SIZE) => {
                self.unsupported.insert("point size export".into());
                None
            }
            (ShaderStage::Pixel, EXPORT_DEPTH) => {
                self.unsupported.insert("depth export".into());
                None
            }
            _ => {
                self.unsupported
                    .insert(format!("{:?} export {register}", self.stage));
                None
            }
        }
    }

    fn alu_writes(&mut self, alu: Alu) {
        let clamp = |clamped: bool, value: String| {
            if clamped {
                format!("saturate({value})")
            } else {
                value
            }
        };
        let vector_mask = alu.vector_write_mask();
        let scalar_mask = alu.scalar_write_mask();
        if alu.is_export() {
            // Both results go to the vector destination; components both write are 1, and
            // with the scalar relative bit set, components neither writes are 0.
            const ALL: u32 = 0b1111;
            let zeros = if alu.scalar_dest_relative() {
                ALL & !(vector_mask | scalar_mask)
            } else {
                0
            };
            if vector_mask | scalar_mask | zeros == 0 {
                return;
            }
            let Some(target) = self.export_target(alu.vector_dest()) else {
                return;
            };
            for (component, name) in COMPONENTS.iter().enumerate() {
                let bit = 1 << component;
                let value = match (vector_mask & bit != 0, scalar_mask & bit != 0) {
                    (true, true) => "1.0".into(),
                    (true, false) => clamp(alu.vector_clamp(), format!("v.{name}")),
                    (false, true) => clamp(alu.scalar_clamp(), "s".into()),
                    (false, false) if zeros & bit != 0 => "0.0".into(),
                    (false, false) => continue,
                };
                emit!(self, "{target}.{name} = {value};");
            }
            return;
        }
        let vector_target = self.addressed_temporary(alu.vector_dest(), alu.vector_dest_relative());
        let scalar_target = self.addressed_temporary(alu.scalar_dest(), alu.scalar_dest_relative());
        for (component, name) in COMPONENTS.iter().enumerate() {
            if vector_mask & (1 << component) != 0 {
                let value = clamp(alu.vector_clamp(), format!("v.{name}"));
                emit!(self, "{vector_target}.{name} = {value};");
            }
        }
        for (component, name) in COMPONENTS.iter().enumerate() {
            if scalar_mask & (1 << component) != 0 {
                let value = clamp(alu.scalar_clamp(), "s".into());
                emit!(self, "{scalar_target}.{name} = {value};");
            }
        }
    }

    fn vertex_fetch(&mut self, fetch: VertexFetch) {
        let format = fetch.format();
        let Some(unpacked) = unpack_vertex(format, fetch.is_signed(), fetch.is_normalized()) else {
            self.unsupported.insert(format!("vertex format {format}"));
            return;
        };
        // Vertex fetch constants are two words each: the address (above the type bits), then
        // the byte order (low two bits) and size. The address is a byte offset in `memory`.
        let constant_word = fetch.constant() * 2;
        if !fetch.is_mini_fetch() {
            emit!(
                self,
                "vfetch_address = i32(constants.fetch[{constant_word}] >> 2u);"
            );
            if fetch.stride() != 0 {
                let rounding = if fetch.is_index_rounded() {
                    " + 0.5"
                } else {
                    ""
                };
                emit!(
                    self,
                    "vfetch_address += i32(floor({}.{}{rounding})) * {};",
                    self.temporary(fetch.source_register()),
                    COMPONENTS[fetch.source_component() as usize],
                    fetch.stride()
                );
            }
        }
        emit!(
            self,
            "let endian = constants.fetch[{}] & 3u;",
            constant_word + 1
        );
        for word in 0..vertex_format_words(format) {
            emit!(
                self,
                "let w{word} = endian_swap(memory[u32(vfetch_address + {})], endian);",
                fetch.offset() + word as i32
            );
        }
        let scale = match fetch.exponent_adjust() {
            0 => String::new(),
            exponent => format!(" * {:?}", 2f32.powi(exponent)),
        };
        emit!(self, "let fetched = {unpacked}{scale};");
        self.fetch_writes(fetch.dest_register(), fetch.dest_swizzle(), "fetched");
    }

    /// Writes a fetch result through its destination swizzle.
    fn fetch_writes(&mut self, register: u32, swizzle: u32, value: &str) {
        const SELECTOR_BITS: u32 = 3;
        const SELECTOR_MASK: u32 = 0b111;
        for (component, name) in COMPONENTS.iter().enumerate() {
            let source = match (swizzle >> (SELECTOR_BITS * component as u32)) & SELECTOR_MASK {
                selector @ 0..=3 => format!("{value}.{}", COMPONENTS[selector as usize]),
                FETCH_ZERO => "0.0".into(),
                FETCH_ONE => "1.0".into(),
                FETCH_KEEP => continue,
                _ => continue,
            };
            emit!(self, "{}.{name} = {source};", self.temporary(register));
        }
    }

    /// Samples a 2D texture at its base level, then applies the fetch constant's swizzle.
    /// Other dimensions return opaque white so geometry still shows.
    fn texture_fetch(&mut self, fetch: TextureFetch) {
        if fetch.dimension() != DIMENSION_2D {
            self.unsupported.insert(format!(
                "texture dimension {} (returns white)",
                fetch.dimension()
            ));
            self.fetch_writes(
                fetch.dest_register(),
                fetch.dest_swizzle(),
                "vec4<f32>(1.0)",
            );
            return;
        }
        let constant = fetch.constant();
        let source = self.temporary(fetch.source_register());
        let coordinate =
            |index: u32| COMPONENTS[((fetch.source_swizzle() >> (2 * index)) & 0b11) as usize];
        emit!(
            self,
            "var coordinates = vec2<f32>({source}.{}, {source}.{});",
            coordinate(0),
            coordinate(1)
        );
        if fetch.unnormalized_coordinates() {
            emit!(
                self,
                "coordinates /= vec2<f32>(textureDimensions(texture{constant}));"
            );
        }
        emit!(
            self,
            "let texel = swizzle_texel(textureSampleLevel(texture{constant}, sampler{constant}, coordinates, 0.0), constants.texture_swizzles[{constant}]);"
        );
        self.fetch_writes(fetch.dest_register(), fetch.dest_swizzle(), "texel");
    }

    /// The vertex entry points (see `VertexEntry`), all running `guest_vertex`.
    fn vertex_entry_points(&mut self) {
        self.code.push_str(VERTEX_ENTRY_POINTS);
        emit!(
            self,
            "fn parallelogram(p: VertexOutput, q: VertexOutput, corner: VertexOutput) -> VertexOutput {{"
        );
        emit!(self, "    var output: VertexOutput;");
        emit!(
            self,
            "    output.position = p.position + q.position - corner.position;"
        );
        for index in 0..INTERPOLATORS {
            emit!(
                self,
                "    output.interpolator{index} = p.interpolator{index} + q.interpolator{index} - corner.interpolator{index};"
            );
        }
        emit!(self, "    return output;");
        emit!(self, "}}");
    }
}

const DIMENSION_2D: u32 = 1;

/// Whether an instruction addresses temporary registers relative to aL.
fn indexes_registers_relatively(instruction: Instruction) -> bool {
    match instruction {
        Instruction::Alu(alu) => {
            (!alu.is_export() && (alu.vector_dest_relative() || alu.scalar_dest_relative()))
                || (1..=3).any(|n| {
                    let source = alu.source(n);
                    source.is_temporary && source.relative
                })
        }
        Instruction::VertexFetch(_) | Instruction::TextureFetch(_) | Instruction::OtherFetch(_) => {
            false
        }
    }
}

/// Whether the control flow only runs execs in order, without loops, calls or jumps.
fn is_straight_line(control_flow: &[ControlFlowKind]) -> bool {
    control_flow.iter().all(|kind| {
        matches!(
            kind,
            ControlFlowKind::Exec { .. }
                | ControlFlowKind::Alloc { .. }
                | ControlFlowKind::Nop
                | ControlFlowKind::MarkVertexFetchDone
        )
    })
}

const VERTEX_ENTRY_POINTS: &str = "\
@vertex
fn main(@builtin(vertex_index) index: u32) -> VertexOutput {
    return guest_vertex(guest_index(index));
}

@vertex
fn main_indexed(@builtin(vertex_index) index: u32) -> VertexOutput {
    return guest_vertex(index + constants.index_bias);
}

@vertex
fn main_quad_list(@builtin(vertex_index) index: u32) -> VertexOutput {
    var corners = array<u32, 6>(0u, 1u, 2u, 0u, 2u, 3u);
    return guest_vertex(guest_index(index / 6u * 4u + corners[index % 6u]));
}

@vertex
fn main_triangle_fan(@builtin(vertex_index) index: u32) -> VertexOutput {
    let triangle = index / 3u;
    let corner = index % 3u;
    return guest_vertex(guest_index(select(triangle + corner, 0u, corner == 0u)));
}

// Of a rectangle's three given corners, the one opposite the longest edge holds the right angle,
// and the missing corner mirrors it. Host vertices: (corner, p, q), then (q, p, fourth), which
// keeps the winding.
@vertex
fn main_rectangle_list(@builtin(vertex_index) index: u32) -> VertexOutput {
    let first = index / 6u * 3u;
    let v0 = guest_vertex(guest_index(first));
    let v1 = guest_vertex(guest_index(first + 1u));
    let v2 = guest_vertex(guest_index(first + 2u));
    let s0 = v0.position.xy / v0.position.w;
    let s1 = v1.position.xy / v1.position.w;
    let s2 = v2.position.xy / v2.position.w;
    let d01 = distance(s0, s1);
    let d02 = distance(s0, s2);
    let d12 = distance(s1, s2);
    var corner = v2;
    var p = v0;
    var q = v1;
    if d12 >= d01 && d12 >= d02 {
        corner = v0;
        p = v1;
        q = v2;
    } else if d02 >= d01 {
        corner = v1;
        p = v0;
        q = v2;
    }
    switch index % 6u {
        case 0u: { return corner; }
        case 1u, 4u: { return p; }
        case 2u, 3u: { return q; }
        default: { return parallelogram(p, q, corner); }
    }
}
";

/// How many vector sources each vector opcode reads (Xenia's kAluVectorOpcodeInfos).
const VECTOR_OPERAND_COUNTS: [u32; 30] = [
    2, 2, 2, 2, 2, 2, 2, 2, // add .. sne
    1, 1, 1, // frc, trunc, floor
    3, 3, 3, 3, // mad, cndeq, cndge, cndgt
    2, 2, 3, 2, 1, // dp4, dp3, dp2add, cube, max4
    2, 2, 2, 2, // setp_*_push
    2, 2, 2, 2, // kill_*
    2, 2, // dst, maxa
];

enum ScalarOperands {
    None,
    /// Source 3's `a` component.
    One,
    /// Source 3's `a` and `b` components.
    Two,
    /// mulsc, addsc, subsc: a constant's `a` and a temporary's `b`.
    ConstantAndTemporary,
}

/// Scalar operand kinds (Xenia's kAluScalarOpcodeInfos).
fn scalar_operands(opcode: u32) -> ScalarOperands {
    match opcode {
        0 | 2 | 4 | 5 | 6 | 23 | 24 | 25 => ScalarOperands::Two,
        33 | 41 | 50.. => ScalarOperands::None,
        42..=47 => ScalarOperands::ConstantAndTemporary,
        _ => ScalarOperands::One,
    }
}

fn vertex_format_words(format: u32) -> u32 {
    match format {
        FORMAT_16_16_16_16 | FORMAT_16_16_16_16_FLOAT | FORMAT_32_32 | FORMAT_32_32_FLOAT => 2,
        FORMAT_32_32_32_FLOAT => 3,
        FORMAT_32_32_32_32 | FORMAT_32_32_32_32_FLOAT => 4,
        _ => 1,
    }
}

/// The fetched words (`w0`, `w1`, ...) as a vec4, missing components zero.
fn unpack_vertex(format: u32, signed: bool, normalized: bool) -> Option<String> {
    // Packed formats: (word, bit offset, width) per component.
    let packed: &[(u32, u32, u32)] = match format {
        FORMAT_8_8_8_8 => &[(0, 0, 8), (0, 8, 8), (0, 16, 8), (0, 24, 8)],
        FORMAT_2_10_10_10 => &[(0, 0, 10), (0, 10, 10), (0, 20, 10), (0, 30, 2)],
        FORMAT_10_11_11 => &[(0, 0, 11), (0, 11, 11), (0, 22, 10)],
        FORMAT_11_11_10 => &[(0, 0, 10), (0, 10, 11), (0, 21, 11)],
        FORMAT_16_16 => &[(0, 0, 16), (0, 16, 16)],
        FORMAT_16_16_16_16 => &[(0, 0, 16), (0, 16, 16), (1, 0, 16), (1, 16, 16)],
        _ => &[],
    };
    let components: Vec<String> = if !packed.is_empty() {
        packed
            .iter()
            .map(|&(word, offset, width)| {
                let value = if signed {
                    format!("f32(extractBits(bitcast<i32>(w{word}), {offset}u, {width}u))")
                } else {
                    format!("f32(extractBits(w{word}, {offset}u, {width}u))")
                };
                match (normalized, signed) {
                    (false, _) => value,
                    (true, true) => {
                        format!(
                            "max({value} / {:?}, -1.0)",
                            ((1u32 << (width - 1)) - 1) as f32
                        )
                    }
                    (true, false) => format!("{value} / {:?}", ((1u64 << width) - 1) as f32),
                }
            })
            .collect()
    } else {
        match format {
            FORMAT_16_16_FLOAT => vec!["unpack_halves(w0)".into()],
            FORMAT_16_16_16_16_FLOAT => {
                vec!["unpack_halves(w0)".into(), "unpack_halves(w1)".into()]
            }
            FORMAT_32 | FORMAT_32_32 | FORMAT_32_32_32_32 => (0..vertex_format_words(format))
                .map(|word| {
                    let (value, range) = if signed {
                        (format!("f32(bitcast<i32>(w{word}))"), i32::MAX as f32)
                    } else {
                        (format!("f32(w{word})"), u32::MAX as f32)
                    };
                    if normalized {
                        format!("{value} / {range:?}")
                    } else {
                        value
                    }
                })
                .collect(),
            FORMAT_32_FLOAT
            | FORMAT_32_32_FLOAT
            | FORMAT_32_32_32_FLOAT
            | FORMAT_32_32_32_32_FLOAT => (0..vertex_format_words(format))
                .map(|word| format!("bitcast<f32>(w{word})"))
                .collect(),
            _ => return None,
        }
    };
    // Half-float pairs count as two components each.
    let width: usize = components
        .iter()
        .map(|component| {
            if component.starts_with("unpack_halves") {
                2
            } else {
                1
            }
        })
        .sum();
    let padding = std::iter::repeat_n("0.0".to_string(), 4 - width);
    let all: Vec<String> = components.into_iter().chain(padding).collect();
    Some(format!("vec4<f32>({})", all.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CF_OPCODE_SHIFT: u32 = 12;
    const CF_EXEC_END: u32 = 2;
    const CF_COND_EXEC: u32 = 3;
    const CF_LOOP_START: u32 = 7;
    const CF_LOOP_END: u32 = 8;
    const CF_COND_CALL: u32 = 9;
    const CF_RETURN: u32 = 10;
    const CF_COND_JMP: u32 = 11;
    const UNCONDITIONAL_BRANCH: u32 = 1 << 13;

    /// Packs control-flow instructions (low word, high 16 bits) two per three words, then
    /// appends the ALU/fetch instructions, which start right after them.
    fn shader(control_flow: &[[u32; 2]], instructions: &[[u32; 3]]) -> Vec<u32> {
        let mut words = Vec::new();
        for pair in control_flow.chunks(2) {
            let first = pair[0];
            let second = pair.get(1).copied().unwrap_or([0, 0]);
            words.extend([
                first[0],
                first[1] | (second[0] << 16),
                (second[0] >> 16) | (second[1] << 16),
            ]);
        }
        for instruction in instructions {
            words.extend(instruction);
        }
        words
    }

    fn exec(opcode: u32, address: u32, count: u32, sequence: u32) -> [u32; 2] {
        [
            address | (count << 12) | (sequence << 16),
            opcode << CF_OPCODE_SHIFT,
        ]
    }

    fn assert_valid(stage: ShaderStage, words: &[u32]) -> Translation {
        let translation = translate(stage, words);
        if let Err(error) = validate(&translation.wgsl) {
            panic!("{error}\n{}", translation.wgsl);
        }
        translation
    }

    /// An ALU instruction with the given opcodes, writing r1 from r2, c3 and r4.
    fn alu(vector_opcode: u32, scalar_opcode: u32) -> [u32; 3] {
        const WRITE_ALL: u32 = 0b1111;
        const SOURCE_1_TEMPORARY: u32 = 1 << 31;
        const SOURCE_3_TEMPORARY: u32 = 1 << 29;
        [
            1 | (1 << 8) | (WRITE_ALL << 16) | (WRITE_ALL << 20) | (scalar_opcode << 26),
            0,
            4 | (3 << 8)
                | (2 << 16)
                | (vector_opcode << 24)
                | SOURCE_1_TEMPORARY
                | SOURCE_3_TEMPORARY,
        ]
    }

    #[test]
    fn every_alu_opcode_translates_to_valid_wgsl() {
        const VECTOR_OPCODES: u32 = 30;
        const SCALAR_OPCODES: u32 = 51;
        const UNDEFINED_SCALAR_OPCODE: u32 = 41;
        for (vector, scalar) in (0..VECTOR_OPCODES).map(|vector| (vector, 50)).chain(
            (0..SCALAR_OPCODES)
                .filter(|&s| s != UNDEFINED_SCALAR_OPCODE)
                .map(|s| (0, s)),
        ) {
            let words = shader(&[exec(CF_EXEC_END, 1, 1, 0)], &[alu(vector, scalar)]);
            let translation = assert_valid(ShaderStage::Pixel, &words);
            assert!(
                translation.unsupported.is_empty(),
                "vector {vector}, scalar {scalar}: {:?}",
                translation.unsupported
            );
        }
    }

    #[test]
    fn vertex_fetches_and_exports_translate_to_valid_wgsl() {
        const FETCH_INTO_R1_FROM_VF0: u32 = 1 << 12;
        const IDENTITY_SWIZZLE: u32 = 0b011_010_001_000;
        const EXPORT_POSITION_FROM_R1: [u32; 3] = [
            EXPORT_POSITION | (1 << 15) | (0b1111 << 16),
            0,
            // max r1, r1: a move.
            (1 << 8) | (1 << 16) | (2 << 24) | (1 << 31) | (1 << 30),
        ];
        let formats = [
            FORMAT_8_8_8_8,
            FORMAT_2_10_10_10,
            FORMAT_10_11_11,
            FORMAT_11_11_10,
            FORMAT_16_16,
            FORMAT_16_16_16_16,
            FORMAT_16_16_FLOAT,
            FORMAT_16_16_16_16_FLOAT,
            FORMAT_32,
            FORMAT_32_32,
            FORMAT_32_32_32_32,
            FORMAT_32_FLOAT,
            FORMAT_32_32_FLOAT,
            FORMAT_32_32_32_32_FLOAT,
            FORMAT_32_32_32_FLOAT,
        ];
        for format in formats {
            for signed in [false, true] {
                let fetch = [
                    FETCH_INTO_R1_FROM_VF0,
                    IDENTITY_SWIZZLE | (u32::from(signed) << 12) | (format << 16),
                    vertex_format_words(format),
                ];
                let mut position = EXPORT_POSITION_FROM_R1;
                position[0] |= 50 << 26;
                // Sequence: the first instruction is a fetch.
                let words = shader(&[exec(CF_EXEC_END, 1, 2, 0b01)], &[fetch, position]);
                let translation = assert_valid(ShaderStage::Vertex, &words);
                assert!(
                    translation.unsupported.is_empty(),
                    "{format}: {:?}",
                    translation.unsupported
                );
                assert!(translation.wgsl.contains("out_position.w = v.w;"));
            }
        }
    }

    #[test]
    fn control_flow_translates_to_valid_wgsl() {
        const BOOL_CONSTANT: u32 = 3;
        // A loop (0-2) around a conditional exec, a call (3) to a return (6), a jump (4) over a
        // bool-conditional exec (5), and the final exec (7). ALU instructions start at 4.
        let control_flow = [
            [3 | (1 << 16), CF_LOOP_START << CF_OPCODE_SHIFT],
            exec(CF_COND_EXEC, 4, 1, 0),
            [1 | (1 << 16), CF_LOOP_END << CF_OPCODE_SHIFT],
            [6 | UNCONDITIONAL_BRANCH, CF_COND_CALL << CF_OPCODE_SHIFT],
            [7 | UNCONDITIONAL_BRANCH, CF_COND_JMP << CF_OPCODE_SHIFT],
            [0, (CF_COND_EXEC << CF_OPCODE_SHIFT) | (BOOL_CONSTANT << 2)],
            [0, CF_RETURN << CF_OPCODE_SHIFT],
            exec(CF_EXEC_END, 4, 1, 0),
        ];
        let words = shader(&control_flow, &[alu(0, 50)]);
        let translation = assert_valid(ShaderStage::Pixel, &words);
        assert!(
            translation.unsupported.is_empty(),
            "{:?}",
            translation.unsupported
        );
    }

    #[test]
    fn relative_register_addressing_uses_a_register_array() {
        const RELATIVE_BIT: u32 = 0x40;
        let mut instruction = alu(0, 50);
        // Source 1 (bits 16-23 of word 2) reads r2 + aL.
        instruction[2] |= RELATIVE_BIT << 16;
        let words = shader(&[exec(CF_EXEC_END, 1, 1, 0)], &[instruction]);
        let translation = assert_valid(ShaderStage::Pixel, &words);
        assert!(translation.wgsl.contains("var r: array<vec4<f32>, 64>;"));
        assert!(translation.wgsl.contains("r[(u32(aL) + 2u) % 64u]"));
    }

    #[test]
    fn a_conditional_end_puts_the_rest_of_the_shader_in_its_else() {
        const CF_COND_EXEC_END: u32 = 4;
        let words = shader(
            &[exec(CF_COND_EXEC_END, 2, 1, 0), exec(CF_EXEC_END, 2, 1, 0)],
            &[[0; 3], alu(0, 50)],
        );
        let translation = assert_valid(ShaderStage::Pixel, &words);
        assert!(!translation.wgsl.contains("switch pc"));
        assert!(translation.wgsl.contains("} else {"));
    }

    #[test]
    fn texture_fetches_bind_their_constant() {
        const TEXTURE_FETCH: u32 = 1;
        const CONSTANT: u32 = 5;
        const DIMENSION_2D_BITS: u32 = DIMENSION_2D << 14;
        let fetch = [
            TEXTURE_FETCH | (1 << 12) | (CONSTANT << 20),
            0b011_010_001_000,
            DIMENSION_2D_BITS,
        ];
        let words = shader(&[exec(CF_EXEC_END, 1, 1, 0b01)], &[fetch]);
        let translation = assert_valid(ShaderStage::Pixel, &words);
        assert_eq!(
            translation.textures.into_iter().collect::<Vec<_>>(),
            [CONSTANT]
        );
        assert!(
            translation.unsupported.is_empty(),
            "{:?}",
            translation.unsupported
        );
    }

    #[test]
    fn untranslatable_opcodes_are_reported() {
        const UNDEFINED_VECTOR_OPCODE: u32 = 30;
        let words = shader(
            &[exec(CF_EXEC_END, 1, 1, 0)],
            &[alu(UNDEFINED_VECTOR_OPCODE, 50)],
        );
        let translation = assert_valid(ShaderStage::Pixel, &words);
        assert_eq!(
            translation.unsupported.into_iter().collect::<Vec<_>>(),
            ["vector opcode 30"]
        );
    }
}
