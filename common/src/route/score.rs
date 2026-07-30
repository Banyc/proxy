use std::time::Duration;

const RTT_EWMA_ALPHA: f64 = 0.07;
const RTT_MEDIAN_WINDOW: usize = 5;
const RTTVAR_EWMA_BETA: f64 = 0.25;
const RTT_EFF_VAR_FACTOR: u32 = 2;
const LOSS_EWMA_ALPHA_UP: f64 = 0.21;
const LOSS_EWMA_ALPHA_DOWN: f64 = 0.07;
const RTT_REF: Duration = Duration::from_millis(100);
const LOSS_EXP: i32 = 3;
const RTT_EXP: i32 = 1;

fn median_duration(rtts: &mut [Duration]) -> Option<Duration> {
    if rtts.is_empty() { return None; }
    rtts.sort_unstable();
    let mid = rtts.len() / 2;
    Some(if rtts.len() % 2 == 1 { rtts[mid] } else { (rtts[mid - 1] + rtts[mid]) / 2 })
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
            let alpha = if sample > p { LOSS_EWMA_ALPHA_UP } else { LOSS_EWMA_ALPHA_DOWN };
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
        if self.recent.len() >= RTT_MEDIAN_WINDOW { self.recent.remove(0); }
        self.recent.push(sample);
        let mut window = self.recent.clone();
        let rolling_median = median_duration(&mut window).expect("window is non-empty");
        match self.srtt {
            None => { self.srtt = Some(sample); self.rttvar = Some(sample / 2); }
            Some(srtt) => {
                let dev = sample.abs_diff(srtt).as_secs_f64();
                let prev = self.rttvar.unwrap_or(Duration::ZERO).as_secs_f64();
                self.rttvar = Some(Duration::from_secs_f64(prev * (1. - RTTVAR_EWMA_BETA) + dev * RTTVAR_EWMA_BETA));
                self.srtt = ewma_duration(self.srtt, Some(rolling_median), RTT_EWMA_ALPHA);
            }
        }
    }
    pub fn effective(&self) -> Option<Duration> {
        self.srtt.map(|s| s + self.rttvar.unwrap_or(Duration::ZERO) * RTT_EFF_VAR_FACTOR)
    }
}

pub(crate) struct ScoredChain {
    pub(crate) index: usize,
    pub(crate) score: f64,
    pub(crate) rtt: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EligibilityGate {
    pub(crate) max_ratio: f64,
}

impl EligibilityGate {
    pub(crate) fn new(max_ratio: f64) -> Self { Self { max_ratio } }
    pub(crate) fn retain_eligible(&self, chains: &mut Vec<ScoredChain>) -> usize {
        let Some(best) = chains.iter().filter_map(|c| c.rtt).min() else { return 0; };
        let cutoff = best.mul_f64(self.max_ratio);
        let before = chains.len();
        chains.retain(|c| c.rtt.is_none_or(|rtt| rtt <= cutoff));
        before - chains.len()
    }
}

pub(crate) fn pick_weighted(scores: &[(usize, f64)], r: f64) -> usize {
    let mut cum = 0.;
    for (index, score) in scores { cum += score; if r < cum { return *index; } }
    scores.last().map(|(i, _)| *i).unwrap_or(0)
}

pub(crate) fn chain_score(weight: f64, loss: Option<f64>, rtt: Option<Duration>) -> f64 {
    let r0 = RTT_REF.as_secs_f64();
    let loss = loss.unwrap_or(0.).clamp(0., 1.);
    let rtt = rtt.map(|r| r.as_secs_f64()).unwrap_or(r0);
    let loss_factor = (1. - loss).powi(LOSS_EXP);
    let rtt_factor = (1. / (1. + rtt / r0)).powi(RTT_EXP);
    weight * loss_factor * rtt_factor
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(n: u64) -> Duration { Duration::from_millis(n) }

    #[test]
    fn median_is_outlier_robust() {
        assert_eq!(median_duration(&mut [ms(10), ms(20), ms(5000)]), Some(ms(20)));
    }
    #[test]
    fn median_even_count_averages_the_two_middles() {
        assert_eq!(median_duration(&mut [ms(40), ms(10), ms(30), ms(20)]), Some(ms(25)));
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
    fn lower_loss_scores_higher() {
        assert!(chain_score(1., Some(0.0), Some(ms(50))) > chain_score(1., Some(0.10), Some(ms(50))));
    }
    #[test]
    fn lower_rtt_scores_higher() {
        assert!(chain_score(1., Some(0.0), Some(ms(10))) > chain_score(1., Some(0.0), Some(ms(300))));
    }
    #[test]
    fn a_lossy_chain_is_de_preferred_but_never_excluded() {
        let lossy = chain_score(1., Some(0.10), Some(ms(50)));
        assert!(lossy > 0.);
        assert!(lossy > chain_score(1., Some(0.0), Some(ms(50))) * 0.5);
    }
    #[test]
    fn latency_factor_is_one_half_at_the_reference() {
        assert!((chain_score(1., Some(0.0), Some(RTT_REF)) - 0.5).abs() < 1e-9);
    }
    #[test]
    fn small_latency_differences_barely_matter() {
        let a = chain_score(1., Some(0.0), Some(ms(10)));
        let b = chain_score(1., Some(0.0), Some(ms(20)));
        assert!(b / a > 0.85, "ratio was {}", b / a);
    }
    #[test]
    fn unknown_metrics_are_neutral() {
        assert!((chain_score(1., None, None) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn pick_weighted_selects_the_bucket_containing_r() {
        let s = [(0usize, 0.7_f64), (1, 0.3)];
        assert_eq!(pick_weighted(&s, 0.0), 0);
        assert_eq!(pick_weighted(&s, 0.69), 0);
        assert_eq!(pick_weighted(&s, 0.70), 1);
        assert_eq!(pick_weighted(&s, 0.99), 1);
    }
    #[test]
    fn pick_weighted_returns_the_chain_index_not_the_position() {
        let s = [(3usize, 0.5_f64), (1, 0.5)];
        assert_eq!(pick_weighted(&s, 0.2), 3);
        assert_eq!(pick_weighted(&s, 0.7), 1);
    }
    #[test]
    fn pick_weighted_single_bucket_always_wins() {
        assert_eq!(pick_weighted(&[(5usize, 1.0_f64)], 0.0), 5);
        assert_eq!(pick_weighted(&[(5usize, 1.0_f64)], 0.999), 5);
    }
    #[test]
    fn pick_weighted_overshoot_falls_back_to_last_instead_of_panicking() {
        assert_eq!(pick_weighted(&[(0usize, 0.3_f64), (1, 0.3)], 0.6), 1);
    }

    fn measured(index: usize, score: f64, rtt: Duration) -> ScoredChain {
        ScoredChain { index, score, rtt: Some(rtt) }
    }

    fn unmeasured(index: usize, score: f64) -> ScoredChain {
        ScoredChain { index, score, rtt: None }
    }

    fn survivors(mut chains: Vec<ScoredChain>, fraction: f64) -> Vec<usize> {
        EligibilityGate::new(fraction).retain_eligible(&mut chains);
        let mut ids: Vec<usize> = chains.into_iter().map(|c| c.index).collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn gate_keeps_the_fast_tier_and_drops_the_slow_one() {
        let s = |rtt| chain_score(1.0 / 6.0, Some(0.0), rtt);
        let chains = vec![
            measured(0, s(Some(ms(150))), ms(150)),
            measured(1, s(Some(ms(150))), ms(150)),
            measured(2, s(Some(ms(150))), ms(150)),
            measured(3, s(Some(ms(150))), ms(150)),
            measured(4, s(Some(ms(250))), ms(250)),
            measured(5, s(Some(ms(250))), ms(250)),
        ];
        assert_eq!(survivors(chains, 1.5), vec![0, 1, 2, 3]);
    }

    #[test]
    fn gate_is_a_no_op_until_something_is_measured() {
        let chains = vec![unmeasured(0, 0.9), unmeasured(1, 0.1)];
        assert_eq!(survivors(chains, 0.8), vec![0, 1]);
    }

    #[test]
    fn gate_never_excludes_an_unmeasured_chain() {
        let chains = vec![measured(0, 1.0, ms(100)), unmeasured(1, 0.1)];
        assert_eq!(survivors(chains, 2.0), vec![0, 1]);
    }

    #[test]
    fn gate_bar_ignores_optimistic_unmeasured_scores() {
        let chains = vec![measured(0, 0.40, ms(50)), measured(1, 0.30, ms(200)), unmeasured(2, 0.90)];
        assert_eq!(survivors(chains, 1.5), vec![0, 2]);
    }

    #[test]
    fn gate_excludes_a_dead_from_start_chain() {
        let dead = ScoredChain { index: 1, score: 0.0, rtt: Some(ms(5000)) };
        let chains = vec![measured(0, 0.4, ms(100)), dead];
        assert_eq!(survivors(chains, 1.5), vec![0]);
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
        assert!(s + ms(100) < j, "steady {} vs jittery {}", s.as_millis(), j.as_millis());
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
        assert!(calm_rttvar.unwrap() < noisy_rttvar.unwrap() / 4,
            "calm {:?} not below quarter of noisy {:?}", calm_rttvar, noisy_rttvar);
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
        assert!(stats.rttvar.unwrap() > ms(1000), "rttvar {:?} not above 1s", stats.rttvar);
    }
}
