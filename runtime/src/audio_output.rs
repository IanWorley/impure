//! Plays the frames the title's audio mixer submits (the `audio` feature): 256 samples of six
//! channels each, downmixed to stereo and queued for the host's default output device at the
//! console's 48 kHz. `PURE_AUDIO_CAPTURE=<file.wav>` also records them, so a run can be checked
//! without listening.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::thread;

use bevy::log::{info, warn};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub const SAMPLE_RATE_HZ: u32 = 48_000;
const OUTPUT_CHANNELS: u16 = 2;
/// At most this much audio waits to play; older samples are dropped, so a stall in the title
/// does not leave sound lagging behind the picture.
const MAXIMUM_QUEUED_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 4 * OUTPUT_CHANNELS as usize;
const CAPTURE_ENVIRONMENT: &str = "PURE_AUDIO_CAPTURE";
const WAV_HEADER_SIZE: u32 = 44;
const WAV_BITS_PER_SAMPLE: u16 = 16;
const WAV_PCM_FORMAT: u16 = 1;
const WAV_FMT_CHUNK_SIZE: u32 = 16;
/// Where the RIFF size and the data chunk's size live in the header.
const WAV_RIFF_SIZE_OFFSET: u64 = 4;
const WAV_DATA_SIZE_OFFSET: u64 = 40;

/// Interleaved stereo samples waiting for the output device.
static QUEUE: Mutex<VecDeque<f32>> = Mutex::new(VecDeque::new());
static STARTED: OnceLock<()> = OnceLock::new();
static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);

/// Queues interleaved stereo samples (-1 to 1) to play, starting the output on first use.
pub fn play(stereo: &[f32]) {
    STARTED.get_or_init(start);
    {
        let mut queue = QUEUE.lock().unwrap();
        queue.extend(stereo);
        let excess = queue.len().saturating_sub(MAXIMUM_QUEUED_SAMPLES);
        queue.drain(..excess);
    }
    if let Some(capture) = CAPTURE.lock().unwrap().as_mut() {
        capture.append(stereo);
    }
}

fn start() {
    if let Some(path) = std::env::var_os(CAPTURE_ENVIRONMENT) {
        match Capture::create(PathBuf::from(path)) {
            Ok(capture) => *CAPTURE.lock().unwrap() = Some(capture),
            Err(error) => warn!(target: "audio", "cannot capture audio: {error}"),
        }
    }
    // cpal's streams cannot move between threads on every platform, so one thread owns it.
    let started = thread::Builder::new()
        .name("audio-output".into())
        .spawn(|| match open_stream() {
            Ok(stream) => {
                info!(target: "audio", "playing through the default output device at {SAMPLE_RATE_HZ} Hz");
                // The stream plays as long as it lives: for the life of the process.
                let _stream = stream;
                loop {
                    thread::park();
                }
            }
            Err(error) => warn!(target: "audio", "no audio output: {error}"),
        });
    if let Err(error) = started {
        warn!(target: "audio", "cannot start audio output: {error}");
    }
}

fn open_stream() -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no default output device")?;
    let config = cpal::StreamConfig {
        channels: OUTPUT_CHANNELS,
        sample_rate: SAMPLE_RATE_HZ,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = device
        .build_output_stream(
            config,
            |output: &mut [f32], _| {
                let mut queue = QUEUE.lock().unwrap();
                for sample in output.iter_mut() {
                    *sample = queue.pop_front().unwrap_or(0.0);
                }
            },
            |error| warn!(target: "audio", "output stream: {error}"),
            None,
        )
        .map_err(|error| error.to_string())?;
    stream.play().map_err(|error| error.to_string())?;
    Ok(stream)
}

/// A 16-bit stereo WAV file whose header is kept up to date as samples arrive.
struct Capture {
    file: File,
    data_bytes: u32,
}

impl Capture {
    fn create(path: PathBuf) -> std::io::Result<Self> {
        let mut file = File::create(&path)?;
        let block_align = OUTPUT_CHANNELS * WAV_BITS_PER_SAMPLE / 8;
        let mut header = Vec::with_capacity(WAV_HEADER_SIZE as usize);
        header.extend(b"RIFF");
        header.extend((WAV_HEADER_SIZE - 8).to_le_bytes());
        header.extend(b"WAVEfmt ");
        header.extend(WAV_FMT_CHUNK_SIZE.to_le_bytes());
        header.extend(WAV_PCM_FORMAT.to_le_bytes());
        header.extend(OUTPUT_CHANNELS.to_le_bytes());
        header.extend(SAMPLE_RATE_HZ.to_le_bytes());
        header.extend((SAMPLE_RATE_HZ * u32::from(block_align)).to_le_bytes());
        header.extend(block_align.to_le_bytes());
        header.extend(WAV_BITS_PER_SAMPLE.to_le_bytes());
        header.extend(b"data");
        header.extend(0u32.to_le_bytes());
        file.write_all(&header)?;
        info!(target: "audio", "capturing audio to {}", path.display());
        Ok(Self {
            file,
            data_bytes: 0,
        })
    }

    fn append(&mut self, stereo: &[f32]) {
        let bytes: Vec<u8> = stereo
            .iter()
            .flat_map(|sample| {
                ((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16).to_le_bytes()
            })
            .collect();
        self.data_bytes += bytes.len() as u32;
        let written = self.file.write_all(&bytes).and_then(|()| {
            self.file.seek(SeekFrom::Start(WAV_RIFF_SIZE_OFFSET))?;
            self.file
                .write_all(&(WAV_HEADER_SIZE - 8 + self.data_bytes).to_le_bytes())?;
            self.file.seek(SeekFrom::Start(WAV_DATA_SIZE_OFFSET))?;
            self.file.write_all(&self.data_bytes.to_le_bytes())?;
            self.file.seek(SeekFrom::End(0)).map(|_| ())
        });
        if let Err(error) = written {
            warn!(target: "audio", "audio capture stopped: {error}");
        }
    }
}
