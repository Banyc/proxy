use std::{
    fmt,
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use tracing::{info, trace};

use crate::error::AnyError;

use super::degradation::{RecyclePacer, RttDegradation};
use super::rtt_stats::{RttStats, ewma_loss};
use super::{ConnChain, PROBE_ROUND_INTERVAL};

pub const PROBE_DEAD_INTERVAL: Duration = Duration::from_secs(60 * 2);
const PROBES_PER_INTERVAL: u32 = 5;
const PROBE_MEAN_INTERVAL: Duration =
    Duration::from_millis(PROBE_ROUND_INTERVAL.as_millis() as u64 / PROBES_PER_INTERVAL as u64);
const PROBE_MEAN_INTERVAL_DEAD: Duration =
    Duration::from_millis(PROBE_DEAD_INTERVAL.as_millis() as u64 / PROBES_PER_INTERVAL as u64);
const PROBE_MIN_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_MAX_INTERVAL: Duration = Duration::from_secs(60);
const DEAD_CONSECUTIVE_FAILURES: u32 = 5;
const RTT_TIMEOUT: Duration = Duration::from_secs(5);

fn poisson_interval(mean: Duration) -> Duration {
    let u: f64 = rand::random::<f64>().clamp(f64::MIN_POSITIVE, 1.0);
    let d = Duration::from_secs_f64(mean.as_secs_f64() * -u.ln());
    d.clamp(PROBE_MIN_INTERVAL, PROBE_MAX_INTERVAL)
}

pub struct DisplayChain<'chain>(&'chain ConnChain);
impl fmt::Display for DisplayChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, c) in self.0.iter().enumerate() {
            write!(f, "{}", c.address)?;
            if i + 1 != self.0.len() {
                write!(f, ",")?;
            }
        }
        write!(f, "]")?;
        Ok(())
    }
}

/// The outcome of one probe round: the RTT sample, plus any teardown
/// epilog produced by the probe that must be owned and reaped by the
/// caller — `probe_task`, through its function-scoped `JoinSet`.
pub struct ProbeOutcome {
    /// The round's RTT sample: `Ok` when a response was in hand, `Err`
    /// when the probe failed.
    pub rtt: Result<Duration, AnyError>,
    /// The teardown epilog future to spawn into the caller's `JoinSet`, if
    /// the probe flow needs observing after the round (e.g. awaiting the
    /// flow's end after the write-half shutdown). `None` when there is
    /// nothing to observe. The caller reaps it with `.unwrap()` so a
    /// panicked epilog re-raises instead of being swallowed.
    pub epilog: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

pub trait ProbeRtt {
    fn probe_rtt(
        &self,
        chain: &ConnChain,
    ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>>;
    /// The probe kind for prober logs, e.g. `"udp"` or `"stream"`.
    fn probe_kind(&self) -> &'static str {
        "unknown"
    }
    fn recycle(&self, _chain: &ConnChain) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
    fn reoptimize(&self, _chain: &ConnChain) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
    fn session_stats(
        &self,
        _chain: &ConnChain,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(async { None })
    }
}

pub(crate) async fn probe_task(
    tracer: Arc<dyn ProbeRtt + Send + Sync>,
    chain: Arc<ConnChain>,
    rtt_stats_store: Arc<RwLock<RttStats>>,
    loss_store: Arc<RwLock<Option<f64>>>,
    cancellation: CancellationToken,
) {
    let mut consecutive_failures: u32 = 0;
    let mut probes_since_log: u32 = 0;
    let mut degradation = RttDegradation::default();
    let mut pacer = RecyclePacer::new(std::time::Instant::now());
    let mut reoptimize_pacer = RecyclePacer::new(std::time::Instant::now());
    let kind = tracer.probe_kind();
    // Each probe round's teardown epilog is owned here, in this task's
    // scope: the future is spawned into this JoinSet instead of escaping
    // as a detached `tokio::spawn`. The JoinSet aborts any outstanding
    // epilog when this task ends, and completed epilogs are reaped below
    // with `.unwrap()`, so a panicked epilog re-raises out of `probe_task`
    // (surfacing at the commit-time JoinSet reap) rather than being
    // silently swallowed by the runtime.
    let mut epilogs = tokio::task::JoinSet::new();
    while !cancellation.is_cancelled() {
        let sample = match tokio::time::timeout(RTT_TIMEOUT, tracer.probe_rtt(&chain)).await {
            Ok(outcome) => {
                if let Some(epilog) = outcome.epilog {
                    epilogs.spawn(epilog);
                }
                match outcome.rtt {
                    Ok(rtt) => Some(rtt),
                    Err(e) => {
                        trace!(kind, "probe error: {e:?}");
                        None
                    }
                }
            }
            Err(_) => {
                trace!(kind, "probe timeout");
                None
            }
        };
        // Reap epilogs that finished while this round ran; a panicked
        // epilog re-raises here, cascading out of probe_task. `try_join_next`
        // never blocks, so the probe cadence is undisturbed.
        while let Some(joined) = epilogs.try_join_next() {
            joined.unwrap();
        }
        if cancellation.is_cancelled() {
            break;
        }
        let (rtt, rttvar, rtt_eff) = {
            let mut store = rtt_stats_store.write().unwrap();
            if let Some(sample) = sample {
                store.apply_sample(sample);
            }
            (store.srtt, store.rttvar, store.effective())
        };
        let loss = {
            let mut store = loss_store.write().unwrap();
            *store = Some(ewma_loss(*store, if sample.is_some() { 0. } else { 1. }));
            *store
        };
        consecutive_failures = if sample.is_some() {
            0
        } else {
            consecutive_failures.saturating_add(1)
        };
        if reoptimize_pacer.allow(std::time::Instant::now()) {
            let addresses = DisplayChain(&chain);
            trace!(%addresses, kind, "Timer reoptimize: reoptimizing first-hop relay");
            let _ = tokio::time::timeout(RTT_TIMEOUT, tracer.reoptimize(&chain)).await;
        }
        if let (Some(_), Some(srtt)) = (sample, rtt)
            && degradation.observe(srtt)
        {
            let mux = tracer.session_stats(&chain).await;
            let addresses = DisplayChain(&chain);
            if pacer.allow(std::time::Instant::now()) {
                info!(%addresses, kind, ?srtt, ?rtt_eff, ?mux, "Chain RTT degraded; recycling first-hop session");
                let _ = tokio::time::timeout(RTT_TIMEOUT, tracer.recycle(&chain)).await;
            } else {
                info!(%addresses, kind, ?srtt, ?rtt_eff, ?mux, "Chain RTT degraded; recycle suppressed (min interval), accepting as new baseline");
            }
        }
        probes_since_log += 1;
        if probes_since_log >= PROBES_PER_INTERVAL {
            probes_since_log = 0;
            let mux = tracer.session_stats(&chain).await;
            let addresses = DisplayChain(&chain);
            info!(%addresses, kind, sample = ?sample, rtt = ?rtt, rttvar = ?rttvar, rtt_eff = ?rtt_eff, ?loss, ?mux, "Probed RTT");
        }
        let mean = if consecutive_failures >= DEAD_CONSECUTIVE_FAILURES {
            PROBE_MEAN_INTERVAL_DEAD
        } else {
            PROBE_MEAN_INTERVAL
        };
        tokio::select! {
            () = tokio::time::sleep(poisson_interval(mean)) => {}
            () = cancellation.cancelled() => {}
        }
    }
    // Outstanding epilogs are aborted and reaped before cancellation
    // returns: a completed epilog that beat the cancellation still surfaces
    // (its panic cascades) instead of being swallowed by the JoinSet drop.
    crate::task_scope::abort_and_reap(&mut epilogs).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn poisson_interval_respects_clamp_bounds() {
        for _ in 0..1000 {
            let d = poisson_interval(PROBE_MEAN_INTERVAL);
            assert!(
                (PROBE_MIN_INTERVAL..=PROBE_MAX_INTERVAL).contains(&d),
                "{d:?}"
            );
        }
    }

    struct FakeTracer {
        calls: Arc<AtomicUsize>,
    }
    impl ProbeRtt for FakeTracer {
        fn probe_rtt(
            &self,
            _chain: &ConnChain,
        ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ProbeOutcome {
                    rtt: Ok(Duration::from_millis(10)),
                    epilog: None,
                }
            })
        }
    }

    #[tokio::test]
    async fn a_fake_tracer_drives_the_probe_loop_until_cancellation() {
        use crate::route::ConnConfig;

        let calls = Arc::new(AtomicUsize::new(0));
        let chain: Arc<ConnChain> = Arc::from(Vec::<ConnConfig>::new());
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let cancellation = CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(probe_task(
            Arc::new(FakeTracer {
                calls: calls.clone(),
            }),
            chain,
            rtt_stats,
            loss,
            cancellation.clone(),
        ));
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "the fake tracer should be polled at least once"
        );
        cancellation.cancel();
        while let Some(res) = tasks.join_next().await {
            res.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_reaps_the_probe_epilog_before_returning() {
        use crate::route::ConnConfig;

        struct EpilogTracer {
            started: Arc<tokio::sync::Notify>,
        }
        impl ProbeRtt for EpilogTracer {
            fn probe_rtt(
                &self,
                _chain: &ConnChain,
            ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
                let started = Arc::clone(&self.started);
                Box::pin(async move {
                    ProbeOutcome {
                        rtt: Ok(Duration::from_millis(10)),
                        // A parked epilog: it starts, then never finishes on
                        // its own, so only the task-scope epilog can reap it.
                        epilog: Some(Box::pin(async move {
                            started.notify_waiters();
                            std::future::pending::<()>().await;
                        })),
                    }
                })
            }
        }

        let epilog_started = Arc::new(tokio::sync::Notify::new());
        let chain: Arc<ConnChain> = Arc::from(Vec::<ConnConfig>::new());
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let cancellation = CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(probe_task(
            Arc::new(EpilogTracer {
                started: Arc::clone(&epilog_started),
            }),
            chain,
            rtt_stats,
            loss,
            cancellation.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(5), epilog_started.notified())
            .await
            .expect("the probe epilog was never spawned");
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(res) = tasks.join_next().await {
                res.unwrap();
            }
        })
        .await
        .expect("probe_task must abort and reap its outstanding epilog before returning");
    }
}
