//! What the command processor hands to a renderer: decoded draws and the shader microcode they
//! use. Nothing here draws; it describes the title's GPU work in the console's terms.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

/// VGT_DRAW_INITIATOR fields.
const PRIMITIVE_MASK: u32 = 0x3F;
const SOURCE_SHIFT: u32 = 6;
const SOURCE_MASK: u32 = 0x3;
const INDEX_SIZE_BIT: u32 = 1 << 11;
const INDEX_COUNT_SHIFT: u32 = 16;
/// VGT_DMA_SIZE: the index count in words, and the byte order in the top two bits.
const DMA_WORDS_MASK: u32 = 0x00FF_FFFF;
const DMA_SWAP_SHIFT: u32 = 30;

/// Render state register indices (Xenia's `register_table.inc`).
pub mod registers {
    pub const RB_SURFACE_INFO: u32 = 0x2000;
    pub const RB_COLOR_INFO: u32 = 0x2001;
    pub const RB_DEPTH_INFO: u32 = 0x2002;
    pub const RB_COLOR1_INFO: u32 = 0x2003;
    pub const RB_COLOR2_INFO: u32 = 0x2004;
    pub const RB_COLOR3_INFO: u32 = 0x2005;
    pub const PA_SC_WINDOW_OFFSET: u32 = 0x2080;
    pub const PA_SC_WINDOW_SCISSOR_TL: u32 = 0x2081;
    pub const PA_SC_WINDOW_SCISSOR_BR: u32 = 0x2082;
    pub const VGT_INDX_OFFSET: u32 = 0x2102;
    pub const RB_COLOR_MASK: u32 = 0x2104;
    /// The blend constant: red, then green, blue and alpha, as floats.
    pub const RB_BLEND_RED: u32 = 0x2105;
    pub const RB_STENCILREFMASK: u32 = 0x210D;
    /// Followed by X offset, Y scale, Y offset, Z scale and Z offset, as floats.
    pub const PA_CL_VPORT_XSCALE: u32 = 0x210F;
    pub const SQ_PROGRAM_CNTL: u32 = 0x2180;
    pub const RB_DEPTHCONTROL: u32 = 0x2200;
    pub const RB_BLENDCONTROL0: u32 = 0x2201;
    pub const PA_SU_SC_MODE_CNTL: u32 = 0x2205;
    pub const PA_CL_VTE_CNTL: u32 = 0x2206;
    pub const RB_MODECONTROL: u32 = 0x2208;
    pub const PA_SU_VTX_CNTL: u32 = 0x2302;
    pub const RB_COPY_CONTROL: u32 = 0x2318;
    pub const RB_COPY_DEST_BASE: u32 = 0x2319;
    pub const RB_COPY_DEST_PITCH: u32 = 0x231A;
    pub const RB_COPY_DEST_INFO: u32 = 0x231B;
    pub const RB_DEPTH_CLEAR: u32 = 0x231D;
    pub const RB_COLOR_CLEAR: u32 = 0x231E;
}

/// RB_MODECONTROL's EDRAM mode for a draw that resolves (copies) EDRAM to memory instead of
/// rendering.
pub const EDRAM_MODE_COPY: u32 = 6;
pub const EDRAM_MODE_MASK: u32 = 0x7;
/// The viewport's scale and offset registers, X, Y then Z.
pub const VIEWPORT_REGISTERS: u32 = 6;

/// Fetch constants start at this register: 32 slots of six words, each a texture or three
/// vertex buffers.
pub const FETCH_CONSTANTS: u32 = 0x4800;
pub const FETCH_CONSTANT_WORDS: u32 = 6;

/// A texture fetch constant's fields (Xenia's `xe_gpu_texture_fetch_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextureDescriptor {
    /// Tiled textures store 32x32 texel tiles in a swizzled order; linear ones store rows.
    pub tiled: bool,
    /// Row pitch, in texels.
    pub pitch: u32,
    /// The xenos::TextureFormat.
    pub format: u32,
    pub endian: u32,
    /// Physical address of the base level.
    pub base: u32,
    pub width: u32,
    pub height: u32,
    /// 1D, 2D, 3D or cube.
    pub dimension: u32,
    /// Three bits per component choosing a source component, or 0 or 1.
    pub swizzle: u32,
    /// Addressing outside 0-1 (xenos::ClampMode): repeat, mirror, clamp, ...
    pub clamp_x: u32,
    pub clamp_y: u32,
    /// xenos::TextureFilter: point (0) or linear (1).
    pub mag_filter: u32,
    pub min_filter: u32,
}

impl TextureDescriptor {
    pub fn decode(words: [u32; FETCH_CONSTANT_WORDS as usize]) -> Self {
        const PITCH_UNIT: u32 = 32;
        const PAGE_MASK: u32 = !0xFFF;
        // 2D sizes are stored minus one, 13 bits each.
        const SIZE_MASK: u32 = 0x1FFF;
        const HEIGHT_SHIFT: u32 = 13;
        Self {
            tiled: words[0] >> 31 != 0,
            pitch: ((words[0] >> 22) & 0x1FF) * PITCH_UNIT,
            format: words[1] & 0x3F,
            endian: (words[1] >> 6) & 0x3,
            base: words[1] & PAGE_MASK,
            width: (words[2] & SIZE_MASK) + 1,
            height: ((words[2] >> HEIGHT_SHIFT) & SIZE_MASK) + 1,
            dimension: (words[5] >> 9) & 0x3,
            swizzle: (words[3] >> 1) & 0xFFF,
            clamp_x: (words[0] >> 10) & 0x7,
            clamp_y: (words[0] >> 13) & 0x7,
            mag_filter: (words[3] >> 19) & 0x3,
            min_filter: (words[3] >> 21) & 0x3,
        }
    }
}

/// Where a draw's indices come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexSource {
    /// An index buffer in memory.
    Dma,
    /// Inline in the packet; titles do not use it.
    Immediate,
    /// Generated: 0, 1, 2, ...
    Auto,
}

/// A decoded VGT_DRAW_INITIATOR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Initiator {
    /// The Xenos primitive type (triangle list 0x04, rectangle list 0x08, quad list 0x0D, ...).
    pub primitive: u32,
    pub source: IndexSource,
    pub indices_are_32_bit: bool,
    pub index_count: u32,
}

impl Initiator {
    pub fn decode(value: u32) -> Self {
        Self {
            primitive: value & PRIMITIVE_MASK,
            source: match (value >> SOURCE_SHIFT) & SOURCE_MASK {
                0 => IndexSource::Dma,
                1 => IndexSource::Immediate,
                _ => IndexSource::Auto,
            },
            indices_are_32_bit: value & INDEX_SIZE_BIT != 0,
            index_count: value >> INDEX_COUNT_SHIFT,
        }
    }
}

/// An index buffer: its guest address, how many indices it holds and their byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexBuffer {
    pub address: u32,
    pub words: u32,
    pub endian: u32,
}

impl IndexBuffer {
    /// From VGT_DMA_SIZE, once the base has been translated to a guest address.
    pub fn new(address: u32, dma_size: u32) -> Self {
        Self {
            address,
            words: dma_size & DMA_WORDS_MASK,
            endian: dma_size >> DMA_SWAP_SHIFT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShaderStage {
    Vertex,
    Pixel,
}

/// A shader's microcode, identified by a hash of its words. Its stage is the slot it is loaded
/// into.
#[derive(Clone, Debug)]
pub struct Shader {
    pub hash: u64,
    pub words: Arc<[u32]>,
}

/// Every distinct shader the title has loaded, so each is translated once.
#[derive(Default)]
pub struct ShaderCache {
    shaders: HashMap<(ShaderStage, u64), Shader>,
}

impl ShaderCache {
    /// The cached shader with these words, adding it if new; the flag says whether it was.
    pub fn intern(&mut self, stage: ShaderStage, words: &[u32]) -> (Shader, bool) {
        let mut hasher = DefaultHasher::new();
        words.hash(&mut hasher);
        let hash = hasher.finish();
        let mut is_new = false;
        let shader = self
            .shaders
            .entry((stage, hash))
            .or_insert_with(|| {
                is_new = true;
                Shader {
                    hash,
                    words: words.into(),
                }
            })
            .clone();
        (shader, is_new)
    }

    pub fn len(&self) -> usize {
        self.shaders.len()
    }
}

/// One draw call, with the shaders active when it was issued.
#[derive(Clone, Debug)]
pub struct Draw {
    pub initiator: Initiator,
    pub indices: Option<IndexBuffer>,
    pub vertex_shader: Option<Shader>,
    pub pixel_shader: Option<Shader>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initiators_decode_their_fields() {
        // An auto-indexed quad list of four vertices, as the title issues for full-screen passes.
        const AUTO_QUAD_LIST: u32 = 0x0004_008D;
        const QUAD_LIST: u32 = 0x0D;
        assert_eq!(
            Initiator::decode(AUTO_QUAD_LIST),
            Initiator {
                primitive: QUAD_LIST,
                source: IndexSource::Auto,
                indices_are_32_bit: false,
                index_count: 4,
            }
        );
    }

    #[test]
    fn identical_microcode_is_cached_once() {
        let mut cache = ShaderCache::default();
        let (first, first_is_new) = cache.intern(ShaderStage::Pixel, &[1, 2, 3]);
        let (again, again_is_new) = cache.intern(ShaderStage::Pixel, &[1, 2, 3]);
        cache.intern(ShaderStage::Vertex, &[1, 2, 3]);
        assert!(first_is_new && !again_is_new);
        assert!(Arc::ptr_eq(&first.words, &again.words));
        assert_eq!(cache.len(), 2);
    }
}
