use std::time::{Duration, Instant, SystemTime};

use crate::{lifecycle::process::RootTaskExit, notify::Notify};

const SUSPEND_CHECK_INTERVAL: Duration = Duration::from_millis(200);
/// The tolerance factor applied to the suspend check interval before a gap is
/// treated as a system suspend rather than a clock hiccup.
const SUSPEND_TOLERATION_COEFFICIENT: f64 = 3.1;
fn suspend_toleration() -> Duration {
    SUSPEND_CHECK_INTERVAL.mul_f64(SUSPEND_TOLERATION_COEFFICIENT)
}

/// Whether a monotonic gap between two checks is a system suspend rather than
/// ordinary scheduler jitter. The comparison is strict: a gap exactly at the
/// tolerance is jitter.
fn is_system_suspend(elapsed: Duration) -> bool {
    suspend_toleration() < elapsed
}

#[derive(Debug, Clone)]
pub struct SystemResumeSignal(pub Notify);

pub fn spawn_suspend_watcher(
    process_tasks: &mut tokio::task::JoinSet<RootTaskExit>,
) -> SystemResumeSignal {
    let system_suspend = SystemResumeSignal(Notify::new());
    process_tasks.spawn({
        let system_suspend = system_suspend.clone();
        async move {
            let mut prev = (Instant::now(), SystemTime::now());
            loop {
                tokio::time::sleep(SUSPEND_CHECK_INTERVAL).await;
                let now = (Instant::now(), SystemTime::now());
                let prev = scopeguard::guard(&mut prev, |prev| *prev = now);
                let elapsed = now.0.duration_since(prev.0);
                if is_system_suspend(elapsed) {
                    system_suspend.0.notify_waiters();
                }
            }
        }
    });
    system_suspend
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tolerance must exceed the check interval: ordinary scheduler
    /// jitter between two checks must not be mistaken for a system suspend,
    /// or every resume subscriber wakes on every tick.
    #[test]
    fn the_suspend_tolerance_exceeds_the_check_interval() {
        assert!(
            suspend_toleration() > SUSPEND_CHECK_INTERVAL,
            "a tolerance at or below the check interval false-triggers a suspend"
        );
    }

    /// The tolerance is the check interval scaled by the configured
    /// coefficient, and a gap must *exceed* it (strict) to count as a suspend.
    /// The exact threshold is unobservable with real time; the pure comparison
    /// makes it deterministic, and pinning the tolerance by value catches a
    /// retune that the earlier "exceeds the interval" check would accept.
    #[test]
    fn the_suspend_threshold_pins_the_coefficient_and_is_strict() {
        assert_eq!(
            suspend_toleration(),
            Duration::from_micros(620_000),
            "the tolerance is the 200ms check interval scaled by the configured coefficient"
        );
        assert!(
            !is_system_suspend(suspend_toleration()),
            "a gap exactly at the tolerance is jitter, not a suspend"
        );
        assert!(
            is_system_suspend(suspend_toleration() + Duration::from_nanos(1)),
            "a gap one nanosecond past the tolerance is a suspend"
        );
    }

    #[tokio::test]
    #[ignore = "requires an actual system suspend to trigger the notification"]
    async fn basics() {
        let mut process_tasks = tokio::task::JoinSet::new();
        let system_suspend = spawn_suspend_watcher(&mut process_tasks);
        let mut system_suspend = system_suspend.0.subscription();
        system_suspend.notified().await;
    }
}
