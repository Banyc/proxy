use std::time::Duration;

const RTT_REF: Duration = Duration::from_millis(100);
const LOSS_EXP: i32 = 3;
const RTT_EXP: i32 = 1;

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
    pub(crate) fn new(max_ratio: f64) -> Self {
        Self { max_ratio }
    }
    pub(crate) fn retain_eligible(&self, chains: &mut Vec<ScoredChain>) -> usize {
        let Some(best) = chains.iter().filter_map(|c| c.rtt).min() else {
            return 0;
        };
        let cutoff = Duration::try_from_secs_f64(best.as_secs_f64() * self.max_ratio)
            .unwrap_or(Duration::MAX);
        let before = chains.len();
        chains.retain(|c| c.rtt.is_none_or(|rtt| rtt <= cutoff));
        before - chains.len()
    }
}

pub(crate) fn pick_weighted(scores: &[(usize, f64)], r: f64) -> usize {
    let mut cum = 0.;
    for (index, score) in scores {
        cum += score;
        if r < cum {
            return *index;
        }
    }
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
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn lower_loss_scores_higher() {
        assert!(
            chain_score(1., Some(0.0), Some(ms(50))) > chain_score(1., Some(0.10), Some(ms(50)))
        );
    }
    #[test]
    fn lower_rtt_scores_higher() {
        assert!(
            chain_score(1., Some(0.0), Some(ms(10))) > chain_score(1., Some(0.0), Some(ms(300)))
        );
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
        ScoredChain {
            index,
            score,
            rtt: Some(rtt),
        }
    }

    fn unmeasured(index: usize, score: f64) -> ScoredChain {
        ScoredChain {
            index,
            score,
            rtt: None,
        }
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
    fn an_enormous_ratio_admits_everything_instead_of_panicking() {
        let chains = vec![measured(0, 1.0, ms(1)), measured(1, 0.5, ms(5000))];
        assert_eq!(survivors(chains, 1e30), vec![0, 1]);
        let chains = vec![measured(0, 1.0, ms(1)), measured(1, 0.5, ms(5000))];
        assert_eq!(survivors(chains, f64::MAX), vec![0, 1]);
    }

    #[test]
    fn gate_bar_ignores_optimistic_unmeasured_scores() {
        let chains = vec![
            measured(0, 0.40, ms(50)),
            measured(1, 0.30, ms(200)),
            unmeasured(2, 0.90),
        ];
        assert_eq!(survivors(chains, 1.5), vec![0, 2]);
    }

    #[test]
    fn gate_excludes_a_dead_from_start_chain() {
        let dead = ScoredChain {
            index: 1,
            score: 0.0,
            rtt: Some(ms(5000)),
        };
        let chains = vec![measured(0, 0.4, ms(100)), dead];
        assert_eq!(survivors(chains, 1.5), vec![0]);
    }
}
