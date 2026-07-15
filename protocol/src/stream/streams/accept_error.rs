use std::{io, net::SocketAddr, time::Instant};

const INITIAL_BACKOFF_MS: u64 = 25;
const MAX_BACKOFF_MS: u64 = 1000;
const WARN_AFTER_CONSECUTIVE: u64 = 3;

fn is_fatal(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::NotConnected
            | io::ErrorKind::Unsupported
    )
}

#[derive(Debug, Default)]
pub(crate) struct AcceptErrorBackoff {
    error_count: u64,
    first_error: Option<String>,
    last_error: Option<String>,
    started_at: Option<Instant>,
    logged: bool,
    retry_at: Option<Instant>,
}

impl AcceptErrorBackoff {
    pub(crate) fn failed(
        &mut self,
        listener: &'static str,
        addr: SocketAddr,
        error: io::Error,
    ) -> io::Result<()> {
        let now = Instant::now();
        let fatal = is_fatal(error.kind());
        let error_msg = format!("{error}");
        self.record(error_msg);
        if !fatal {
            let wait_ms = INITIAL_BACKOFF_MS * 2u64.saturating_pow((self.error_count - 1) as u32);
            let wait_ms = wait_ms.min(MAX_BACKOFF_MS);
            self.retry_at = Some(now + std::time::Duration::from_millis(wait_ms));
        } else {
            self.retry_at = None;
        }
        self.maybe_log(listener, addr, fatal);
        if fatal { Err(error) } else { Ok(()) }
    }

    pub(crate) fn failed_dispatching(
        &mut self,
        listener: &'static str,
        addr: SocketAddr,
        error: io::Error,
    ) -> io::Result<()> {
        let fatal = is_fatal(error.kind());
        let error_msg = format!("{error}");
        self.record(error_msg);
        self.retry_at = None;
        self.maybe_log(listener, addr, fatal);
        if fatal { Err(error) } else { Ok(()) }
    }

    fn record(&mut self, error_msg: String) {
        let now = Instant::now();
        self.error_count += 1;
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if self.first_error.is_none() {
            self.first_error = Some(error_msg.clone());
        }
        self.last_error = Some(error_msg);
    }

    fn maybe_log(&mut self, listener: &str, addr: SocketAddr, fatal: bool) {
        if self.logged {
            return;
        }
        if fatal || self.error_count >= WARN_AFTER_CONSECUTIVE {
            self.logged = true;
            let elapsed = self
                .started_at
                .map(|start| Instant::now().duration_since(start))
                .unwrap_or_default();
            tracing::warn!(
                error_count = self.error_count,
                first_error = %self.first_error.as_deref().unwrap_or("?"),
                last_error = %self.last_error.as_deref().unwrap_or("?"),
                elapsed_ms = elapsed.as_millis(),
                fatal,
                listener,
                %addr,
                "Listener accept errors"
            );
        }
    }

    pub(crate) fn retry_delay(&self) -> Option<std::time::Duration> {
        self.retry_at.map(|retry_at| {
            let now = Instant::now();
            if retry_at > now {
                retry_at.duration_since(now)
            } else {
                std::time::Duration::ZERO
            }
        })
    }

    #[cfg(test)]
    pub(crate) fn retry_at(&self) -> Option<Instant> {
        self.retry_at
    }

    pub(crate) fn accepted(&mut self, listener: &str, addr: SocketAddr) {
        if self.error_count > 0 && !self.logged {
            let elapsed = self
                .started_at
                .map(|start| Instant::now().duration_since(start))
                .unwrap_or_default();
            tracing::warn!(
                error_count = self.error_count,
                first_error = %self.first_error.as_deref().unwrap_or("?"),
                last_error = %self.last_error.as_deref().unwrap_or("?"),
                elapsed_ms = elapsed.as_millis(),
                listener,
                %addr,
                "Listener accept recovered after error streak"
            );
        }
        self.error_count = 0;
        self.started_at = None;
        self.first_error = None;
        self.last_error = None;
        self.logged = false;
        self.retry_at = None;
    }
}

pub(crate) async fn accept_after_retry<T, F, Fut>(
    delay: Option<std::time::Duration>,
    accept: F,
) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    accept().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatching_listener_records_error_without_pausing() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        let err = io::Error::new(io::ErrorKind::WouldBlock, "test");
        let _ = backoff.failed_dispatching("rtp_test", addr, err);
        assert!(backoff.first_error.is_some(), "error must be recorded");
        assert!(
            backoff.retry_at().is_none(),
            "retry_at must not be set on failed_dispatching"
        );
        assert_eq!(backoff.error_count, 1);
    }

    #[test]
    fn ordinary_listener_sets_exponential_backoff() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        let err = io::Error::new(io::ErrorKind::WouldBlock, "test");

        let now = Instant::now();
        let _ = backoff.failed("tcp", addr, err);
        assert_eq!(backoff.error_count, 1);
        assert!(backoff.retry_at().is_some());
        let delay = backoff.retry_at().unwrap().duration_since(now);
        assert!(delay.as_millis() >= 25 && delay.as_millis() <= 30);
    }

    #[test]
    fn backoff_caps_at_one_second() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        for _ in 0..10 {
            let err = io::Error::new(io::ErrorKind::WouldBlock, "test");
            let _ = backoff.failed("tcp", addr, err);
        }
        let now = Instant::now();
        let delay = backoff.retry_at().unwrap().duration_since(now);
        assert!(delay.as_millis() <= 1000);
    }

    #[test]
    fn fatal_error_returns_immediately() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        let err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let result = backoff.failed("tcp", addr, err);
        assert!(result.is_err());
    }

    #[test]
    fn fatal_error_logs_on_first_occurrence() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        let err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let _ = backoff.failed("tcp", addr, err);
        assert!(backoff.logged);
    }

    #[test]
    fn non_fatal_error_does_not_log_until_third() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        let _ = backoff.failed(
            "tcp",
            addr,
            io::Error::new(io::ErrorKind::WouldBlock, "test"),
        );
        assert!(!backoff.logged);
        let _ = backoff.failed(
            "tcp",
            addr,
            io::Error::new(io::ErrorKind::WouldBlock, "test"),
        );
        assert!(!backoff.logged);
        let _ = backoff.failed(
            "tcp",
            addr,
            io::Error::new(io::ErrorKind::WouldBlock, "test"),
        );
        assert!(backoff.logged);
    }

    #[test]
    fn accepted_clears_state_after_logged_error_streak() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        for _ in 0..3 {
            let err = io::Error::new(io::ErrorKind::WouldBlock, "test");
            let _ = backoff.failed("tcp", addr, err);
        }
        assert!(backoff.logged);
        backoff.accepted("tcp", addr);
        assert_eq!(backoff.error_count, 0);
        assert!(backoff.first_error.is_none());
        assert!(!backoff.logged);
    }

    #[test]
    fn accepted_clears_state_when_streak_not_yet_logged() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        let _ = backoff.failed(
            "tcp",
            addr,
            io::Error::new(io::ErrorKind::WouldBlock, "test"),
        );
        assert!(!backoff.logged);
        backoff.accepted("tcp", addr);
        assert_eq!(backoff.error_count, 0);
        assert!(!backoff.logged);
    }

    #[tokio::test]
    async fn accept_after_retry_does_not_poll_accept_before_deadline() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let polled = Arc::new(AtomicBool::new(false));
        let future = accept_after_retry(Some(std::time::Duration::from_secs(60)), {
            let polled = Arc::clone(&polled);
            move || {
                std::future::poll_fn(move |_| {
                    polled.store(true, Ordering::SeqCst);
                    std::task::Poll::Ready(())
                })
            }
        });
        tokio::pin!(future);
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert!(!polled.load(Ordering::SeqCst));
    }
}
