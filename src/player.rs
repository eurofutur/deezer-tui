use std::io::{self, BufReader, Read, Seek, SeekFrom};

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use rodio::{Decoder, OutputStream, Sink};

pub struct StreamingPlayer {
    _stream: OutputStream,
    sink: Sink,
    /// Threads spawned by the audio backend for this stream (cpal_alsa_out).
    output_tids: Vec<String>,
}

impl StreamingPlayer {
    pub fn new() -> Result<Self> {
        let before = thread_ids();
        let (stream, stream_handle) =
            OutputStream::try_default().context("failed to open audio output stream")?;
        let sink = Sink::try_new(&stream_handle).context("failed to create audio sink")?;
        let output_tids = thread_ids().into_iter().filter(|t| !before.contains(t)).collect();

        Ok(Self {
            _stream: stream,
            sink,
            output_tids,
        })
    }

    /// CPU time (clock ticks) used so far by the audio output thread(s).
    ///
    /// cpal 0.15 never gives up on a persistent POLLERR (e.g. the Bluetooth sink
    /// went into error): its worker loops on poll() at 100% CPU and plays nothing.
    /// The caller samples this to notice that and rebuild the stream.
    pub fn output_cpu_ticks(&self) -> u64 {
        self.output_tids.iter().map(|tid| thread_cpu_ticks(tid)).sum()
    }

    pub fn play_stream(&self, receiver: Receiver<Vec<u8>>, size_hint: Option<usize>) -> Result<()> {
        let stream_buffer = StreamBuffer::new(receiver, size_hint);
        let decoder = Decoder::new(BufReader::new(stream_buffer))
            .context("failed to create streaming audio decoder")?;

        self.sink.append(decoder);
        self.sink.play();
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.sink.empty()
    }

    pub fn pause(&self) {
        self.sink.pause();
    }

    pub fn resume(&self) {
        self.sink.play();
    }

    pub fn stop(&self) {
        self.sink.stop();
    }

    pub fn set_volume(&self, volume: f32) {
        self.sink.set_volume(volume.clamp(0.0, 1.0));
    }
}

pub struct StreamBuffer {
    receiver: Receiver<Vec<u8>>,
    /// All bytes received so far (grows indefinitely but lets us seek backward).
    buffer: Vec<u8>,
    /// Current logical read position within `buffer`.
    position: usize,
    closed: bool,
}

impl StreamBuffer {
    /// `size_hint` is the track length: reserving it once avoids the doubling
    /// reallocations (a 35 MB FLAC used to end up in a 64 MB buffer).
    pub fn new(receiver: Receiver<Vec<u8>>, size_hint: Option<usize>) -> Self {
        Self {
            receiver,
            buffer: Vec::with_capacity(size_hint.unwrap_or(0).min(256 << 20)),
            position: 0,
            closed: false,
        }
    }

    /// Pull more chunks from the receiver until `buffer` has at least `need` bytes,
    /// or the sender is closed.
    fn fill_to(&mut self, need: usize) -> io::Result<()> {
        while self.buffer.len() < need && !self.closed {
            match self.receiver.recv() {
                Ok(chunk) if !chunk.is_empty() => self.buffer.extend_from_slice(&chunk),
                Ok(_) => continue,
                Err(_) => self.closed = true,
            }
        }
        Ok(())
    }
}

impl Read for StreamBuffer {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // Ensure at least one byte beyond current position is available.
        let want = self.position + out.len();
        self.fill_to(want)?;

        let available = self.buffer.len().saturating_sub(self.position);
        if available == 0 {
            return Ok(0);
        }
        let n = out.len().min(available);
        out[..n].copy_from_slice(&self.buffer[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

impl Seek for StreamBuffer {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Start(n) => {
                let n = n as usize;
                self.fill_to(n)?;
                self.position = n.min(self.buffer.len());
                Ok(self.position as u64)
            }
            SeekFrom::Current(delta) => {
                let new_pos = (self.position as i64 + delta).max(0) as usize;
                self.fill_to(new_pos)?;
                self.position = new_pos.min(self.buffer.len());
                Ok(self.position as u64)
            }
            SeekFrom::End(delta) => {
                // We don't know the end until the stream is fully received.
                // Drain the receiver so we know the true length.
                while !self.closed {
                    match self.receiver.recv() {
                        Ok(chunk) if !chunk.is_empty() => self.buffer.extend_from_slice(&chunk),
                        Ok(_) => continue,
                        Err(_) => self.closed = true,
                    }
                }
                let new_pos = (self.buffer.len() as i64 + delta).max(0) as usize;
                self.position = new_pos.min(self.buffer.len());
                Ok(self.position as u64)
            }
        }
    }
}

fn thread_ids() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .map(|dir| dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

/// utime + stime of one thread of this process, 0 once it is gone.
fn thread_cpu_ticks(tid: &str) -> u64 {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")) else {
        return 0;
    };
    // Fields after "(comm) ": state is index 0, utime 11, stime 12.
    let fields: Vec<&str> = stat.rsplit_once(") ").map(|(_, rest)| rest).unwrap_or("").split(' ').collect();
    let field = |i: usize| fields.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    field(11) + field(12)
}

/// Give freed heap back to the kernel (glibc keeps per-thread arenas otherwise).
pub fn release_free_memory() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Big buffers (track data, cover PNGs) always go through mmap so that freeing
/// them returns the memory, instead of glibc's adaptive threshold that climbs to
/// 32 MB and leaves them fragmenting the heap for the whole session.
pub fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 1 << 20);
    }
}
