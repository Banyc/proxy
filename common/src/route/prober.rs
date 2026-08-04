use std::{
    fmt,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use tracing::{info, trace};

use crate::error::AnyError;

use super::recycle::{RecyclePacer, RttDegradation};
use super::rtt_stats::{RttStats, ewma_loss};
use super::{ConnChain, TRACE_INTERVAL};

pub const TRACE_DEAD_INTERVAL: Duration = Duration::from_secs(60 * 2);
const PROBES_PER_INTERVAL: u32 = 5;
const PROBE_MEAN_INTERVAL: Duration =
    Duration::from_millis(TRACE_INTERVAL.as_millis() as u64 / PROBES_PER_INTERVAL as u64);
const PROBE_MEAN_INTERVAL_DEAD: Duration =
    Duration::from_millis(TRACE_DEAD_INTERVAL.as_millis() as u64 / PROBES_PER_INTERVAL as u64);
const PROBE_MIN_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_MAX_INTERVAL: Duration = Duration::from_secs(60);
const DEAD_CONSECUTIVE_FAILURES: u32 = 5;
const RTT_TIMEOUT: Duration = Duration::from_secs(5);

fn poisson_interval(mean: Duration) -> Duration {
    let u: f64 = rand::random::<f64>().clamp(f64::MIN_POSITIVE, 1.0);
    let d = Duration::from_secs_f64(mean.as_secs_f64() * -u.ln());
    d.clamp(PROBE_MIN_INTERVAL, PROBE_MAX_INTERVAL)
}

pub struct DisplayChain<'chain, Addr>(&'chain ConnChain<Addr>);
impl<Addr> fmt::Display for DisplayChain<'_, Addr>
where
    Addr: fmt::Display,
{
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

pub trait TraceRtt {
    type Addr;
    fn trace_rtt(
        &self,
        chain: &ConnChain<Self::Addr>,
    ) -> impl Future<Output = Result<Duration, AnyError>> + Send;
    fn recycle(&self, _chain: &ConnChain<Self::Addr>) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn reoptimize(&self, _chain: &ConnChain<Self::Addr>) -> impl Future<Output = ()> + Send {
        async {}
    }
    fn session_stats(
        &self,
        _chain: &ConnChain<Self::Addr>,
    ) -> impl Future<Output = Option<String>> + Send {
        async { None }
    }
}

pub(crate) fn spawn_tracer<Tracer, Addr>(
    tracer: Arc<Tracer>,
    chain: Arc<ConnChain<Addr>>,
    rtt_stats_store: Arc<RwLock<RttStats>>,
    loss_store: Arc<RwLock<Option<f64>>>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()>
where
    Tracer: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    Addr: fmt::Display + Send + Sync + 'static,
{
    tokio::task::spawn(async move {
        let mut consecutive_failures: u32 = 0;
        let mut probes_since_log: u32 = 0;
        let mut degradation = RttDegradation::default();
        let mut pacer = RecyclePacer::new(std::time::Instant::now());
        let mut reoptimize_pacer = RecyclePacer::new(std::time::Instant::now());
        while !cancellation.is_cancelled() {
            let sample = match tokio::time::timeout(RTT_TIMEOUT, tracer.trace_rtt(&chain)).await {
                Ok(Ok(rtt)) => Some(rtt),
                Ok(Err(e)) => {
                    trace!("trace error: {e:?}");
                    None
                }
                Err(_) => {
                    trace!("trace timeout");
                    None
                }
            };
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
                trace!(%addresses, "Timer reoptimize: offering a first-hop re-lay");
                let _ = tokio::time::timeout(RTT_TIMEOUT, tracer.reoptimize(&chain)).await;
            }
            if let (Some(_), Some(srtt)) = (sample, rtt)
                && degradation.observe(srtt)
            {
                let mux = tracer.session_stats(&chain).await;
                let addresses = DisplayChain(&chain);
                if pacer.allow(std::time::Instant::now()) {
                    info!(%addresses, ?srtt, ?rtt_eff, ?mux, "Chain RTT degraded; recycling first-hop session");
                    let _ = tokio::time::timeout(RTT_TIMEOUT, tracer.recycle(&chain)).await;
                } else {
                    info!(%addresses, ?srtt, ?rtt_eff, ?mux, "Chain RTT degraded; recycle suppressed (min interval), accepting as new baseline");
                }
            }
            probes_since_log += 1;
            if probes_since_log >= PROBES_PER_INTERVAL {
                probes_since_log = 0;
                let mux = tracer.session_stats(&chain).await;
                let addresses = DisplayChain(&chain);
                info!(%addresses, sample = ?sample, rtt = ?rtt, rttvar = ?rttvar, rtt_eff = ?rtt_eff, ?loss, ?mux, "Traced RTT");
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
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
    impl TraceRtt for FakeTracer {
        type Addr = SocketAddr;
        async fn trace_rtt(&self, _chain: &ConnChain<SocketAddr>) -> Result<Duration, AnyError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Duration::from_millis(10))
        }
    }

    #[tokio::test]
    async fn a_fake_tracer_drives_the_probe_loop_until_cancellation() {
        use crate::route::ConnConfig;

        let calls = Arc::new(AtomicUsize::new(0));
        let chain: Arc<ConnChain<SocketAddr>> = Arc::from(Vec::<ConnConfig<SocketAddr>>::new());
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let cancellation = CancellationToken::new();
        let handle = spawn_tracer(
            Arc::new(FakeTracer {
                calls: calls.clone(),
            }),
            chain,
            rtt_stats,
            loss,
            cancellation.clone(),
        );
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "the fake tracer should be polled at least once"
        );
        cancellation.cancel();
        handle.await.unwrap();
    }
}
