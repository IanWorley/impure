//! The geometry of EDRAM rendering: render target surfaces in samples, the window offset and
//! scissor, and the rectangle a resolve copies. Pure register arithmetic, following Xenia's
//! `draw_util.cc`.
//!
//! The renderer keeps each EDRAM surface as a host texture at sample resolution: a 4x
//! multisampled surface stores 2x2 samples per pixel, as EDRAM does, and resolves average
//! them. Surfaces that alias in EDRAM sample space alias on the host too: titles clear a 1x
//! surface by drawing to a 4x surface of half its pitch.

/// EDRAM holds 10 MiB of 32-bit samples.
const EDRAM_SIZE: u32 = 10 << 20;
const BYTES_PER_SAMPLE: u32 = 4;
/// The tallest host surface; EDRAM bounds narrow surfaces far beyond it.
const MAXIMUM_HEIGHT_SAMPLES: u32 = 4096;
const SURFACE_PITCH_MASK: u32 = 0x3FFF;
const MSAA_SHIFT: u32 = 16;
const MSAA_MASK: u32 = 0x3;
/// xenos::MsaaSamples: 2x stacks samples vertically, 4x in a 2x2 square.
const MSAA_2X: u32 = 1;
const MSAA_4X: u32 = 2;
/// Resolves cover whole 8x8-pixel blocks.
const RESOLVE_ALIGNMENT: u32 = 8;
/// Signed 15-bit fields: window offsets.
const OFFSET_BITS: u32 = 15;
const OFFSET_Y_SHIFT: u32 = 16;
/// 14-bit scissor coordinates, and the bit that ignores the window offset.
const SCISSOR_MASK: u32 = 0x3FFF;
const SCISSOR_Y_SHIFT: u32 = 16;
const SCISSOR_WINDOW_OFFSET_DISABLE: u32 = 1 << 31;
/// RB_DEPTH_INFO: the depth surface holds 20e4 floats (D24FS8) rather than 24-bit unorm.
const DEPTH_FORMAT_FLOAT: u32 = 1 << 16;
/// RB_DEPTH_CLEAR: 24-bit depth above 8-bit stencil.
const DEPTH_CLEAR_SHIFT: u32 = 8;
const UNORM_24_MAXIMUM: f32 = 0xFF_FFFF as f32;
/// The 20e4 format: a 4-bit exponent biased by 15 above a 20-bit mantissa, no sign.
const FLOAT_20E4_MANTISSA_BITS: u32 = 20;
const FLOAT_20E4_MANTISSA_MASK: u32 = (1 << FLOAT_20E4_MANTISSA_BITS) - 1;
/// Rebiases a 20e4 exponent to f32's (127 - 15), and widens the mantissa to f32's 23 bits.
const FLOAT_20E4_EXPONENT_REBIAS: u32 = 112;
const F32_MANTISSA_BITS: u32 = 23;

/// A render target surface from RB_SURFACE_INFO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surface {
    /// Row pitch in pixels.
    pub pitch: u32,
    /// Samples per pixel along X and Y, as shifts.
    pub sample_shift: (u32, u32),
}

impl Surface {
    pub fn decode(surface_info: u32) -> Self {
        let msaa = (surface_info >> MSAA_SHIFT) & MSAA_MASK;
        Self {
            pitch: surface_info & SURFACE_PITCH_MASK,
            sample_shift: (u32::from(msaa >= MSAA_4X), u32::from(msaa >= MSAA_2X)),
        }
    }

    /// The surface with 2x multisampling rendered at one sample per pixel, which halves the
    /// work of drawing to it. 4x surfaces keep their samples: 1x surfaces alias them at twice
    /// their pitch, which only holds in sample space.
    pub fn without_vertical_samples(self) -> Self {
        if self.sample_shift.0 == 0 {
            Self {
                sample_shift: (0, 0),
                ..self
            }
        } else {
            self
        }
    }

    /// The host texture's size in samples: the pitch, and as many rows as EDRAM holds.
    pub fn host_size(self) -> (u32, u32) {
        let width = self.pitch << self.sample_shift.0;
        let height = (EDRAM_SIZE / (width.max(1) * BYTES_PER_SAMPLE)).min(MAXIMUM_HEIGHT_SAMPLES);
        (width, height)
    }

    /// A rectangle in pixels as samples.
    pub fn samples(self, rectangle: Rectangle) -> Rectangle {
        let (x, y) = self.sample_shift;
        Rectangle {
            left: rectangle.left << x,
            top: rectangle.top << y,
            right: rectangle.right << x,
            bottom: rectangle.bottom << y,
        }
    }
}

/// A rectangle, right and bottom exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rectangle {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Rectangle {
    pub fn is_empty(self) -> bool {
        self.left >= self.right || self.top >= self.bottom
    }

    pub fn clamp(self, width: u32, height: u32) -> Self {
        Self {
            left: self.left.min(width),
            top: self.top.min(height),
            right: self.right.min(width),
            bottom: self.bottom.min(height),
        }
    }
}

/// PA_SC_WINDOW_OFFSET as signed pixels.
pub fn window_offset(register: u32) -> (i32, i32) {
    let field = |value: u32| ((value << (32 - OFFSET_BITS)) as i32) >> (32 - OFFSET_BITS);
    (field(register), field(register >> OFFSET_Y_SHIFT))
}

/// The window scissor in pixels, moved by the window offset unless its top-left register
/// says not to.
pub fn scissor(top_left: u32, bottom_right: u32, window_offset: (i32, i32)) -> Rectangle {
    let (dx, dy) = if top_left & SCISSOR_WINDOW_OFFSET_DISABLE != 0 {
        (0, 0)
    } else {
        window_offset
    };
    let coordinate = |value: u32, shift: u32, offset: i32| {
        (((value >> shift) & SCISSOR_MASK) as i32 + offset).max(0) as u32
    };
    Rectangle {
        left: coordinate(top_left, 0, dx),
        top: coordinate(top_left, SCISSOR_Y_SHIFT, dy),
        right: coordinate(bottom_right, 0, dx),
        bottom: coordinate(bottom_right, SCISSOR_Y_SHIFT, dy),
    }
}

/// The depth a resolve clears the depth surface to, in the format RB_DEPTH_INFO gives it.
pub fn depth_clear_value(clear: u32, depth_info: u32) -> f32 {
    let depth = clear >> DEPTH_CLEAR_SHIFT;
    if depth_info & DEPTH_FORMAT_FLOAT == 0 {
        return depth as f32 / UNORM_24_MAXIMUM;
    }
    // As Xenia's Float20e4To32: a zero exponent holds a denormal, normalized here.
    let (mut exponent, mut mantissa) = (
        depth >> FLOAT_20E4_MANTISSA_BITS,
        depth & FLOAT_20E4_MANTISSA_MASK,
    );
    if exponent == 0 {
        if mantissa == 0 {
            return 0.0;
        }
        let shift = mantissa.leading_zeros() - (u32::BITS - FLOAT_20E4_MANTISSA_BITS - 1);
        exponent = 1u32.wrapping_sub(shift);
        mantissa = (mantissa << shift) & FLOAT_20E4_MANTISSA_MASK;
    }
    f32::from_bits(
        (exponent.wrapping_add(FLOAT_20E4_EXPONENT_REBIAS) << F32_MANTISSA_BITS)
            | (mantissa << (F32_MANTISSA_BITS - FLOAT_20E4_MANTISSA_BITS)),
    )
}

/// The pixels a resolve copies: the bounds of its three vertices (Direct3D puts them in vertex
/// fetch constant 0), rounded by the top-left rule, moved by the window offset, clamped to the
/// scissor and the surface pitch, and widened to whole 8x8 blocks.
pub fn resolve_rectangle(
    vertices: [f32; 6],
    half_pixel: bool,
    window_offset: (i32, i32),
    scissor: Rectangle,
    pitch: u32,
) -> Option<Rectangle> {
    if scissor.is_empty() {
        return None;
    }
    // Xenia's 16.8 fixed point: a centre at .5 is covered at the top-left, not the bottom-right.
    const FIXED_ONE: f32 = 256.0;
    const ROUNDING: i32 = 127;
    const FIXED_SHIFT: i32 = 8;
    let offset = if half_pixel { 0.5 } else { 0.0 };
    let fixed = |value: f32| ((value + offset) * FIXED_ONE) as i32;
    let xs = [vertices[0], vertices[2], vertices[4]].map(fixed);
    let ys = [vertices[1], vertices[3], vertices[5]].map(fixed);
    let round = |value: i32| (value + ROUNDING) >> FIXED_SHIFT;
    let (dx, dy) = window_offset;
    let clamp_x = |value: i32| (value + dx).clamp(scissor.left as i32, scissor.right as i32) as u32;
    let clamp_y = |value: i32| (value + dy).clamp(scissor.top as i32, scissor.bottom as i32) as u32;
    let align_down = |value: u32| value / RESOLVE_ALIGNMENT * RESOLVE_ALIGNMENT;
    let rectangle = Rectangle {
        left: align_down(clamp_x(round(*xs.iter().min()?))),
        top: align_down(clamp_y(round(*ys.iter().min()?))),
        right: clamp_x(round(*xs.iter().max()?))
            .next_multiple_of(RESOLVE_ALIGNMENT)
            .min(align_down(pitch)),
        bottom: clamp_y(round(*ys.iter().max()?)).next_multiple_of(RESOLVE_ALIGNMENT),
    };
    (!rectangle.is_empty()).then_some(rectangle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_4x_surface_of_half_pitch_covers_the_same_samples_as_a_1x_surface() {
        const PITCH_640_4X: u32 = 0x0002_0280;
        const PITCH_1280_1X: u32 = 0x0000_0500;
        let multisampled = Surface::decode(PITCH_640_4X);
        assert_eq!(multisampled.sample_shift, (1, 1));
        assert_eq!(
            multisampled.host_size().0,
            Surface::decode(PITCH_1280_1X).host_size().0
        );
    }

    #[test]
    fn window_offsets_are_signed() {
        const MINUS_ONE_X_TWO_Y: u32 = 0x7FFF | (2 << 16);
        assert_eq!(window_offset(MINUS_ONE_X_TWO_Y), (-1, 2));
    }

    #[test]
    fn full_screen_resolves_cover_the_screen() {
        // Direct3D's resolve rectangle for 1280x720, with the -0.5 pixel offset it applies.
        let vertices = [-0.5, -0.5, 1279.5, -0.5, 1279.5, 719.5];
        let scissor = scissor(0, 1280 | (720 << 16), (0, 0));
        assert_eq!(
            resolve_rectangle(vertices, true, (0, 0), scissor, 1280),
            Some(Rectangle {
                left: 0,
                top: 0,
                right: 1280,
                bottom: 720
            })
        );
    }

    #[test]
    fn only_2x_surfaces_drop_their_vertical_samples() {
        const PITCH_1280_2X: u32 = 0x0001_0500;
        const PITCH_640_4X: u32 = 0x0002_0280;
        assert_eq!(
            Surface::decode(PITCH_1280_2X)
                .without_vertical_samples()
                .sample_shift,
            (0, 0)
        );
        assert_eq!(
            Surface::decode(PITCH_640_4X)
                .without_vertical_samples()
                .sample_shift,
            (1, 1)
        );
    }

    #[test]
    fn depth_clears_follow_the_depth_surface_format() {
        const FLOAT_DEPTH: u32 = 1 << 16;
        const UNORM_ONE: u32 = 0xFF_FFFF << 8;
        // 1.0 and 0.5 as 20e4 floats: exponents 15 and 14 (bias 15), zero mantissas.
        const FLOAT_ONE: u32 = 0xF0_0000 << 8;
        const FLOAT_HALF: u32 = 0xE0_0000 << 8;
        assert_eq!(depth_clear_value(UNORM_ONE, 0), 1.0);
        assert_eq!(depth_clear_value(FLOAT_ONE, FLOAT_DEPTH), 1.0);
        assert_eq!(depth_clear_value(FLOAT_HALF, FLOAT_DEPTH), 0.5);
        assert_eq!(depth_clear_value(0, FLOAT_DEPTH), 0.0);
        // The smallest denormal: a mantissa of 1 under a zero exponent.
        const FLOAT_SMALLEST: u32 = 1 << 8;
        assert_eq!(
            depth_clear_value(FLOAT_SMALLEST, FLOAT_DEPTH),
            2f32.powi(-34)
        );
    }

    #[test]
    fn an_inverted_scissor_resolves_nothing() {
        let vertices = [-0.5, -0.5, 1279.5, -0.5, 1279.5, 719.5];
        let inverted = scissor(1280 | (720 << 16), 0, (0, 0));
        assert_eq!(
            resolve_rectangle(vertices, true, (0, 0), inverted, 1280),
            None
        );
    }

    #[test]
    fn resolves_follow_the_window_offset() {
        // A tile rendered 256 rows up resolves rows 256-511 of the screen from EDRAM rows 0-255.
        let vertices = [-0.5, 255.5, 1279.5, 255.5, 1279.5, 511.5];
        let resolved = resolve_rectangle(
            vertices,
            true,
            (0, -256),
            scissor(0, 1280 | (256 << 16), (0, 0)),
            1280,
        );
        assert_eq!(
            resolved.map(|rectangle| (rectangle.top, rectangle.bottom)),
            Some((0, 256))
        );
    }
}
