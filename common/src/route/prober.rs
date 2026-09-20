use std::{
    fmt,
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use tracing::{info, trace};

use crate::{OptLog, clock::Clock, error::AnyError};

use super::degradation::{RecyclePacer, RttDegradation};
use super::rtt_stats::{RttStats, ewma_loss};
use super::{PROBE_ROUND_INTERVAL, RouteChain};

pub const PROBE_DEAD_INTERVAL: Duration = Duration::from_secs(60 * 2);
const PROBES_PER_INTERVAL: u32 = 5;
const PROBE_MEAN_INTERVAL: Duration =
    Duration::from_millis(PROBE_ROUND_INTERVAL.as_millis() as u64 / PROBES_PER_INTERVAL as u64);

/// A prober RTT value for log output: formatted with at most two decimals
/// and a human unit (s/ms/µs/ns), instead of `Duration`'s full-precision
/// Debug rendering.
struct RttLog(Option<Duration>);
impl fmt::Display for RttLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(d) = &self.0 {
            write!(f, "{}", fmt_rtt(*d))?;
        }
        Ok(())
    }
}
impl fmt::Debug for RttLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
/// A prober loss fraction for log output: at most two decimals, no trailing
/// `.0` (a zero loss prints `0`, not `0.0`).
struct LossLog(Option<f64>);
impl fmt::Display for LossLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(loss) = &self.0 {
            write!(f, "{}", super::fmt_2dec(*loss))?;
        }
        Ok(())
    }
}
impl fmt::Debug for LossLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
/// Rounds to at most two decimals, trimming trailing zeros (and a bare
/// decimal point), so `0.0` renders as `0` and `0.123456` as `0.12`.
fn fmt_rtt(d: Duration) -> String {
    let secs = d.as_secs_f64();
    let (value, unit) = if secs >= 1. {
        (secs, "s")
    } else if secs >= 1e-3 {
        (secs * 1e3, "ms")
    } else if secs >= 1e-6 {
        (secs * 1e6, "µs")
    } else {
        (secs * 1e9, "ns")
    };
    format!("{}{unit}", super::fmt_2dec(value))
}
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

pub struct DisplayChain<'chain>(&'chain RouteChain);
impl<'chain> DisplayChain<'chain> {
    pub fn new(chain: &'chain RouteChain) -> Self {
        Self(chain)
    }
}
impl fmt::Display for DisplayChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, c) in self.0.iter().enumerate() {
            // A named conn prints its name; an unnamed one falls back to
            // its raw address.
            write!(f, "{c}")?;
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
        chain: &RouteChain,
    ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>>;
    /// The probe kind for prober logs, e.g. `"udp"` or `"stream"`.
    fn probe_kind(&self) -> &'static str {
        "unknown"
    }
    fn recycle(&self, _chain: &RouteChain) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
    fn reoptimize(&self, _chain: &RouteChain) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
    fn session_stats(
        &self,
        _chain: &RouteChain,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(async { None })
    }
}

pub(crate) async fn probe_task(
    tracer: Arc<dyn ProbeRtt + Send + Sync>,
    chain: Arc<RouteChain>,
    rtt_stats_store: Arc<RwLock<RttStats>>,
    loss_store: Arc<RwLock<Option<f64>>>,
    clock: Arc<dyn Clock>,
    cancellation: CancellationToken,
) {
    let mut consecutive_failures: u32 = 0;
    let mut probes_since_log: u32 = 0;
    let mut degradation = RttDegradation::default();
    let mut pacer = RecyclePacer::new(clock.now());
    let mut reoptimize_pacer = RecyclePacer::new(clock.now());
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
                        trace!(kind, "probe error: {e}");
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
        if reoptimize_pacer.allow(clock.now()) {
            let addresses = DisplayChain(&chain);
            trace!(%addresses, kind, "Timer reoptimize: reoptimizing first-hop relay");
            let _ = tokio::time::timeout(RTT_TIMEOUT, tracer.reoptimize(&chain)).await;
        }
        if let (Some(_), Some(srtt)) = (sample, rtt)
            && degradation.observe(srtt)
        {
            let mux = tracer.session_stats(&chain).await;
            let addresses = DisplayChain(&chain);
            if pacer.allow(clock.now()) {
                info!(
                    %addresses,
                    kind,
                    srtt = ?RttLog(Some(srtt)),
                    rtt_eff = ?RttLog(rtt_eff),
                    mux = ?OptLog(mux.as_deref()),
                    "Chain RTT degraded; recycling first-hop session"
                );
                let _ = tokio::time::timeout(RTT_TIMEOUT, tracer.recycle(&chain)).await;
            } else {
                info!(
                    %addresses,
                    kind,
                    srtt = ?RttLog(Some(srtt)),
                    rtt_eff = ?RttLog(rtt_eff),
                    mux = ?OptLog(mux.as_deref()),
                    "Chain RTT degraded; recycle suppressed (min interval), accepting as new baseline"
                );
            }
        }
        probes_since_log += 1;
        if probes_since_log >= PROBES_PER_INTERVAL {
            probes_since_log = 0;
            let mux = tracer.session_stats(&chain).await;
            let addresses = DisplayChain(&chain);
            info!(
                %addresses,
                kind,
                sample = ?RttLog(sample),
                rtt = ?RttLog(rtt),
                rttvar = ?RttLog(rttvar),
                rtt_eff = ?RttLog(rtt_eff),
                loss = ?LossLog(loss),
                mux = ?OptLog(mux.as_deref()),
                "Probed RTT"
            );
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
    crate::lifecycle::task_scope::abort_and_reap(&mut epilogs).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A minimal `tracing::Subscriber` that renders every event's fields to
    /// an in-memory buffer (`?` fields through their `Debug` impl, which is
    /// what `RttLog`/`LossLog` implement), so a test can assert the *content*
    /// of the prober's logs — the observable record of which path a
    /// degraded chain took — instead of only counting tracer calls.
    /// Installed as the thread-local default (tests run on a current-thread
    /// runtime, so every spawned prober task shares it).
    #[derive(Default)]
    struct CaptureVisitor {
        parts: Vec<String>,
    }
    impl tracing::field::Visit for CaptureVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.parts.push(format!("{}={value:?}", field.name()));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.parts.push(format!("{}={value}", field.name()));
        }
        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.parts.push(format!("{}={value}", field.name()));
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.parts.push(format!("{}={value}", field.name()));
        }
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.parts.push(format!("{}={value}", field.name()));
        }
        fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
            self.parts.push(format!("{}={value}", field.name()));
        }
    }
    #[derive(Debug)]
    struct CaptureSubscriber {
        buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl tracing::Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = CaptureVisitor::default();
            event.record(&mut visitor);
            let mut buf = self.buf.lock().unwrap();
            buf.extend_from_slice(
                format!(
                    "[{}] {}\n",
                    event.metadata().target(),
                    visitor.parts.join(" ")
                )
                .as_bytes(),
            );
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn capture_logs() -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
        // Process-global, not thread-local, so a *concurrent* test sharing
        // the harness worker thread cannot displace the subscriber while
        // this test's prober task is still logging mid-run.
        let buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = std::sync::Arc::default();
        let _ = tracing::subscriber::set_global_default(CaptureSubscriber {
            buf: std::sync::Arc::clone(&buf),
        });
        buf
    }

    fn rendered(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    /// How often a rendered log line appears.
    fn occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    /// The process-global capture is shared with concurrent tests; keep only
    /// the events of the scripted prober (identified by its probe kind),
    /// which is what the assertions read.
    fn scripted_lines(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
        rendered(buf)
            .lines()
            .filter(|l| l.contains("kind=scripted-chain"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A tracer that scripts a long RTT degradation curve and advances the
    /// injected clock past the 600s pacer interval exactly once, between the
    /// second and third regression. All `ProbeRtt` methods except
    /// `probe_rtt` are left at their trait defaults, so the default
    /// no-op `recycle`/`reoptimize`/`session_stats` implementations are the
    /// ones driven.
    struct ScriptedChain {
        calls: AtomicUsize,
        clock: std::sync::Arc<crate::clock::test_support::ManualClock>,
    }
    impl ScriptedChain {
        /// The sample for the scripted round. The RTT store smooths samples
        /// through an EWMA of a rolling median, so the scripted *values* are
        /// the raw probes; the degradation fires on the smoothed value.
        /// Round 1..=7 (1.2s) degrades on the smoothed value while the
        /// injected clock is still at its start — suppressed by the 600s
        /// pacer. Calls 8..=13 (4.8s) re-degrade after the clock has been
        /// advanced by 600s — allowed to recycle. The rest exercise the
        /// second/second/micro/nano rendering units and the failure tail.
        fn sample(&self) -> Option<Duration> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 6 {
                // Cross the 600s RecyclePacer exactly once, mid-script.
                self.clock.advance(Duration::from_secs(600));
            }
            match call {
                0 => Some(Duration::from_millis(10)),
                1..=7 => Some(Duration::from_millis(1200)),
                8..=13 => Some(Duration::from_millis(4800)),
                14..=19 => Some(Duration::from_millis(9600)),
                20..=25 => Some(Duration::from_millis(19200)),
                26..=29 => Some(Duration::from_millis(25) / 1000),
                30..=34 => Some(Duration::from_nanos(500)),
                // A sustained failure tail drives the rendered loss towards
                // a full `1` and the dead-interval mean.
                _ => None,
            }
        }
    }
    impl ProbeRtt for ScriptedChain {
        fn probe_kind(&self) -> &'static str {
            // A unique kind so the process-global capture can be filtered to
            // this test's own prober events (other tests run concurrently
            // and log to the same subscriber).
            "scripted-chain"
        }
        fn probe_rtt(
            &self,
            _chain: &RouteChain,
        ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
            let call = self.sample();
            Box::pin(async move {
                ProbeOutcome {
                    rtt: call.ok_or_else(|| crate::error::AnyError::from("scripted probe failure")),
                    epilog: None,
                }
            })
        }
    }

    fn two_hop_chain() -> Arc<RouteChain> {
        use crate::route::HopConfig;
        let hop = |addr: &str, key: u8| HopConfig {
            name: None,
            address: addr.parse().unwrap(),
            header_crypto: tokio_chacha20::config::Config::new([key; 32].into()),
            payload_crypto: None,
        };
        Arc::from([hop("tcp://10.0.0.1:9000", 1), hop("tcp://10.0.0.2:9001", 2)])
    }

    /// The prober's recycle/reoptimize decision record, rendered through the
    /// logging path: the first degradation fires while the injected clock is
    /// still at its start, so the 600s `RecyclePacer` must suppress it; the
    /// second fires after the clock crossed 600s and must recycle through the
    /// *default* no-op implementation; the periodic probe log renders RTT
    /// samples in every human unit and both loss states.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn degraded_chain_recycles_and_suppresses_under_log_rendering() {
        use crate::clock::test_support::ManualClock;

        let logs = capture_logs();
        let clock = std::sync::Arc::new(ManualClock::new());
        let tracer = std::sync::Arc::new(ScriptedChain {
            calls: AtomicUsize::new(0),
            clock: std::sync::Arc::clone(&clock),
        });
        let chain = two_hop_chain();
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let cancellation = CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(probe_task(
            std::sync::Arc::clone(&tracer) as Arc<dyn ProbeRtt + Send + Sync>,
            chain,
            rtt_stats,
            loss,
            std::sync::Arc::clone(&clock) as Arc<dyn Clock>,
            cancellation.clone(),
        ));
        // Each round sleeps at most 60s, so a 61s advance runs at least one
        // round; the script needs 35 rounds plus a ~25-round failure tail to
        // push the rendered loss from its recovery value to a full `1`.
        for _ in 0..70 {
            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
        while let Some(res) = tasks.join_next().await {
            res.unwrap();
        }

        let logs = scripted_lines(&logs);
        assert_eq!(
            occurrences(&logs, "Chain RTT degraded; recycling first-hop session"),
            1,
            "the post-advance degradation must be the only one allowed to \
             recycle:\n{logs}"
        );
        assert_eq!(
            occurrences(
                &logs,
                "recycle suppressed (min interval), accepting as new baseline"
            ),
            2,
            "the pre-advance degradation and the post-recycle re-degradation \
             must both be suppressed by the pacer:\n{logs}"
        );
        assert_eq!(
            occurrences(&logs, "Timer reoptimize: reoptimizing first-hop relay"),
            1,
            "reoptimize is polled every round but its own pacer allows exactly \
             once, on the clock crossing:\n{logs}"
        );
        // The periodic probe log must render each RTT sample in its own unit.
        for (needle, what) in [
            ("sample=1.2s", "second unit"),
            ("sample=25µs", "microsecond unit"),
            ("sample=500ns", "nanosecond unit"),
        ] {
            assert!(
                logs.contains(needle),
                "the probe log must render the {what} sample: missing `{needle}` in:\n{logs}"
            );
        }
        assert!(
            logs.contains("srtt=") && logs.contains("ms"),
            "the degraded/suppressed logs render the smoothed srtt in a \
             millisecond value:\n{logs}"
        );
        // The loss store renders 0 after a clean run and 1 after the
        // sustained failure tail.
        assert!(
            logs.contains("loss=0"),
            "clean rounds render zero loss:\n{logs}"
        );
        assert!(
            logs.contains("loss=1"),
            "the failure tail renders full loss:\n{logs}"
        );
    }

    /// A probe that never resolves must be reaped by the round's 5s timeout
    /// as a failure: the timeout arm produces no sample, which drives the
    /// loss store to 1.0 and keeps the loop probing.
    #[tokio::test(start_paused = true)]
    async fn a_probe_that_hangs_times_out_into_a_failure() {
        struct HangingTracer;
        impl ProbeRtt for HangingTracer {
            fn probe_rtt(
                &self,
                _chain: &RouteChain,
            ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
                Box::pin(std::future::pending())
            }
        }

        let chain: Arc<RouteChain> = Arc::from(Vec::<crate::route::HopConfig>::new());
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let cancellation = CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(probe_task(
            Arc::new(HangingTracer),
            chain,
            rtt_stats,
            Arc::clone(&loss),
            Arc::new(crate::clock::SystemClock),
            cancellation.clone(),
        ));
        // Three rounds; each round's 5s probe timeout fires within a 6s
        // advance, so every round is a timed-out failure.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(6)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *loss.read().unwrap(),
            Some(1.0),
            "every timed-out probe must count as a failure in the loss store"
        );
        cancellation.cancel();
        while let Some(res) = tasks.join_next().await {
            res.unwrap();
        }
    }

    /// A panicking teardown epilog must be reaped by the round's
    /// `try_join_next` loop and its panic re-raised through the `.unwrap()`:
    /// if the reap or the unwrap were removed, the panic would be swallowed
    /// and `probe_task` would keep probing instead of surfacing it.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_epilog_propagates_through_the_reap() {
        struct PanicEpilogTracer {
            calls: AtomicUsize,
        }
        impl ProbeRtt for PanicEpilogTracer {
            fn probe_rtt(
                &self,
                _chain: &RouteChain,
            ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
                let index = self.calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    ProbeOutcome {
                        rtt: Ok(Duration::from_millis(10)),
                        epilog: if index == 0 {
                            Some(Box::pin(async {
                                panic!("scripted epilog panic");
                            }))
                        } else {
                            None
                        },
                    }
                })
            }
        }

        let chain: Arc<RouteChain> = Arc::from(Vec::<crate::route::HopConfig>::new());
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let cancellation = CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(probe_task(
            Arc::new(PanicEpilogTracer {
                calls: AtomicUsize::new(0),
            }),
            chain,
            rtt_stats,
            loss,
            Arc::new(crate::clock::SystemClock),
            cancellation,
        ));
        let mut outcome = None;
        // Under paused time the sleep between rounds resolves only when the
        // executor idles, so advance a little and then reap the round-2
        // panic.
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::task::yield_now().await;
            if let Some(res) = tasks.try_join_next() {
                outcome = Some(res);
                break;
            }
        }
        let outcome = outcome.expect("probe_task must terminate after the panicking epilog");
        assert!(
            outcome.is_err(),
            "the epilog panic must re-raise through the reap unwrap, not be \
             swallowed: {outcome:?}"
        );
    }

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
            _chain: &RouteChain,
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
        use crate::route::HopConfig;

        let calls = Arc::new(AtomicUsize::new(0));
        let chain: Arc<RouteChain> = Arc::from(Vec::<HopConfig>::new());
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
            Arc::new(crate::clock::SystemClock),
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
        use crate::route::HopConfig;

        struct EpilogTracer {
            started: Arc<tokio::sync::Notify>,
        }
        impl ProbeRtt for EpilogTracer {
            fn probe_rtt(
                &self,
                _chain: &RouteChain,
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
        let chain: Arc<RouteChain> = Arc::from(Vec::<HopConfig>::new());
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
            Arc::new(crate::clock::SystemClock),
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

    /// A tracer that scripts a two-step RTT regression and crosses the
    /// pacer interval on the injected clock. The first degradation fires
    /// while the injected clock is still at its start, so the 600s
    /// `RecyclePacer` must suppress it; the second fires after the clock
    /// has been advanced by exactly 600s, so it must be allowed through.
    /// `reoptimize` is polled every round, so it too is gated by its own
    /// `RecyclePacer`.
    struct PacingTracer {
        clock: std::sync::Arc<crate::clock::test_support::ManualClock>,
        calls: AtomicUsize,
        recycles: AtomicUsize,
        reoptimizes: AtomicUsize,
        recycle_instants: std::sync::Mutex<Vec<std::time::Instant>>,
    }
    impl ProbeRtt for PacingTracer {
        fn probe_rtt(
            &self,
            _chain: &RouteChain,
        ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            // Call 0 seeds the degradation baseline. Calls 1..=5 hold the
            // 3x jump long enough to fire the first degrade; calls 6..
            // hold a second, larger jump that fires again. Crossing the
            // pacer interval only from call 6 leaves the first degrade
            // suppressed.
            let rtt = match call {
                0 => Duration::from_millis(10),
                1..=5 => Duration::from_millis(50),
                _ => Duration::from_millis(200),
            };
            if call == 6 {
                self.clock.advance(Duration::from_secs(600));
            }
            Box::pin(async move {
                ProbeOutcome {
                    rtt: Ok(rtt),
                    epilog: None,
                }
            })
        }
        fn recycle(&self, _chain: &RouteChain) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            self.recycles.fetch_add(1, Ordering::SeqCst);
            self.recycle_instants.lock().unwrap().push(self.clock.now());
            Box::pin(async {})
        }
        fn reoptimize(&self, _chain: &RouteChain) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            self.reoptimizes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    /// The prober's recycle/reoptimize pacing must read the injected clock:
    /// the first degradation (before the clock advances) is suppressed by
    /// the 600s `RecyclePacer`, the second (exactly 600s later) is allowed,
    /// and `reoptimize` — polled every round — is allowed on the same
    /// crossing. No real 600s wait: a `ManualClock` is advanced by the
    /// scripted tracer between probe rounds.
    #[tokio::test(start_paused = true)]
    async fn probe_task_paces_recycle_and_reoptimize_on_the_injected_clock() {
        use crate::clock::test_support::ManualClock;
        use crate::route::HopConfig;

        let clock = std::sync::Arc::new(ManualClock::new());
        let tracer = std::sync::Arc::new(PacingTracer {
            clock: std::sync::Arc::clone(&clock),
            calls: AtomicUsize::new(0),
            recycles: AtomicUsize::new(0),
            reoptimizes: AtomicUsize::new(0),
            recycle_instants: std::sync::Mutex::new(Vec::new()),
        });
        let chain: Arc<RouteChain> = Arc::from(Vec::<HopConfig>::new());
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let cancellation = CancellationToken::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(probe_task(
            std::sync::Arc::clone(&tracer) as Arc<dyn ProbeRtt + Send + Sync>,
            chain,
            rtt_stats,
            loss,
            std::sync::Arc::clone(&clock) as Arc<dyn Clock>,
            cancellation.clone(),
        ));
        // Each round sleeps a Poisson interval of at most 60s, so advancing
        // virtual time by 61s runs at least one further round; the scripted
        // regressions complete within the first eleven rounds.
        for _ in 0..40 {
            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
        while let Some(res) = tasks.join_next().await {
            res.unwrap();
        }
        assert_eq!(
            tracer.recycles.load(Ordering::SeqCst),
            1,
            "the first degradation is inside the 600s pacer window and must be \
             suppressed; only the post-interval one may recycle"
        );
        assert_eq!(
            tracer.reoptimizes.load(Ordering::SeqCst),
            1,
            "reoptimize is polled every round but gated by its own 600s pacer; \
             it must fire exactly once, on the clock crossing"
        );
        let instants = tracer.recycle_instants.lock().unwrap();
        assert_eq!(
            instants.len(),
            1,
            "exactly one recycle must have been recorded"
        );
        assert_eq!(
            instants[0].duration_since(clock.now()),
            Duration::ZERO,
            "the recycle must occur at the advanced clock instant, not at wall time"
        );
    }
}
