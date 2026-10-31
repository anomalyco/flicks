//! A segment decision, not a frame decision. The clock and the network live outside this rule.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Tier {
    P480,
    P720,
    P1080,
    Direct,
}

#[derive(Clone, Copy, Debug)]
pub struct AdaptiveStats {
    /// The rendition of the last displayed frame (the startup tier before first display).
    pub tier: Tier,
    /// EWMA of completed video segment body transfers, in bits per second.
    pub throughput_bps: u64,
    /// Media ready ahead of the playback clock, including the currently decoding segment.
    pub buffer_seconds: f64,
    /// Retained compressed segments and their sample indexes, not decoded surfaces or RSS.
    pub buffer_bytes: usize,
}

#[derive(Default)]
pub(crate) struct Abr {
    pub bps: u64,
    current: usize,
    upgrade: Option<usize>,
}

impl Abr {
    pub fn new(bps: u64) -> Self {
        Self { bps, ..Default::default() }
    }
    pub fn observe(&mut self, bytes: usize, elapsed: std::time::Duration) {
        // Small instantaneous reads from a socket buffer are not bandwidth measurements.
        if bytes < 16 * 1024 {
            return;
        }
        let bps = (8.0 * bytes as f64 / elapsed.as_secs_f64().max(0.001)) as u64;
        self.bps = if self.bps == 0 { bps } else { self.bps.midpoint(bps) };
    }

    /// Rates include audio and are ordered from the smallest encode to the original.
    pub fn choose(&mut self, rates: impl DoubleEndedIterator<Item = u64> + ExactSizeIterator, ahead: f64) -> usize {
        let budget = self.bps.saturating_mul(7) / 10;
        let best = rates.enumerate().rfind(|(_, rate)| *rate <= budget).map_or(0, |(i, _)| i);
        let target = if ahead < 6.0 { best.min(self.current.saturating_sub(1)) } else { best };
        if target <= self.current || (ahead >= 10.0 && self.upgrade == Some(target)) {
            self.upgrade = None;
            self.current = target;
        } else {
            self.upgrade = (ahead >= 10.0).then_some(target);
        }
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sustainable_tiers_wait_on_upgrade_not_downgrade() {
        let mut abr = Abr::default();
        let rates = [1_392_000, 3_192_000, 8_192_000, 11_000_000];
        let decide = |abr: &mut Abr, ahead| abr.choose(rates.into_iter(), ahead);
        assert_eq!(decide(&mut abr, 0.0), 0);
        abr.observe(2_500_000, std::time::Duration::from_secs(1));
        assert_eq!(abr.bps, 20_000_000);
        assert_eq!(decide(&mut abr, 6.0), 0);
        assert_eq!(decide(&mut abr, 15.0), 0);
        assert_eq!(decide(&mut abr, 20.0), 3);
        abr.observe(375_000, std::time::Duration::from_secs(1));
        assert_eq!(abr.bps, 11_500_000);
        assert_eq!(decide(&mut abr, 24.0), 1);
        assert_eq!(decide(&mut abr, 4.0), 0);
        assert_eq!(decide(&mut abr, 16.0), 0);
        assert_eq!(decide(&mut abr, 5.0), 0);
    }
}
