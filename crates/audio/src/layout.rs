//! Speaker positions and each decoder's output channel order.

/// A speaker position. The declaration order is the canonical channel order (WAV / SMPTE for
/// 5.1: L R C LFE Ls Rs, then the 7.1 rear pair, the back centre and the front centre pair).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channel {
    L,
    R,
    C,
    Lfe,
    /// Left surround (5.1 surround, or a quad stream's rear left).
    Ls,
    Rs,
    /// Left rear surround (the extra pair of 7.1).
    Lb,
    Rb,
    /// Back centre (6.1, AC-3 2/1 and 3/1).
    Cb,
    /// Left centre, between L and C (AAC front-wide 7.1, where L and R are the wide pair).
    Lc,
    Rc,
}

use Channel::*;

/// `flicks-aac` output order (AAC element order) for an `AudioSpecificConfig` channel
/// configuration. Configuration 0 (layout from a PCE) is taken from the channel count. `None` for
/// anything not in the table.
pub fn aac(channel_config: u8, channels: usize) -> Option<&'static [Channel]> {
    let by_count: &[&[Channel]] = &[
        &[C],
        &[L, R],
        &[C, L, R],
        &[C, L, R, Cb],
        &[C, L, R, Ls, Rs],
        &[C, L, R, Ls, Rs, Lfe],
        &[C, L, R, Ls, Rs, Cb, Lfe],
        &[C, L, R, Ls, Rs, Lb, Rb, Lfe],
    ];
    let layout: &'static [Channel] = match channel_config {
        0 => by_count.get(channels.checked_sub(1)?)?,
        1..=6 => by_count.get(channel_config as usize - 1)?,
        // Front wide 7.1 (ISO/IEC 14496-3 Table 1.19): C, Lc Rc, then the outside L R pair.
        7 => &[C, Lc, Rc, L, R, Ls, Rs, Lfe],
        11 => by_count.get(6)?,
        12 => by_count.get(7)?,
        _ => return None,
    };
    (layout.len() == channels).then_some(layout)
}

/// `flicks-ac3` output order (already WAV order: fronts, LFE, surrounds) for an AC-3 / E-AC-3
/// `acmod` and `lfeon`. Dual mono (1+1) plays as L R.
pub fn ac3(acmod: u8, lfeon: bool) -> Option<&'static [Channel]> {
    Some(match (acmod, lfeon) {
        (0 | 2, false) => &[L, R],
        (0 | 2, true) => &[L, R, Lfe],
        (1, false) => &[C],
        (1, true) => &[C, Lfe],
        (3, false) => &[L, R, C],
        (3, true) => &[L, R, C, Lfe],
        (4, false) => &[L, R, Cb],
        (4, true) => &[L, R, Lfe, Cb],
        (5, false) => &[L, R, C, Cb],
        (5, true) => &[L, R, C, Lfe, Cb],
        (6, false) => &[L, R, Ls, Rs],
        (6, true) => &[L, R, Lfe, Ls, Rs],
        (7, false) => &[L, R, C, Ls, Rs],
        (7, true) => &[L, R, C, Lfe, Ls, Rs],
        _ => return None,
    })
}

/// `flicks-opus` output order (Vorbis order, RFC 7845 §5.1.1) for an `OpusHead` mapping family
/// and channel count. `None` for families other than 0 and 1.
pub fn opus(mapping_family: u8, channels: usize) -> Option<&'static [Channel]> {
    Some(match (mapping_family, channels) {
        (0 | 1, 1) => &[C],
        (0 | 1, 2) => &[L, R],
        (1, 3) => &[L, C, R],
        (1, 4) => &[L, R, Ls, Rs],
        (1, 5) => &[L, C, R, Ls, Rs],
        (1, 6) => &[L, C, R, Ls, Rs, Lfe],
        (1, 7) => &[L, C, R, Ls, Rs, Cb, Lfe],
        (1, 8) => &[L, C, R, Ls, Rs, Lb, Rb, Lfe],
        _ => return None,
    })
}
