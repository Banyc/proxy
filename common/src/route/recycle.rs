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
