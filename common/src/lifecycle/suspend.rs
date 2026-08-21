use std::time::{Duration, Instant, SystemTime};

use crate::{lifecycle::process::RootTaskExit, notify::Notify};

const SUSPEND_CHECK_INTERVAL: Duration = Duration::from_millis(200);
/// The tolerance factor applied to the suspend check interval before a gap is
/// treated as a system suspend rather than a clock hiccup.
const SUSPEND_TOLERATION_COEFFICIENT: f64 = 3.1;
fn suspend_toleration() -> Duration {
    SUSPEND_CHECK_INTERVAL.mul_f64(SUSPEND_TOLERATION_COEFFICIENT)
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
                if suspend_toleration() < elapsed {
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

    #[tokio::test]
    #[ignore = "requires an actual system suspend to trigger the notification"]
    async fn basics() {
        let mut process_tasks = tokio::task::JoinSet::new();
        let system_suspend = spawn_suspend_watcher(&mut process_tasks);
        let mut system_suspend = system_suspend.0.subscription();
        system_suspend.notified().await;
    }
}
