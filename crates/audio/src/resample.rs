//! Rational polyphase resampler with a Kaiser-windowed sinc filter.
//!
//! The rate ratio is reduced to `up / down`; output frame `n` is the band-limited input at input
//! time `n * down / up`, so output and input stay aligned (no group delay), and the filter has one
//! phase per distinct fractional position. The cutoff is 0.91 of the lower Nyquist frequency and
//! the filter spans 64 zero crossings of the lower rate (Kaiser β = 9), see `docs/perf.md` for the
//! measured quality and cost.

use crate::Error;

/// Half the filter length, in samples of the lower of the two rates.
const HALF: f64 = 32.0;
/// Cutoff as a fraction of the lower Nyquist frequency.
const CUTOFF: f64 = 0.91;
const BETA: f64 = 9.0;
/// The most filter phases (output rate / gcd) accepted. Every pair of common rates from 8 to
/// 192 kHz needs ≤ 2560 (11.025 → 192 kHz; 2560 × 64 taps is 640 KB of filter).
const MAX_PHASES: usize = 2560;
const MAX_CHANNELS: usize = 8;

pub struct Resampler {
    channels: usize,
    up: usize,
    down: usize,
    taps: usize,
    /// `up` phases of `taps` coefficients.
    filter: Vec<f32>,
    /// Interleaved input frames not yet consumed; the window of the next output starts at
    /// frame `pos`.
    history: Vec<f32>,
    pos: usize,
    phase: usize,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Result<Resampler, Error> {
        if in_rate == 0 || out_rate == 0 || channels == 0 || channels > MAX_CHANNELS {
            return Err(Error::Format("resampler rate or channel count"));
        }
        let g = gcd(in_rate, out_rate);
        let (up, down) = ((out_rate / g) as usize, (in_rate / g) as usize);
        if up > MAX_PHASES {
            return Err(Error::Format("resampling ratio needs too many filter phases"));
        }
        // fc is the cutoff relative to the input Nyquist frequency; the filter is stretched by
        // 1/fc in input samples when downsampling.
        let ratio = (up as f64 / down as f64).min(1.0);
        let fc = CUTOFF * ratio;
        let half = (HALF / ratio).ceil();
        let taps = 2 * half as usize;
        let mut filter = Vec::with_capacity(up * taps);
        for p in 0..up {
            let start = filter.len();
            for k in 0..taps {
                // Distance from the output position (input frame `half - 1` plus p / up) to tap k.
                let u = p as f64 / up as f64 + (half - 1.0) - k as f64;
                filter.push(fc * sinc(fc * u) * kaiser(u / half));
            }
            // Unit DC gain for every phase.
            let sum: f64 = filter[start..].iter().sum();
            filter[start..].iter_mut().for_each(|h| *h /= sum);
        }
        let mut r = Resampler {
            channels,
            up,
            down,
            taps,
            filter: filter.into_iter().map(|h| h as f32).collect(),
            history: Vec::new(),
            pos: 0,
            phase: 0,
        };
        r.reset();
        Ok(r)
    }

    /// Forgets all input (after a seek): the next input frame is time zero again.
    pub fn reset(&mut self) {
        // `half - 1` frames of silence before the first input frame centre the first window on it.
        self.history.clear();
        self.history.resize((self.taps / 2 - 1) * self.channels, 0.0);
        self.pos = 0;
        self.phase = 0;
    }

    /// Output frames per input frame.
    pub fn ratio(&self) -> f64 {
        self.up as f64 / self.down as f64
    }

    /// Resamples interleaved `input` frames, appending the output frames it completes to `out`.
    /// Output lags input by the filter's look-ahead (32 frames of the lower rate).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        self.history.extend_from_slice(&input[..input.len() - input.len() % ch]);
        let frames = self.history.len() / ch;
        let mut acc = [0f32; MAX_CHANNELS];
        while self.pos + self.taps <= frames {
            let h = &self.filter[self.phase * self.taps..][..self.taps];
            let window = &self.history[self.pos * ch..][..self.taps * ch];
            let acc = &mut acc[..ch];
            acc.fill(0.0);
            for (frame, &g) in window.chunks_exact(ch).zip(h) {
                for (a, x) in acc.iter_mut().zip(frame) {
                    *a += g * x;
                }
            }
            out.extend_from_slice(acc);
            self.phase += self.down;
            self.pos += self.phase / self.up;
            self.phase %= self.up;
        }
        let consumed = self.pos.min(frames);
        self.history.drain(..consumed * ch);
        self.pos -= consumed;
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Kaiser window at `x` in [-1, 1].
fn kaiser(x: f64) -> f64 {
    if x.abs() > 1.0 {
        return 0.0;
    }
    bessel_i0(BETA * (1.0 - x * x).sqrt()) / bessel_i0(BETA)
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..50 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}
