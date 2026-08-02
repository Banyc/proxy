use std::time::Duration;

const RTT_EWMA_ALPHA: f64 = 0.07;
const RTT_MEDIAN_WINDOW: usize = 5;
const RTTVAR_EWMA_BETA: f64 = 0.25;
const RTT_EFF_VAR_FACTOR: u32 = 2;
const LOSS_EWMA_ALPHA_UP: f64 = 0.21;
const LOSS_EWMA_ALPHA_DOWN: f64 = 0.07;

fn median_duration(rtts: &mut [Duration]) -> Option<Duration> {
    if rtts.is_empty() {
        return None;
    }
    rtts.sort_unstable();
    let mid = rtts.len() / 2;
    Some(if rtts.len() % 2 == 1 {
        rtts[mid]
    } else {
        (rtts[mid - 1] + rtts[mid]) / 2
    })
}

fn ewma_duration(prev: Option<Duration>, sample: Option<Duration>, alpha: f64) -> Option<Duration> {
    match (prev, sample) {
        (_, None) => prev,
        (None, Some(s)) => Some(s),
        (Some(p), Some(s)) => {
            let blended = p.as_secs_f64() * (1. - alpha) + s.as_secs_f64() * alpha;
            Some(Duration::from_secs_f64(blended))
        }
    }
}

pub(crate) fn ewma_loss(prev: Option<f64>, sample: f64) -> f64 {
    match prev {
        None => sample,
        Some(p) => {
            let alpha = if sample > p {
                LOSS_EWMA_ALPHA_UP
            } else {
                LOSS_EWMA_ALPHA_DOWN
            };
            p * (1. - alpha) + sample * alpha
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RttStats {
    pub srtt: Option<Duration>,
    pub rttvar: Option<Duration>,
    recent: Vec<Duration>,
}

impl RttStats {
    pub(crate) fn apply_sample(&mut self, sample: Duration) {
        if self.recent.len() >= RTT_MEDIAN_WINDOW {
            self.recent.remove(0);
        }
        self.recent.push(sample);
        let mut window = self.recent.clone();
        let rolling_median = median_duration(&mut window).expect("window is non-empty");
        match self.srtt {
            None => {
                self.srtt = Some(sample);
                self.rttvar = Some(sample / 2);
            }
            Some(srtt) => {
                let dev = sample.abs_diff(srtt).as_secs_f64();
                let prev = self.rttvar.unwrap_or(Duration::ZERO).as_secs_f64();
                self.rttvar = Some(Duration::from_secs_f64(
                    prev * (1. - RTTVAR_EWMA_BETA) + dev * RTTVAR_EWMA_BETA,
                ));
                self.srtt = ewma_duration(self.srtt, Some(rolling_median), RTT_EWMA_ALPHA);
            }
        }
    }
    pub fn effective(&self) -> Option<Duration> {
        self.srtt
            .map(|s| s + self.rttvar.unwrap_or(Duration::ZERO) * RTT_EFF_VAR_FACTOR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn median_is_outlier_robust() {
        assert_eq!(
            median_duration(&mut [ms(10), ms(20), ms(5000)]),
            Some(ms(20))
        );
    }
    #[test]
    fn median_even_count_averages_the_two_middles() {
        assert_eq!(
            median_duration(&mut [ms(40), ms(10), ms(30), ms(20)]),
            Some(ms(25))
        );
    }
    #[test]
    fn median_of_empty_wave_is_none() {
        assert_eq!(median_duration(&mut []), None);
    }
    #[test]
    fn ewma_duration_first_sample_seeds_the_estimate() {
        assert_eq!(ewma_duration(None, Some(ms(100)), 0.07), Some(ms(100)));
    }
    #[test]
    fn ewma_duration_dead_wave_keeps_previous_estimate() {
        assert_eq!(ewma_duration(Some(ms(100)), None, 0.07), Some(ms(100)));
        assert_eq!(ewma_duration(None, None, 0.07), None);
    }
    #[test]
    fn ewma_duration_blends_toward_the_new_sample() {
        let blended = ewma_duration(Some(ms(100)), Some(ms(200)), 0.07).unwrap();
        assert!((blended.as_secs_f64() - 0.107).abs() < 1e-9);
    }
    #[test]
    fn ewma_loss_is_fast_to_distrust_slow_to_trust() {
        assert!((ewma_loss(None, 0.4) - 0.4).abs() < f64::EPSILON);
        let up = ewma_loss(Some(0.2), 1.0);
        assert!((up - 0.368).abs() < 1e-9);
        let down = ewma_loss(Some(0.8), 0.0);
        assert!((down - 0.744).abs() < 1e-9);
        let up_move = ewma_loss(Some(0.5), 1.0) - 0.5;
        let down_move = 0.5 - ewma_loss(Some(0.5), 0.0);
        assert!(up_move > down_move);
        assert!((0.0..=1.0).contains(&up) && (0.0..=1.0).contains(&down));
    }

    #[test]
    fn rtt_stats_first_sample_seeds_rfc6298_pessimistic() {
        let mut s = RttStats::default();
        s.apply_sample(ms(100));
        assert_eq!(s.srtt, Some(ms(100)));
        assert_eq!(s.rttvar, Some(ms(50)));
        assert_eq!(s.effective(), Some(ms(200)));
    }

    #[test]
    fn steady_route_beats_jittery_route_with_similar_median() {
        let mut steady = RttStats::default();
        let mut jittery = RttStats::default();
        for _ in 0..20 {
            steady.apply_sample(ms(150));
            jittery.apply_sample(ms(150));
            jittery.apply_sample(ms(900));
        }
        let s = steady.effective().unwrap();
        let j = jittery.effective().unwrap();
        assert!(
            s + ms(100) < j,
            "steady {} vs jittery {}",
            s.as_millis(),
            j.as_millis()
        );
    }

    #[test]
    fn rttvar_decays_once_the_path_calms_down() {
        let mut stats = RttStats::default();
        for _ in 0..10 {
            stats.apply_sample(ms(200));
            stats.apply_sample(ms(800));
        }
        let noisy_rttvar = stats.rttvar;
        for _ in 0..40 {
            stats.apply_sample(ms(150));
        }
        let calm_rttvar = stats.rttvar;
        assert!(calm_rttvar.is_some() && noisy_rttvar.is_some());
        assert!(
            calm_rttvar.unwrap() < noisy_rttvar.unwrap() / 4,
            "calm {:?} not below quarter of noisy {:?}",
            calm_rttvar,
            noisy_rttvar
        );
    }

    #[test]
    fn rolling_median_keeps_srtt_outlier_robust() {
        let mut stats = RttStats::default();
        for _ in 0..5 {
            stats.apply_sample(ms(150));
        }
        let before = stats.srtt;
        stats.apply_sample(ms(5000));
        let after = stats.srtt;
        assert!(after.is_some() && before.is_some());
        let drift = after.unwrap().abs_diff(before.unwrap());
        assert!(drift < ms(2), "srtt drifted by {:?}", drift);
        assert!(
            stats.rttvar.unwrap() > ms(1000),
            "rttvar {:?} not above 1s",
            stats.rttvar
        );
    }
}
