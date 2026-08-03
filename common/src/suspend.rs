use std::time::{Duration, Instant, SystemTime};

use crate::notify::Notify;

const SUSPEND_CHECK_INTERVAL: Duration = Duration::from_millis(200);
const SUSPEND_TOLERATION_COEFFICIENT: f64 = 3.1;
fn suspend_toleration() -> Duration {
    SUSPEND_CHECK_INTERVAL.mul_f64(SUSPEND_TOLERATION_COEFFICIENT)
}

#[derive(Debug, Clone)]
pub struct Suspended(pub Notify);

pub fn spawn_check_system_suspend() -> Suspended {
    let suspended = Suspended(Notify::new());
    tokio::spawn({
        let suspended = suspended.clone();
        async move {
            let mut prev = (Instant::now(), SystemTime::now());
            loop {
                tokio::time::sleep(SUSPEND_CHECK_INTERVAL).await;
                let now = (Instant::now(), SystemTime::now());
                let prev = scopeguard::guard(&mut prev, |prev| *prev = now);
                let elapsed = now.0.duration_since(prev.0);
                if suspend_toleration() < elapsed {
                    suspended.0.notify_waiters();
                }
            }
        }
    });
    suspended
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn basics() {
        let suspended = spawn_check_system_suspend();
        let mut suspended = suspended.0.waiter();
        suspended.notified().await;
    }
}
