//! Guest textures as host texel data: undoing the console's tiled layout and byte order, and
//! decoding formats hosts may not sample directly (DXT, 16-bit packed) to 8-bit RGBA.
//!
//! Only the base level of 2D textures is read. Layouts follow Xenia's `texture_address.h`.

use crate::gpu::TextureDescriptor;

/// xenos::TextureFormat values handled here.
const FORMAT_8: u32 = 2;
const FORMAT_1_5_5_5: u32 = 3;
const FORMAT_5_6_5: u32 = 4;
const FORMAT_8_8_8_8: u32 = 6;
const FORMAT_8_8: u32 = 10;
const FORMAT_4_4_4_4: u32 = 15;
const FORMAT_DXT1: u32 = 18;
const FORMAT_DXT2_3: u32 = 19;
const FORMAT_DXT4_5: u32 = 20;

/// Tiled textures are stored in 32x32-block macro tiles, so their pitch is padded to 32 blocks.
const MACRO_TILE_BLOCKS: u32 = 32;
const DXT_BLOCK_SIZE: u32 = 4;

/// What the host texture holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFormat {
    R8,
    Rg8,
    Rgba8,
}

/// A decoded base level, rows packed tightly.
pub struct Decoded {
    pub format: HostFormat,
    pub data: Vec<u8>,
}

/// How a guest format is stored: block size in texels, and bytes per block.
#[derive(Clone, Copy)]
struct Layout {
    block_size: u32,
    bytes_per_block: u32,
}

fn layout(format: u32) -> Option<Layout> {
    let texels = |bytes_per_block| Layout {
        block_size: 1,
        bytes_per_block,
    };
    let dxt = |bytes_per_block| Layout {
        block_size: DXT_BLOCK_SIZE,
        bytes_per_block,
    };
    Some(match format {
        FORMAT_8 => texels(1),
        FORMAT_1_5_5_5 | FORMAT_5_6_5 | FORMAT_8_8 | FORMAT_4_4_4_4 => texels(2),
        FORMAT_8_8_8_8 => texels(4),
        FORMAT_DXT1 => dxt(8),
        FORMAT_DXT2_3 | FORMAT_DXT4_5 => dxt(16),
        _ => return None,
    })
}

/// The block grid a texture occupies in memory: its row pitch and height in blocks.
fn storage_blocks(descriptor: &TextureDescriptor, layout: Layout) -> (u32, u32) {
    let blocks = |texels: u32| texels.div_ceil(layout.block_size);
    let pitch = blocks(descriptor.pitch.max(descriptor.width));
    let height = blocks(descriptor.height);
    if descriptor.tiled {
        (
            pitch.next_multiple_of(MACRO_TILE_BLOCKS),
            height.next_multiple_of(MACRO_TILE_BLOCKS),
        )
    } else {
        (pitch, height)
    }
}

/// How many bytes of guest memory the base level spans, if the format is handled. Tiling
/// interleaves macro tiles of small texels within 4 KiB, so tiled sizes round up to it.
pub fn guest_size(descriptor: &TextureDescriptor) -> Option<u32> {
    const TILED_ALIGNMENT: u32 = 4096;
    let layout = layout(descriptor.format)?;
    let (pitch, height) = storage_blocks(descriptor, layout);
    let size = pitch * height * layout.bytes_per_block;
    Some(if descriptor.tiled {
        size.next_multiple_of(TILED_ALIGNMENT)
    } else {
        size
    })
}

/// Decodes the base level from its guest bytes (`guest_size` of them, as stored).
pub fn decode(descriptor: &TextureDescriptor, guest: &[u8]) -> Option<Decoded> {
    let layout = layout(descriptor.format)?;
    let mut bytes = guest.to_vec();
    swap_words(&mut bytes, descriptor.endian);
    let (pitch, _) = storage_blocks(descriptor, layout);
    let width_blocks = descriptor.width.div_ceil(layout.block_size);
    let height_blocks = descriptor.height.div_ceil(layout.block_size);
    let block_bytes = layout.bytes_per_block as usize;
    // Blocks in row order, whatever the storage layout.
    let mut blocks = Vec::with_capacity((width_blocks * height_blocks) as usize * block_bytes);
    for y in 0..height_blocks {
        for x in 0..width_blocks {
            let offset = if descriptor.tiled {
                tiled_offset(x, y, pitch, layout.bytes_per_block.trailing_zeros())
            } else {
                (y * pitch + x) * layout.bytes_per_block
            } as usize;
            blocks.extend_from_slice(bytes.get(offset..offset + block_bytes)?);
        }
    }
    let (width, height) = (descriptor.width as usize, descriptor.height as usize);
    Some(match descriptor.format {
        FORMAT_8 => Decoded {
            format: HostFormat::R8,
            data: blocks,
        },
        FORMAT_8_8 => Decoded {
            format: HostFormat::Rg8,
            data: blocks,
        },
        FORMAT_8_8_8_8 => Decoded {
            format: HostFormat::Rgba8,
            data: blocks,
        },
        FORMAT_5_6_5 | FORMAT_1_5_5_5 | FORMAT_4_4_4_4 => Decoded {
            format: HostFormat::Rgba8,
            data: blocks
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|&texel| unpack_16_bit(descriptor.format, u16::from_le_bytes(texel)))
                .collect(),
        },
        FORMAT_DXT1 | FORMAT_DXT2_3 | FORMAT_DXT4_5 => Decoded {
            format: HostFormat::Rgba8,
            data: decode_dxt(descriptor.format, &blocks, width, height),
        },
        _ => return None,
    })
}

/// Applies the GPU's byte order (xenos::Endian) to every 32-bit word.
pub fn swap_words(data: &mut [u8], endian: u32) {
    const EIGHT_IN_16: u32 = 1;
    const EIGHT_IN_32: u32 = 2;
    const SIXTEEN_IN_32: u32 = 3;
    for word in data.as_chunks_mut::<4>().0 {
        match endian {
            EIGHT_IN_16 => {
                word.swap(0, 1);
                word.swap(2, 3);
            }
            EIGHT_IN_32 => word.reverse(),
            SIXTEEN_IN_32 => word.rotate_left(2),
            _ => {}
        }
    }
}

/// The byte offset of block (x, y) in a tiled 2D texture whose pitch is `pitch` blocks (a
/// multiple of 32): Xenia's `texture_address::Tiled2D`.
fn tiled_offset(x: u32, y: u32, pitch: u32, bytes_per_block_log2: u32) -> u32 {
    const MACRO_TILE_LOG2: u32 = 5;
    const MACRO_TILE_BLOCKS_LOG2: u32 = 6;
    let outer = ((y >> MACRO_TILE_LOG2) * (pitch >> MACRO_TILE_LOG2) + (x >> MACRO_TILE_LOG2))
        << MACRO_TILE_BLOCKS_LOG2;
    let inner = (((y >> 1) & 0b111) << 3) | (x & 0b111);
    let bytes = (outer | inner) << bytes_per_block_log2;
    let bank = (y >> 4) & 0b1;
    let pipe = ((x >> 3) & 0b11) ^ (((y >> 3) & 0b1) << 1);
    ((y & 1) << 4)
        | (pipe << 6)
        | (bank << 11)
        | (bytes & 0b1111)
        | (((bytes >> 4) & 0b1) << 5)
        | (((bytes >> 5) & 0b111) << 8)
        | ((bytes >> 8) << 12)
}

/// A 16-bit texel as RGBA, X from the lowest bits as the console orders components.
fn unpack_16_bit(format: u32, texel: u16) -> [u8; 4] {
    let field = |shift: u32, bits: u32| {
        let value = u32::from(texel >> shift) & ((1 << bits) - 1);
        (value * u32::from(u8::MAX) / ((1 << bits) - 1)) as u8
    };
    match format {
        FORMAT_5_6_5 => [field(0, 5), field(5, 6), field(11, 5), u8::MAX],
        FORMAT_1_5_5_5 => [field(0, 5), field(5, 5), field(10, 5), field(15, 1)],
        _ => [field(0, 4), field(4, 4), field(8, 4), field(12, 4)],
    }
}

/// DXT (BC1-3) blocks in row order to tightly packed RGBA.
fn decode_dxt(format: u32, blocks: &[u8], width: usize, height: usize) -> Vec<u8> {
    const BLOCK_TEXELS: usize = DXT_BLOCK_SIZE as usize;
    let block_bytes = if format == FORMAT_DXT1 { 8 } else { 16 };
    let width_blocks = width.div_ceil(BLOCK_TEXELS);
    let mut rgba = vec![0; width * height * 4];
    for (index, block) in blocks.chunks_exact(block_bytes).enumerate() {
        let (block_x, block_y) = (index % width_blocks, index / width_blocks);
        let texels = match format {
            FORMAT_DXT1 => color_block(block, true),
            FORMAT_DXT2_3 => {
                let mut texels = color_block(&block[8..], false);
                let alphas = u64::from_le_bytes(block[..8].try_into().unwrap());
                for (texel, color) in texels.iter_mut().enumerate() {
                    color[3] = ((alphas >> (4 * texel)) & 0xF) as u8 * 0x11;
                }
                texels
            }
            _ => {
                let mut texels = color_block(&block[8..], false);
                let alphas = alpha_palette(block[0], block[1]);
                let indices = u64::from_le_bytes([
                    block[2], block[3], block[4], block[5], block[6], block[7], 0, 0,
                ]);
                for (texel, color) in texels.iter_mut().enumerate() {
                    color[3] = alphas[((indices >> (3 * texel)) & 0b111) as usize];
                }
                texels
            }
        };
        for (texel, color) in texels.iter().enumerate() {
            let x = block_x * BLOCK_TEXELS + texel % BLOCK_TEXELS;
            let y = block_y * BLOCK_TEXELS + texel / BLOCK_TEXELS;
            if x < width && y < height {
                let offset = (y * width + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(color);
            }
        }
    }
    rgba
}

/// A BC1 colour block's sixteen texels. `allow_transparent` lets the second endpoint order
/// select three colours and transparent black, as DXT1 does.
fn color_block(block: &[u8], allow_transparent: bool) -> [[u8; 4]; 16] {
    let expand = |color: u16| -> [u32; 3] {
        let (r, g, b) = (
            u32::from(color >> 11),
            u32::from((color >> 5) & 0x3F),
            u32::from(color & 0x1F),
        );
        [
            (r << 3) | (r >> 2),
            (g << 2) | (g >> 4),
            (b << 3) | (b >> 2),
        ]
    };
    let first = u16::from_le_bytes([block[0], block[1]]);
    let second = u16::from_le_bytes([block[2], block[3]]);
    let (a, b) = (expand(first), expand(second));
    let mix = |weight_a: u32, weight_b: u32, total: u32| -> [u8; 4] {
        let channel = |i: usize| ((a[i] * weight_a + b[i] * weight_b) / total) as u8;
        [channel(0), channel(1), channel(2), u8::MAX]
    };
    let palette = if first > second || !allow_transparent {
        [mix(1, 0, 1), mix(0, 1, 1), mix(2, 1, 3), mix(1, 2, 3)]
    } else {
        [mix(1, 0, 1), mix(0, 1, 1), mix(1, 1, 2), [0; 4]]
    };
    let indices = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    std::array::from_fn(|texel| palette[((indices >> (2 * texel)) & 0b11) as usize])
}

/// A BC3 alpha block's eight alphas from its two endpoints.
fn alpha_palette(first: u8, second: u8) -> [u8; 8] {
    let (a, b) = (u32::from(first), u32::from(second));
    if first > second {
        std::array::from_fn(|index| match index {
            0 => first,
            1 => second,
            _ => ((a * (8 - index as u32) + b * (index as u32 - 1)) / 7) as u8,
        })
    } else {
        std::array::from_fn(|index| match index {
            0 => first,
            1 => second,
            6 => 0,
            7 => u8::MAX,
            _ => ((a * (6 - index as u32) + b * (index as u32 - 1)) / 5) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(format: u32, width: u32, height: u32, tiled: bool) -> TextureDescriptor {
        TextureDescriptor {
            tiled,
            pitch: width,
            format,
            endian: 0,
            base: 0,
            width,
            height,
            dimension: 1,
            swizzle: 0,
            clamp_x: 0,
            clamp_y: 0,
            mag_filter: 0,
            min_filter: 0,
        }
    }

    #[test]
    fn tiled_offsets_interleave_rows_and_columns() {
        const BYTES_PER_TEXEL_LOG2: u32 = 2;
        const PITCH: u32 = 32;
        // Neighbours in a row are adjacent; the next row is 16 bytes on; the eighth column
        // moves to the next pipe.
        assert_eq!(tiled_offset(0, 0, PITCH, BYTES_PER_TEXEL_LOG2), 0);
        assert_eq!(tiled_offset(1, 0, PITCH, BYTES_PER_TEXEL_LOG2), 4);
        assert_eq!(tiled_offset(0, 1, PITCH, BYTES_PER_TEXEL_LOG2), 16);
        assert_eq!(tiled_offset(8, 0, PITCH, BYTES_PER_TEXEL_LOG2), 1 << 6);
    }

    #[test]
    fn tiling_round_trips_every_texel() {
        // A 32x32 tiled 8-bit texture whose texel at (x, y) holds a distinct value.
        let texture = descriptor(FORMAT_8, 32, 32, true);
        let mut guest = vec![0u8; guest_size(&texture).unwrap() as usize];
        for y in 0..32 {
            for x in 0..32 {
                guest[tiled_offset(x, y, 32, 0) as usize] = (x + y * 7) as u8;
            }
        }
        let decoded = decode(&texture, &guest).unwrap();
        assert_eq!(decoded.format, HostFormat::R8);
        assert!((0..32 * 32).all(|i| decoded.data[i as usize] == ((i % 32) + (i / 32) * 7) as u8));
    }

    #[test]
    fn dxt1_blocks_decode_their_endpoints() {
        // One 4x4 block: red and blue endpoints, every texel index 1 (the second endpoint)
        // except texel 0 (index 0).
        const RED: u16 = 0xF800;
        const BLUE: u16 = 0x001F;
        let mut block = Vec::new();
        block.extend(RED.to_le_bytes());
        block.extend(BLUE.to_le_bytes());
        block.extend((0x5555_5554u32).to_le_bytes());
        let decoded = decode(&descriptor(FORMAT_DXT1, 4, 4, false), &block).unwrap();
        assert_eq!(&decoded.data[..4], &[255, 0, 0, 255]);
        assert_eq!(&decoded.data[4..8], &[0, 0, 255, 255]);
    }

    #[test]
    fn dxt5_alpha_interpolates_between_endpoints() {
        assert_eq!(alpha_palette(255, 0)[2], 218);
        assert_eq!(alpha_palette(0, 255)[6], 0);
        assert_eq!(alpha_palette(0, 255)[7], 255);
    }

    #[test]
    fn sixteen_bit_texels_expand_to_eight_bits() {
        assert_eq!(unpack_16_bit(FORMAT_5_6_5, 0x001F), [255, 0, 0, 255]);
        assert_eq!(unpack_16_bit(FORMAT_4_4_4_4, 0xF000), [0, 0, 0, 255]);
    }
}
