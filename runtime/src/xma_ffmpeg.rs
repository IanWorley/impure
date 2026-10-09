//! XMA frames decoded by FFmpeg (the `audio` feature), linked dynamically from the system's
//! install so its LGPL code stays a separate library.
//!
//! An XMA frame is a WMA Pro frame. FFmpeg's own XMA decoders take whole packets and hold back
//! 4,096 samples to keep multi-stream files in step, so a sound's last samples would only appear
//! once the stream ended; the hardware emits each frame's samples as it decodes it. So, as Xenia
//! did with upstream FFmpeg, each frame goes to FFmpeg's WMA Pro decoder on its own, in a packet
//! made for it: a 4-bit sequence number, 2 unused bits, a 15-bit count of bits carried over from
//! the previous packet (none), then the frame. WMA Pro configured with XMA's decode flags
//! decodes 512 samples a frame at every XMA sample rate; on a set of real sounds the output
//! matches FFmpeg's XMA2 decoder sample for sample.
//!
//! A stream's channel count is not taken from its context: Pure's FMOD plays every sound
//! through contexts marked mono, stereo music included, and decoding stereo frames as mono
//! fails on nearly every frame. So a stream's first frames are decoded both ways, and the way
//! FFmpeg parses without errors is kept.

use std::cell::Cell;
use std::ptr;
use std::sync::Once;

use bevy::log::{debug, warn};
use ffmpeg_sys_next as ffmpeg;

use crate::xma::FrameDecoder;

/// XMA's WMA Pro decode flags (as FFmpeg's XMA decoders set them): length-prefixed frames, 512
/// samples a frame.
const XMA_DECODE_FLAGS: u16 = 0x10D6;
const BITS_PER_SAMPLE: u16 = 16;
/// WMA Pro's extradata: bits per sample at 0, the channel mask at 2 (none: use the channel
/// count) and the decode flags at 14.
const EXTRADATA_SIZE: usize = 18;
const EXTRADATA_DECODE_FLAGS: usize = 14;
/// The decoder reads frame lengths in log2(block align) + 4 bits; XMA's are 15 bits, so the
/// block align must be 2,048 to 4,095. The largest holds every valid frame in one packet.
const BLOCK_ALIGN: usize = 4095;
const SEQUENCE_BITS: u32 = 4;
const SEQUENCE_MASK: u8 = 0xF;
const UNUSED_HEADER_BITS: u32 = 2;
const CARRIED_OVER_BITS: u32 = 15;
const HEADER_BITS: u32 = SEQUENCE_BITS + UNUSED_HEADER_BITS + CARRIED_OVER_BITS;
pub const SAMPLES_PER_FRAME: usize = 512;
const BYTES_PER_SAMPLE: usize = 2;
const MONO: usize = 1;
const STEREO: usize = 2;

thread_local! {
    /// Whether FFmpeg reported an error since this was last cleared. Frames decode on the
    /// thread that asks, so FFmpeg logs its errors on that thread too.
    static FFMPEG_ERROR: Cell<bool> = const { Cell::new(false) };
}

/// The `va_list` parameter of FFmpeg's log callback. x86-64's System V `va_list` is a
/// one-element array, which decays to a pointer as a parameter; elsewhere it is passed as is.
#[cfg(all(target_arch = "x86_64", not(windows)))]
type LogArguments = *mut ffmpeg::__va_list_tag;
#[cfg(not(all(target_arch = "x86_64", not(windows))))]
type LogArguments = ffmpeg::va_list;

/// Notes FFmpeg's errors instead of printing them: a frame decoded the wrong way logs several.
unsafe extern "C" fn log_callback(
    _: *mut std::ffi::c_void,
    level: std::ffi::c_int,
    _: *const std::ffi::c_char,
    _: LogArguments,
) {
    if level <= ffmpeg::AV_LOG_ERROR as std::ffi::c_int {
        FFMPEG_ERROR.with(|error| error.set(true));
    }
}

/// One FFmpeg WMA Pro decoder, for a given channel count.
struct Codec {
    context: *mut ffmpeg::AVCodecContext,
    packet: *mut ffmpeg::AVPacket,
    frame: *mut ffmpeg::AVFrame,
    channels: usize,
    sequence: u8,
}

/// A frame's samples, channels interleaved.
type Samples = Vec<f32>;

impl Codec {
    fn open(channels: usize, sample_rate: u32) -> Option<Self> {
        // SAFETY: plain FFmpeg allocation and setup; every pointer is checked before use, and a
        // partly built codec is freed by Drop.
        unsafe {
            let codec = ffmpeg::avcodec_find_decoder(ffmpeg::AVCodecID::AV_CODEC_ID_WMAPRO);
            if codec.is_null() {
                return None;
            }
            let opened = Self {
                context: ffmpeg::avcodec_alloc_context3(codec),
                packet: ffmpeg::av_packet_alloc(),
                frame: ffmpeg::av_frame_alloc(),
                channels,
                sequence: 0,
            };
            if opened.context.is_null() || opened.packet.is_null() || opened.frame.is_null() {
                return None;
            }
            let context = &mut *opened.context;
            context.sample_rate = sample_rate as i32;
            context.block_align = BLOCK_ALIGN as i32;
            ffmpeg::av_channel_layout_default(&mut context.ch_layout, channels as i32);
            let padding = ffmpeg::AV_INPUT_BUFFER_PADDING_SIZE as usize;
            let extradata = ffmpeg::av_mallocz(EXTRADATA_SIZE + padding).cast::<u8>();
            if extradata.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts_mut(extradata, EXTRADATA_SIZE);
            bytes[..2].copy_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
            bytes[EXTRADATA_DECODE_FLAGS..EXTRADATA_DECODE_FLAGS + 2]
                .copy_from_slice(&XMA_DECODE_FLAGS.to_le_bytes());
            context.extradata = extradata;
            context.extradata_size = EXTRADATA_SIZE as i32;
            (ffmpeg::avcodec_open2(opened.context, codec, ptr::null_mut()) >= 0).then_some(opened)
        }
    }

    /// The frame's samples: None if FFmpeg reported an error, empty if it produced none (it
    /// holds back a stream's first frame).
    fn decode(&mut self, frame: &[u8], bits: u32) -> Option<Samples> {
        let packet = self.packet_for(frame, bits)?;
        FFMPEG_ERROR.with(|error| error.set(false));
        let mut samples = Vec::new();
        // SAFETY: the packet is allocated with FFmpeg's padding and filled within its size; the
        // decoded frame is planar float with `channels` planes of `nb_samples` samples.
        unsafe {
            if ffmpeg::av_new_packet(self.packet, packet.len() as i32) < 0 {
                return None;
            }
            ptr::copy_nonoverlapping(packet.as_ptr(), (*self.packet).data, packet.len());
            let sent = ffmpeg::avcodec_send_packet(self.context, self.packet);
            ffmpeg::av_packet_unref(self.packet);
            if sent < 0 {
                return None;
            }
            while ffmpeg::avcodec_receive_frame(self.context, self.frame) >= 0 {
                let decoded = &*self.frame;
                let count = (decoded.nb_samples as usize).min(SAMPLES_PER_FRAME);
                let planes: Vec<*const f32> = (0..self.channels)
                    .map(|channel| *decoded.extended_data.add(channel) as *const f32)
                    .collect();
                for index in 0..count {
                    samples.extend(planes.iter().map(|plane| *plane.add(index)));
                }
                ffmpeg::av_frame_unref(self.frame);
            }
        }
        (!FFMPEG_ERROR.with(Cell::get)).then_some(samples)
    }

    /// A packet holding the frame alone, as the decoder reads one.
    fn packet_for(&mut self, frame: &[u8], bits: u32) -> Option<Vec<u8>> {
        if (HEADER_BITS + bits) as usize > BLOCK_ALIGN * 8 {
            return None;
        }
        let mut packet = vec![0u8; BLOCK_ALIGN];
        // The sequence number is the first byte's top bits; the carried-over count stays 0.
        packet[0] = self.sequence << (u8::BITS - SEQUENCE_BITS);
        self.sequence = (self.sequence + 1) & SEQUENCE_MASK;
        for bit in 0..bits {
            let value = (frame[(bit / 8) as usize] >> (7 - bit % 8)) & 1;
            // The frame's last bit says whether another frame follows in its packet; here none
            // does.
            if bit + 1 == bits || value == 0 {
                continue;
            }
            let at = HEADER_BITS + bit;
            packet[(at / 8) as usize] |= 1 << (7 - at % 8);
        }
        Some(packet)
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: each free function accepts a null pointer and nulls what it frees.
        unsafe {
            ffmpeg::av_frame_free(&mut self.frame);
            ffmpeg::av_packet_free(&mut self.packet);
            ffmpeg::avcodec_free_context(&mut self.context);
        }
    }
}

/// One stream's decoder: a mono and a stereo codec until the stream's frames show which it is.
pub struct Decoder {
    codecs: Vec<Codec>,
    /// Decided once one way decodes cleanly and the other does not.
    decided: bool,
    /// The samples of the frame `channels` judged, for `decode` to write.
    held: Option<Samples>,
}

// SAFETY: the FFmpeg objects are used only through `&mut self`, by one thread at a time.
unsafe impl Send for Decoder {}

impl Decoder {
    fn new(sample_rate: u32) -> Option<Self> {
        static LOGGING: Once = Once::new();
        // SAFETY: installs a callback that only touches a thread-local flag.
        LOGGING.call_once(|| unsafe { ffmpeg::av_log_set_callback(Some(log_callback)) });
        let codecs: Vec<Codec> = [MONO, STEREO]
            .into_iter()
            .filter_map(|channels| Codec::open(channels, sample_rate))
            .collect();
        (!codecs.is_empty()).then_some(Self {
            codecs,
            decided: false,
            held: None,
        })
    }
}

impl FrameDecoder for Decoder {
    fn channels(&mut self, frame: &[u8], bits: u32, hint: usize) -> usize {
        if !self.decided {
            let results: Vec<Option<Samples>> = self
                .codecs
                .iter_mut()
                .map(|codec| codec.decode(frame, bits))
                .collect();
            let clean: Vec<usize> = (0..results.len())
                .filter(|&i| results[i].is_some())
                .collect();
            // One way parses and the other does not: that is the stream. Until then, the
            // context's word stands, and the frame is silent if neither parses.
            let chosen = match clean.as_slice() {
                [only] => {
                    self.decided = true;
                    *only
                }
                _ => (0..self.codecs.len())
                    .find(|&i| self.codecs[i].channels == hint)
                    .unwrap_or(0),
            };
            if self.decided {
                let channels = self.codecs[chosen].channels;
                if channels != hint {
                    debug!(target: "xma", "a stream its context calls {hint}-channel decodes as {channels}-channel");
                }
                self.codecs.swap(0, chosen);
                self.codecs.truncate(1);
                self.held = results.into_iter().nth(chosen).flatten();
            } else {
                self.codecs.swap(0, chosen);
                self.held = results
                    .into_iter()
                    .nth(chosen)
                    .flatten()
                    .or(Some(Vec::new()));
            }
        }
        self.codecs[0].channels
    }

    fn decode(&mut self, frame: &[u8], bits: u32, output: &mut [u8]) -> bool {
        let samples = match self.held.take() {
            Some(samples) => samples,
            None => match self.codecs[0].decode(frame, bits) {
                Some(samples) => samples,
                None => return false,
            },
        };
        output.fill(0);
        for (bytes, sample) in output.chunks_exact_mut(BYTES_PER_SAMPLE).zip(samples) {
            let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
            bytes.copy_from_slice(&value.to_be_bytes());
        }
        true
    }
}

/// Opens a decoder for a stream at `sample_rate`, logging once if FFmpeg cannot.
pub fn open(sample_rate: u32) -> Option<Decoder> {
    static WARNED: Once = Once::new();
    let decoder = Decoder::new(sample_rate);
    if decoder.is_none() {
        WARNED.call_once(|| warn!(target: "xma", "FFmpeg could not open a WMA Pro decoder; XMA audio stays silent"));
    }
    decoder
}
