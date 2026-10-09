//! A thread that hands the renderer's recorded work to the GPU queue, so the command processor
//! can record the next draws while the previous ones are finished, uploaded and submitted.
//! Finishing a command encoder (wgpu validates and encodes every recorded call then) and copying
//! a batch's guest memory into the queue took about 40% of the command processor's time.
//!
//! Every queue operation goes through here, in the order the renderer asked for it: a texture
//! written for the next frame must not reach the GPU before the draws of this frame that read
//! the old contents.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use bevy::log::{info, warn};

use crate::renderer::words_as_bytes;

const THREAD_NAME: &str = "gpu-submit";

/// Work for the queue, in the order the renderer recorded it.
pub enum Work {
    WriteTexture {
        texture: wgpu::Texture,
        data: Vec<u8>,
        layout: wgpu::TexelCopyBufferLayout,
    },
    /// Uploads a batch's data, then submits the draws recorded with it. The data's allocations
    /// come back through `Submitter::recycled` once copied, so the renderer can reuse them.
    Submit {
        encoder: wgpu::CommandEncoder,
        uploads: Uploads,
    },
    /// Submits a frame's presentation, then waits for the GPU to finish it, then saves the
    /// display texture to each capture path and each readback the frame recorded.
    Present {
        encoder: wgpu::CommandEncoder,
        frame: u64,
        display: wgpu::Texture,
        captures: Vec<PathBuf>,
        readbacks: Vec<Readback>,
    },
}

/// A texture copied into a buffer by recorded commands, to save as a PNG file once they have run.
pub struct Readback {
    buffer: wgpu::Buffer,
    size: (u32, u32),
    padded_row_bytes: u32,
    format: wgpu::TextureFormat,
    path: PathBuf,
}

impl Readback {
    /// Records a copy of `texture` into a new buffer, for formats `save` understands.
    pub fn record(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        path: PathBuf,
    ) -> Option<Self> {
        let format = texture.format();
        let bytes_per_texel = match format {
            wgpu::TextureFormat::Rgba8Unorm
            | wgpu::TextureFormat::Rgba8UnormSrgb
            | wgpu::TextureFormat::R32Float => 4,
            _ => return None,
        };
        let size = (texture.width(), texture.height());
        let padded_row_bytes =
            (size.0 * bytes_per_texel).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(padded_row_bytes * size.1),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes),
                    rows_per_image: None,
                },
            },
            texture.size(),
        );
        Some(Self {
            buffer,
            size,
            padded_row_bytes,
            format,
            path,
        })
    }

    /// Saves the copy, once the commands that made it have run: colour as RGB (alpha ignored,
    /// as the console's scan-out does), and 32-bit float as grey scaled from its smallest to its
    /// largest finite value, which the log line gives.
    fn save(self, device: &wgpu::Device, limit: Duration) {
        let slice = self.buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(limit),
        });
        let (width, height) = self.size;
        let rows: Vec<Vec<u8>> = slice
            .get_mapped_range()
            .chunks(self.padded_row_bytes as usize)
            .map(|row| row[..(width * 4) as usize].to_vec())
            .collect();
        let mut range = String::new();
        let rgb: Vec<u8> = if self.format == wgpu::TextureFormat::R32Float {
            let values: Vec<f32> = rows
                .iter()
                .flat_map(|row| row.chunks_exact(4))
                .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
                .collect();
            let finite = values.iter().copied().filter(|value| value.is_finite());
            let low = finite.clone().fold(f32::INFINITY, f32::min);
            let high = finite.fold(f32::NEG_INFINITY, f32::max);
            range = format!(" (float {low}..{high})");
            let scale = if high > low {
                255.0 / (high - low)
            } else {
                0.0
            };
            values
                .iter()
                .flat_map(|value| [((value - low) * scale) as u8; 3])
                .collect()
        } else {
            rows.iter()
                .flat_map(|row| {
                    row.chunks_exact(4)
                        .flat_map(|texel| [texel[0], texel[1], texel[2]])
                })
                .collect()
        };
        match image::RgbImage::from_raw(width, height, rgb).map(|frame| frame.save(&self.path)) {
            Some(Ok(())) => info!(target: "renderer", "saved {}{range}", self.path.display()),
            Some(Err(error)) => {
                warn!(target: "renderer", "cannot save {}: {error}", self.path.display())
            }
            None => warn!(target: "renderer", "cannot save {}: wrong size", self.path.display()),
        }
    }
}

/// A batch's draw constants, guest memory and host indices, and the buffers they go to.
pub struct Uploads {
    pub constants: (wgpu::Buffer, Vec<u8>),
    pub memory: (wgpu::Buffer, Vec<u8>),
    pub indices: (wgpu::Buffer, Vec<u32>),
}

/// A batch's emptied allocations, returned for reuse: constants, guest memory and indices.
pub type Recycled = (Vec<u8>, Vec<u8>, Vec<u32>);

/// The last frame the GPU has finished presenting.
struct Progress {
    finished: Mutex<u64>,
    changed: Condvar,
}

pub struct Submitter {
    work: Sender<Work>,
    recycled: Receiver<Recycled>,
    progress: Arc<Progress>,
}

impl Submitter {
    pub fn start(device: wgpu::Device, queue: wgpu::Queue, frame_wait_limit: Duration) -> Self {
        let (work, incoming) = mpsc::channel();
        let (recycle, recycled) = mpsc::channel();
        let progress = Arc::new(Progress {
            finished: Mutex::new(0),
            changed: Condvar::new(),
        });
        let worker_progress = Arc::clone(&progress);
        thread::Builder::new()
            .name(THREAD_NAME.to_string())
            .spawn(move || {
                let worker = Worker {
                    device,
                    queue,
                    recycle,
                    progress: worker_progress,
                    frame_wait_limit,
                };
                for work in incoming {
                    worker.run(work);
                }
            })
            .expect("cannot start the GPU submit thread");
        Self {
            work,
            recycled,
            progress,
        }
    }

    pub fn send(&self, work: Work) {
        // The worker only stops when the process does.
        let _ = self.work.send(work);
    }

    /// Allocations a submitted batch gave back, if any yet.
    pub fn recycled(&self) -> Option<Recycled> {
        self.recycled.try_recv().ok()
    }

    /// Waits, at most `limit`, until the GPU has finished presenting `frame`, so the title runs
    /// at most one frame ahead of the GPU.
    pub fn wait_for_frame(&self, frame: u64, limit: Duration) {
        let finished = self.progress.finished.lock().unwrap();
        let _ = self
            .progress
            .changed
            .wait_timeout_while(finished, limit, |finished| *finished < frame)
            .unwrap();
    }
}

struct Worker {
    device: wgpu::Device,
    queue: wgpu::Queue,
    recycle: Sender<Recycled>,
    progress: Arc<Progress>,
    frame_wait_limit: Duration,
}

impl Worker {
    /// Runs one piece of work, reporting validation errors instead of letting them reach Bevy,
    /// which would close the app. Error scopes belong to the thread that pushed them.
    fn run(&self, work: Work) {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let what = match work {
            Work::WriteTexture {
                texture,
                data,
                layout,
            } => {
                self.queue
                    .write_texture(texture.as_image_copy(), &data, layout, texture.size());
                "texture upload"
            }
            Work::Submit { encoder, uploads } => {
                let Uploads {
                    constants: (constants_buffer, mut constants),
                    memory: (memory_buffer, mut memory),
                    indices: (indices_buffer, mut indices),
                } = uploads;
                self.queue.write_buffer(&constants_buffer, 0, &constants);
                self.queue.write_buffer(&memory_buffer, 0, &memory);
                self.queue
                    .write_buffer(&indices_buffer, 0, words_as_bytes(&indices));
                self.queue.submit([encoder.finish()]);
                constants.clear();
                memory.clear();
                indices.clear();
                let _ = self.recycle.send((constants, memory, indices));
                "submission"
            }
            Work::Present {
                mut encoder,
                frame,
                display,
                captures,
                readbacks,
            } => {
                let captures: Vec<Readback> = captures
                    .into_iter()
                    .filter_map(|path| Readback::record(&self.device, &mut encoder, &display, path))
                    .collect();
                let submission = self.queue.submit([encoder.finish()]);
                let _ = self.device.poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(self.frame_wait_limit),
                });
                *self.progress.finished.lock().unwrap() = frame;
                self.progress.changed.notify_all();
                for readback in captures.into_iter().chain(readbacks) {
                    readback.save(&self.device, self.frame_wait_limit);
                }
                "presentation"
            }
        };
        if let Some(error) = bevy::tasks::block_on(scope.pop()) {
            warn!(target: "renderer", "{what} failed: {error}");
        }
    }
}
