//! The default output device (CoreAudio through cpal), fed from a queue of device-format frames.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{StreamConfig, StreamInstant};

use crate::{Channel, Error, Resampler, mix};

/// Plays timestamped audio on the default output device and reports which timestamp is being
/// heard, so the player can use audio as its master clock.
pub struct Output {
    stream: cpal::Stream,
    shared: Arc<Mutex<Queue>>,
    rate: u32,
    channels: usize,
    source: Option<Source>,
    scratch: Vec<f32>,
    resampled: Vec<f32>,
}

/// The current source format and its conversion to the device.
struct Source {
    rate: u32,
    layout: Vec<Channel>,
    matrix: Vec<f32>,
    resampler: Option<Resampler>,
    /// Device frame index of the source's first frame, and the source frames pushed since.
    base: f64,
    frames: u64,
}

struct Queue {
    /// Interleaved device-format samples not yet handed to the device.
    samples: VecDeque<f32>,
    /// (device frame index, source time in seconds) where each push starts.
    segments: VecDeque<(f64, f64)>,
    /// Frames handed to the device so far.
    played: u64,
    /// From the last callback: the frame index heard at that instant.
    clock: Option<(u64, StreamInstant)>,
    /// From the last callback: callback to playback.
    latency: Duration,
}

impl Output {
    /// Opens the default output device at its own rate and channel count, and starts it
    /// (playing silence until frames are pushed).
    pub fn open() -> Result<Output, Error> {
        let device = cpal::default_host().default_output_device().ok_or(Error::NoDevice)?;
        let supported = device.default_output_config().map_err(Error::device)?;
        let rate = supported.sample_rate();
        let channels = usize::from(supported.channels());
        if channels == 0 || rate == 0 {
            return Err(Error::Format("device reports no channels or rate"));
        }
        let shared = Arc::new(Mutex::new(Queue {
            samples: VecDeque::new(),
            segments: VecDeque::new(),
            played: 0,
            clock: None,
            latency: Duration::ZERO,
        }));
        let config =
            StreamConfig { channels: supported.channels(), sample_rate: rate, buffer_size: cpal::BufferSize::Default };
        let queue = shared.clone();
        let callback = move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let Ok(mut q) = queue.lock() else {
                data.fill(0.0);
                return;
            };
            let n = data.len().min(q.samples.len()) / channels * channels;
            for (d, s) in data.iter_mut().zip(q.samples.drain(..n)) {
                *d = s;
            }
            data[n..].fill(0.0);
            let ts = info.timestamp();
            q.clock = Some((q.played, ts.playback));
            q.latency = ts.playback.saturating_duration_since(ts.callback);
            q.played += (n / channels) as u64;
        };
        // CoreAudio converts f32 to whatever the hardware takes.
        let stream = device.build_output_stream::<f32, _, _>(config, callback, |_| {}, None).map_err(Error::device)?;
        stream.play().map_err(Error::device)?;
        Ok(Output { stream, shared, rate, channels, source: None, scratch: Vec::new(), resampled: Vec::new() })
    }

    /// Device sample rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Device channel count.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Queues interleaved frames in `layout` at `rate` whose first frame has source time `pts`
    /// (seconds). They are remixed to the device's channels and resampled to its rate. A change
    /// of rate or layout starts a new conversion (the old resampler's last ~0.7 ms are dropped).
    pub fn push(&mut self, pts: f64, rate: u32, layout: &[Channel], samples: &[f32]) -> Result<(), Error> {
        let n = layout.len();
        if n == 0 || n > 8 || !samples.len().is_multiple_of(n) || !pts.is_finite() {
            return Err(Error::Format("push: bad layout or sample count"));
        }
        if self.source.as_ref().is_none_or(|s| s.rate != rate || s.layout != layout) {
            let resampler =
                if rate == self.rate { None } else { Some(Resampler::new(rate, self.rate, self.channels.min(8))?) };
            self.source = Some(Source {
                rate,
                layout: layout.to_vec(),
                matrix: mix::matrix(layout, self.channels.min(8)),
                resampler,
                base: self.queued_end(),
                frames: 0,
            });
        }
        let Some(src) = self.source.as_mut() else { return Ok(()) };
        let out_ch = self.channels.min(8);
        self.scratch.clear();
        mix::apply(&src.matrix, n, out_ch, samples, &mut self.scratch);
        let ratio = self.rate as f64 / rate as f64;
        let start = src.base + src.frames as f64 * ratio;
        src.frames += (samples.len() / n) as u64;
        let data = match src.resampler.as_mut() {
            Some(r) => {
                self.resampled.clear();
                r.process(&self.scratch, &mut self.resampled);
                &self.resampled
            }
            None => &self.scratch,
        };
        let mut q = self.shared.lock().map_err(|_| Error::Format("output queue poisoned"))?;
        for frame in data.chunks_exact(out_ch) {
            q.samples.extend(frame);
            q.samples.extend(std::iter::repeat_n(0.0, self.channels - out_ch));
        }
        q.segments.push_back((start, pts));
        // Keep a second of history behind the device position.
        let behind = q.played as f64 - self.rate as f64;
        while q.segments.get(1).is_some_and(|s| s.0 < behind) {
            q.segments.pop_front();
        }
        Ok(())
    }

    /// Source time (seconds) of the sound leaving the device now; `None` before the first pushed
    /// frame has started playing.
    pub fn position(&self) -> Option<f64> {
        let q = self.shared.lock().ok()?;
        let (frame, at) = q.clock?;
        let now = self.stream.now();
        let dt = match now.checked_duration_since(at) {
            Some(d) => d.as_secs_f64(),
            None => -at.saturating_duration_since(now).as_secs_f64(),
        };
        // Between callbacks the device clock advances at the device rate; it stops at the last
        // frame handed over (an underrun).
        let speaker = (frame as f64 + dt * self.rate as f64).clamp(0.0, q.played as f64);
        let (start, pts) = q.segments.iter().rev().find(|s| s.0 <= speaker)?;
        Some(pts + (speaker - start) / self.rate as f64)
    }

    /// Time from a frame being handed to the device to it being heard (buffer, device and safety
    /// offset latency, as CoreAudio reports it).
    pub fn latency(&self) -> Duration {
        self.shared.lock().map(|q| q.latency).unwrap_or_default()
    }

    /// Drops everything queued and the resampler state (for a seek); the next push starts afresh.
    pub fn clear(&mut self) {
        if let Ok(mut q) = self.shared.lock() {
            q.samples.clear();
            q.segments.clear();
        }
        self.source = None;
    }

    /// Device frame index just after the last queued frame.
    fn queued_end(&self) -> f64 {
        self.shared.lock().map(|q| q.played as f64 + (q.samples.len() / self.channels) as f64).unwrap_or(0.0)
    }
}
