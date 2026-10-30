//! Maps a source layout onto a device's channels: pass-through in canonical order, or the
//! ITU-R BS.775 stereo downmix.

use crate::Channel::{self, *};

const G: f32 = std::f32::consts::FRAC_1_SQRT_2; // -3 dB

/// Mixing matrix, `out_channels` rows of `src.len()` gains.
///
/// - A source with at least two channels on a device with at least as many: each source channel
///   goes to its rank in the canonical order ([`Channel`] declaration order), so 5.1 always
///   leaves as L R C LFE Ls Rs.
/// - Otherwise the BS.775 Lo/Ro downmix into the first two device channels: Lo = L + 0.707 C +
///   0.707 Ls (+ 0.707 Lb), Ro likewise; back centre at 0.5 to both; LFE dropped. A mono source
///   plays as phantom centre (0.707 to both). Gains are not normalised, as BS.775 specifies; a
///   loud 5.1 mix can exceed full scale.
/// - A mono device gets (Lo + Ro) / 2.
pub fn matrix(src: &[Channel], out_channels: usize) -> Vec<f32> {
    let n = src.len();
    let mut m = vec![0f32; out_channels * n];
    if n >= 2 && out_channels >= n {
        for (i, ch) in src.iter().enumerate() {
            let rank = src.iter().filter(|o| *o < ch).count();
            m[rank * n + i] = 1.0;
        }
        return m;
    }
    for (i, ch) in src.iter().enumerate() {
        let (lo, ro) = match ch {
            L => (1.0, 0.0),
            R => (0.0, 1.0),
            C => (G, G),
            Lfe => (0.0, 0.0),
            Ls | Lb => (G, 0.0),
            Rs | Rb => (0.0, G),
            Cb => (0.5, 0.5),
        };
        match out_channels {
            0 => {}
            1 => m[i] = (lo + ro) / 2.0,
            _ => {
                m[i] = lo;
                m[n + i] = ro;
            }
        }
    }
    m
}

/// Applies `matrix` (from [`matrix`]) to interleaved `input` frames of `src_channels`, appending
/// interleaved frames of `out_channels` to `out`.
pub fn apply(matrix: &[f32], src_channels: usize, out_channels: usize, input: &[f32], out: &mut Vec<f32>) {
    if src_channels == 0 || matrix.len() != src_channels * out_channels {
        return;
    }
    out.reserve(input.len() / src_channels * out_channels);
    for frame in input.chunks_exact(src_channels) {
        for row in matrix.chunks_exact(src_channels) {
            out.push(row.iter().zip(frame).map(|(g, x)| g * x).sum());
        }
    }
}
