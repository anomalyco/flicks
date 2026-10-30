//! The default output device (CoreAudio through cpal), fed from a queue of device-format frames.
//! Elsewhere [`Output::open`] fails with [`Error::NoDevice`], so callers build on every platform.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Channel, Error, Resampler, mix};

/// Plays timestamped audio on the default output device and reports which timestamp is being
/// heard, so the player can use audio as its master clock.
pub struct Output {
    stream: Stream,
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

#[derive(Default)]
struct Queue {
    /// Interleaved device-format samples not yet handed to the device.
    samples: VecDeque<f32>,
    /// (device frame index, source time in seconds) where each push starts.
    segments: VecDeque<(f64, f64)>,
    /// Frames handed to the device so far.
    played: u64,
    /// From the last callback that handed over frames: the frame index heard at that stream time
    /// (seconds).
    clock: Option<(u64, f64)>,
    /// The latest device frame [`Queue::position`] reported. After an underrun the refilled
    /// frames are heard only a latency after their callback, and the clock must not step back.
    heard: f64,
    /// From the last callback: callback to playback.
    latency: Duration,
}

impl Queue {
    /// The device callback: hands over queued frames and fills the rest of `data` with silence.
    /// `playback` is the stream time (seconds) at which `data`'s first frame is heard.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn fill(&mut self, data: &mut [f32], channels: usize, playback: f64, latency: Duration) {
        let n = data.len().min(self.samples.len()) / channels * channels;
        let (head, tail) = data.split_at_mut(n);
        for (d, s) in head.iter_mut().zip(self.samples.drain(..n)) {
            *d = s;
        }
        tail.fill(0.0);
        self.latency = latency;
        // Silence moves no source time: the clock keeps extrapolating from the last frames
        // played and stops at the last of them.
        if n > 0 {
            self.clock = Some((self.played, playback));
            self.played += (n / channels) as u64;
        }
    }

    /// Appends converted frames that start at device frame `start` and source time `pts`.
    fn append(&mut self, start: f64, pts: f64, samples: impl IntoIterator<Item = f32>, rate: u32) {
        self.samples.extend(samples);
        self.segments.push_back((start, pts));
        // Keep a second of history behind the device position.
        let behind = self.played as f64 - rate as f64;
        while self.segments.get(1).is_some_and(|s| s.0 < behind) {
            self.segments.pop_front();
        }
    }

    /// Source time heard at stream time `now` (seconds); never earlier than the last report.
    fn position(&mut self, now: f64, rate: u32) -> Option<f64> {
        let (frame, at) = self.clock?;
        // Between callbacks the device clock advances at the device rate; it stops at the last
        // frame handed over (an underrun or the end).
        let speaker = (frame as f64 + (now - at) * rate as f64).min(self.played as f64).max(self.heard);
        self.heard = speaker;
        let (start, pts) = self.segments.iter().rev().find(|s| s.0 <= speaker)?;
        Some(pts + (speaker - start) / rate as f64)
    }

    /// Duration of the frames not yet handed to the device.
    fn queued(&self, channels: usize, rate: u32) -> Duration {
        let frames = (self.samples.len() / channels) as u64;
        Duration::from_nanos(frames.saturating_mul(1_000_000_000) / u64::from(rate))
    }

    /// Forgets all queued audio and the clock: frame 0 is the next one pushed.
    fn clear(&mut self) {
        self.samples.clear();
        self.segments.clear();
        self.played = 0;
        self.clock = None;
        self.heard = 0.0;
    }
}

impl Output {
    /// Opens the default output device at its own rate and channel count, and starts it
    /// (playing silence until frames are pushed).
    pub fn open() -> Result<Output, Error> {
        let shared = Arc::new(Mutex::new(Queue::default()));
        let (stream, rate, channels) = Stream::open(shared.clone())?;
        // A second of room, so pushes of a player that keeps well under that never reallocate
        // while holding the callback's lock.
        if let Ok(mut q) = shared.lock() {
            q.samples.reserve(rate as usize * channels);
        }
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
        let pad = self.channels - out_ch;
        let frames = data.chunks_exact(out_ch).flat_map(|f| f.iter().copied().chain(std::iter::repeat_n(0.0, pad)));
        let mut q = self.shared.lock().map_err(|_| Error::Format("output queue poisoned"))?;
        q.append(start, pts, frames, self.rate);
        Ok(())
    }

    /// Source time (seconds) of the sound leaving the device now; `None` before the first pushed
    /// frame has started playing (and after [`Output::clear`] until the next one has). It never
    /// goes backwards between clears, and stops at the last frame pushed.
    pub fn position(&self) -> Option<f64> {
        let now = self.stream.now();
        self.shared.lock().ok()?.position(now, self.rate)
    }

    /// Duration of the pushed audio not yet handed to the device. Keep it short (around 0.3 s):
    /// it is how far ahead of the clock a seek or a format change takes effect.
    pub fn queued(&self) -> Duration {
        self.shared.lock().map(|q| q.queued(self.channels, self.rate)).unwrap_or_default()
    }

    /// Time from a frame being handed to the device to it being heard (buffer, device and safety
    /// offset latency, as CoreAudio reports it).
    pub fn latency(&self) -> Duration {
        self.shared.lock().map(|q| q.latency).unwrap_or_default()
    }

    /// Stops the device; queued audio and the clock hold until [`Output::play`].
    pub fn pause(&self) -> Result<(), Error> {
        self.stream.pause()
    }

    /// Restarts the device after [`Output::pause`] (it starts playing on open).
    pub fn play(&self) -> Result<(), Error> {
        self.stream.play()
    }

    /// Drops everything queued, the clock and the resampler state (for a seek); the next push
    /// starts afresh.
    pub fn clear(&mut self) {
        if let Ok(mut q) = self.shared.lock() {
            q.clear();
        }
        self.source = None;
    }

    /// Device frame index just after the last queued frame.
    fn queued_end(&self) -> f64 {
        self.shared.lock().map(|q| q.played as f64 + (q.samples.len() / self.channels) as f64).unwrap_or(0.0)
    }
}

#[cfg(target_os = "macos")]
struct Stream(cpal::Stream);

#[cfg(target_os = "macos")]
impl Stream {
    /// Builds and starts the default device's stream at its own rate and channel count.
    fn open(queue: Arc<Mutex<Queue>>) -> Result<(Stream, u32, usize), Error> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let device = cpal::default_host().default_output_device().ok_or(Error::NoDevice)?;
        let supported = device.default_output_config().map_err(Error::device)?;
        let rate = supported.sample_rate();
        let channels = usize::from(supported.channels());
        if channels == 0 || rate == 0 {
            return Err(Error::Format("device reports no channels or rate"));
        }
        let config = cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let callback = move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
            let Ok(mut q) = queue.lock() else {
                data.fill(0.0);
                return;
            };
            let ts = info.timestamp();
            q.fill(data, channels, seconds(ts.playback), ts.playback.saturating_duration_since(ts.callback));
        };
        // CoreAudio converts f32 to whatever the hardware takes.
        let stream = device.build_output_stream::<f32, _, _>(config, callback, |_| {}, None).map_err(Error::device)?;
        stream.play().map_err(Error::device)?;
        Ok((Stream(stream), rate, channels))
    }

    fn now(&self) -> f64 {
        use cpal::traits::StreamTrait;
        seconds(self.0.now())
    }

    fn pause(&self) -> Result<(), Error> {
        use cpal::traits::StreamTrait;
        self.0.pause().map_err(Error::device)
    }

    fn play(&self) -> Result<(), Error> {
        use cpal::traits::StreamTrait;
        self.0.play().map_err(Error::device)
    }
}

#[cfg(target_os = "macos")]
fn seconds(t: cpal::StreamInstant) -> f64 {
    t.as_nanos() as f64 / 1e9
}

/// No output backend on this platform yet: never constructed.
#[cfg(not(target_os = "macos"))]
enum Stream {}

#[cfg(not(target_os = "macos"))]
impl Stream {
    fn open(_queue: Arc<Mutex<Queue>>) -> Result<(Stream, u32, usize), Error> {
        Err(Error::NoDevice)
    }

    fn now(&self) -> f64 {
        match *self {}
    }

    fn pause(&self) -> Result<(), Error> {
        match *self {}
    }

    fn play(&self) -> Result<(), Error> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 1000;
    const BUFFER: usize = 10;
    const LATENCY: f64 = 0.005;

    /// Queues `frames` mono frames starting at source time `pts`.
    fn push(q: &mut Queue, pts: f64, frames: usize) {
        let start = q.played as f64 + q.samples.len() as f64;
        q.append(start, pts, std::iter::repeat_n(0.5, frames), RATE);
    }

    /// Runs the device callback for one buffer at stream time `t`.
    fn callback(q: &mut Queue, t: f64) {
        let mut data = [1.0; BUFFER];
        q.fill(&mut data, 1, t + LATENCY, Duration::from_secs_f64(LATENCY));
    }

    #[test]
    fn position_is_monotonic_through_underrun_and_end() {
        let mut q = Queue::default();
        assert_eq!(q.position(0.0, RATE), None);
        push(&mut q, 5.0, 25);
        let (mut last, mut t) = (f64::NEG_INFINITY, 0.0);
        // 25 frames: two full buffers, a partial one, then underrun callbacks with nothing queued.
        for step in 0..60 {
            if step % 10 == 0 {
                callback(&mut q, t);
            }
            if let Some(p) = q.position(t, RATE) {
                assert!(p >= last, "position went back from {last} to {p} at {t}");
                last = p;
            }
            t += 0.001;
        }
        // The clock reached the end of what was pushed and stayed there.
        assert!((last - 5.025).abs() < 1e-9, "end position {last}");
        // Refilled after the underrun: the clock holds until the new frames are heard, then
        // runs again from where they start.
        push(&mut q, 5.025, 20);
        for step in 0..30 {
            if step % 10 == 0 {
                callback(&mut q, t);
            }
            let p = q.position(t, RATE).unwrap();
            assert!(p >= last, "position went back from {last} to {p} at {t}");
            last = p;
            t += 0.001;
        }
        assert!((last - 5.045).abs() < 1e-9, "end position {last}");
    }

    #[test]
    fn clear_resets_the_clock() {
        let mut q = Queue::default();
        push(&mut q, 100.0, 40);
        callback(&mut q, 0.0);
        callback(&mut q, 0.01);
        assert!(q.position(0.02, RATE).unwrap() > 100.0);
        q.clear();
        assert_eq!(q.position(0.02, RATE), None);
        assert_eq!(q.queued(1, RATE), Duration::ZERO);
        // After a seek back to 3 s the clock reports the new time, not the old one.
        push(&mut q, 3.0, 40);
        callback(&mut q, 0.03);
        let p = q.position(0.03 + LATENCY + 0.004, RATE).unwrap();
        assert!((p - 3.004).abs() < 1e-9, "position after clear {p}");
    }

    #[test]
    fn queued_counts_frames_not_yet_handed_over() {
        let mut q = Queue::default();
        push(&mut q, 0.0, 300);
        assert_eq!(q.queued(1, RATE), Duration::from_millis(300));
        callback(&mut q, 0.0);
        assert_eq!(q.queued(1, RATE), Duration::from_millis(290));
        // Two channels: 300 samples are 150 frames.
        assert_eq!(
            Queue { samples: vec![0.0; 300].into(), ..Queue::default() }.queued(2, RATE),
            Duration::from_millis(150)
        );
    }
}
