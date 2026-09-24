use std::time::Duration;

const DEGRADED_RTT_RATIO: f64 = 3.0;
const DEGRADED_PROBE_STREAK: u32 = 5;
const MIN_RECYCLE_INTERVAL: Duration = Duration::from_secs(600);
#[derive(Debug)]
pub(crate) struct RecyclePacer {
    last: std::time::Instant,
}
impl RecyclePacer {
    pub(crate) fn new(now: std::time::Instant) -> Self {
        Self { last: now }
    }
    pub(crate) fn allow(&mut self, now: std::time::Instant) -> bool {
        if now.duration_since(self.last) < MIN_RECYCLE_INTERVAL {
            return false;
        }
        self.last = now;
        true
    }
}
#[derive(Debug, Default)]
pub(crate) struct RttDegradation {
    best: Option<Duration>,
    streak: u32,
}
impl RttDegradation {
    pub(crate) fn observe(&mut self, srtt: Duration) -> bool {
        let best = match self.best {
            Some(best) if best <= srtt => best,
            _ => {
                self.best = Some(srtt);
                srtt
            }
        };
        if srtt.as_secs_f64() < best.as_secs_f64() * DEGRADED_RTT_RATIO {
            self.streak = 0;
            return false;
        }
        self.streak += 1;
        if self.streak < DEGRADED_PROBE_STREAK {
            return false;
        }
        self.streak = 0;
        self.best = Some(srtt);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn pacer_suppresses_early_and_frequent_recycles() {
        let t0 = std::time::Instant::now();
        let mut p = RecyclePacer::new(t0);
        assert!(!p.allow(t0 + Duration::from_secs(30)));
        assert!(!p.allow(t0 + Duration::from_secs(599)));
        assert!(p.allow(t0 + Duration::from_secs(600)));
        assert!(!p.allow(t0 + Duration::from_secs(900)));
        assert!(p.allow(t0 + Duration::from_secs(1300)));
    }
    #[test]
    fn degradation_fires_after_sustained_regression_then_rebases() {
        let mut d = RttDegradation::default();
        for _ in 0..10 {
            assert!(!d.observe(ms(50)));
        }
        for _ in 0..4 {
            assert!(!d.observe(ms(200)));
        }
        assert!(d.observe(ms(200)));
        for _ in 0..10 {
            assert!(!d.observe(ms(200)));
        }
        assert!(!d.observe(ms(50)));
        for _ in 0..4 {
            assert!(!d.observe(ms(200)));
        }
        assert!(d.observe(ms(200)));
    }

    /// The degradation threshold is `srtt >= DEGRADED_RTT_RATIO * best`: a
    /// sample *exactly* at the ratio already counts toward the streak (only a
    /// strictly smaller one resets it), and a sample just below it does not.
    /// `1s` and `3s` are exact in `as_secs_f64` and `1.0 * 3.0` is exact, so
    /// the boundary is the same expression the code evaluates rather than an
    /// f64 approximation — and because the ratio is the literal `3.0` here, a
    /// retune (e.g. to `3.1`) drops `3s` strictly below the threshold and
    /// resets the streak instead. The tests in this module only ever use a 4x
    /// regression (200ms against a 50ms best), which stays degraded for any
    /// ratio from 3.0 up to 4.0, so none of them observes the boundary.
    #[test]
    fn the_degradation_ratio_boundary_counts_a_sample_exactly_at_the_ratio() {
        let mut d = RttDegradation::default();
        assert!(!d.observe(Duration::from_secs(1)));
        for _ in 0..DEGRADED_PROBE_STREAK - 1 {
            assert!(!d.observe(Duration::from_secs(3)));
        }
        assert!(
            d.observe(Duration::from_secs(3)),
            "a 3x regression must fire after DEGRADED_PROBE_STREAK observations"
        );

        // A sample strictly below the ratio resets the streak.
        let mut d = RttDegradation::default();
        assert!(!d.observe(Duration::from_secs(1)));
        for _ in 0..DEGRADED_PROBE_STREAK - 1 {
            assert!(!d.observe(Duration::from_secs(3)));
        }
        assert!(
            !d.observe(Duration::from_secs(2)),
            "2x is below the ratio and must reset the streak"
        );
        assert!(
            !d.observe(Duration::from_secs(3)),
            "the streak restarted at one, so it must not fire yet"
        );
    }

    #[test]
    fn degradation_streak_resets_when_rtt_recovers() {
        let mut d = RttDegradation::default();
        assert!(!d.observe(ms(50)));
        for _ in 0..4 {
            assert!(!d.observe(ms(200)));
        }
        assert!(!d.observe(ms(60)));
        for _ in 0..4 {
            assert!(!d.observe(ms(200)));
        }
        assert!(d.observe(ms(200)));
    }
}
