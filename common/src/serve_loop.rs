use std::{future::Future, io, net::SocketAddr, sync::Arc, time::Duration};

use metrics::counter;
use thiserror::Error;
use tracing::{info, trace};

use crate::loading;

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
pub struct AcceptErrorBackoff {
    error_count: u64,
    first_error: Option<String>,
    last_error: Option<String>,
    started_at: Option<std::time::Instant>,
    logged: bool,
    retry_at: Option<std::time::Instant>,
}

impl AcceptErrorBackoff {
    pub fn failed(
        &mut self,
        listener: &'static str,
        addr: SocketAddr,
        error: io::Error,
    ) -> io::Result<()> {
        let now = std::time::Instant::now();
        let fatal = is_fatal(error.kind());
        let error_msg = format!("{error}");
        self.record(error_msg);
        if !fatal {
            let shifts = u32::try_from(self.error_count - 1).unwrap_or(u32::MAX);
            let wait_ms = INITIAL_BACKOFF_MS
                .saturating_mul(2u64.saturating_pow(shifts))
                .min(MAX_BACKOFF_MS);
            self.retry_at = Some(now + Duration::from_millis(wait_ms));
        } else {
            self.retry_at = None;
        }
        self.maybe_log(listener, addr, fatal);
        if fatal { Err(error) } else { Ok(()) }
    }

    pub fn failed_dispatching(
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
        let now = std::time::Instant::now();
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
                .map(|start| std::time::Instant::now().duration_since(start))
                .unwrap_or_default();
            if fatal {
                tracing::error!(
                    error_count = self.error_count,
                    first_error = %self.first_error.as_deref().unwrap_or("?"),
                    last_error = %self.last_error.as_deref().unwrap_or("?"),
                    elapsed_ms = elapsed.as_millis(),
                    fatal,
                    listener,
                    %addr,
                    "Listener accept errors"
                );
            } else {
                tracing::warn!(
                    error_count = self.error_count,
                    first_error = %self.first_error.as_deref().unwrap_or("?"),
                    last_error = %self.last_error.as_deref().unwrap_or("?"),
                    elapsed_ms = elapsed.as_millis(),
                    listener,
                    %addr,
                    "Listener accept errors"
                );
            }
        }
    }

    pub fn retry_delay(&self) -> Option<Duration> {
        self.retry_at.map(|retry_at| {
            let now = std::time::Instant::now();
            if retry_at > now {
                retry_at.duration_since(now)
            } else {
                Duration::ZERO
            }
        })
    }

    #[cfg(test)]
    pub fn retry_at(&self) -> Option<std::time::Instant> {
        self.retry_at
    }

    pub fn accepted(&mut self, listener: &str, addr: SocketAddr) {
        if self.error_count > 0 && !self.logged {
            let elapsed = self
                .started_at
                .map(|start| std::time::Instant::now().duration_since(start))
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

pub async fn accept_after_retry<T, F, Fut>(delay: Option<Duration>, accept: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    accept().await
}

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("Failed to get local address: {0}")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

pub async fn serve_loop<H, T, A, AF, W, S, X, E, XF>(
    label: &'static str,
    counter_name: Option<&'static str>,
    dispatching: bool,
    addr: SocketAddr,
    mut conn_handler: Arc<H>,
    mut set_conn_handler_rx: loading::ReplaceConnHandlerRx<H>,
    mut swap: S,
    mut accept: A,
    mut wrap: W,
    state: &mut X,
    mut extra: E,
) -> Result<(), ServeError>
where
    H: Send + Sync + 'static,
    A: FnMut() -> AF,
    AF: Future<Output = io::Result<T>> + Send,
    W: for<'a> FnMut(&'a mut X, T, Arc<H>),
    S: FnMut(Arc<H>),
    E: FnMut(&mut X) -> XF,
    XF: Future<Output = ()> + Send,
{
    info!(?addr, "Listening");
    let mut accept_backoff = AcceptErrorBackoff::default();
    loop {
        trace!("Waiting for connection");
        tokio::select! {
            res = accept_after_retry(accept_backoff.retry_delay(), || accept()) => {
                let stream = match res {
                    Ok(res) => {
                        accept_backoff.accepted(label, addr);
                        res
                    }
                    Err(e) => {
                        let result = if dispatching {
                            accept_backoff.failed_dispatching(label, addr, e)
                        } else {
                            accept_backoff.failed(label, addr, e)
                        };
                        if let Err(source) = result {
                            return Err(ServeError::Accept { source, addr });
                        }
                        if dispatching {
                            tokio::task::yield_now().await;
                        }
                        continue;
                    }
                };
                if let Some(counter_name) = counter_name {
                    counter!(counter_name).increment(1);
                }
                wrap(state, stream, Arc::clone(&conn_handler));
            }
            _ = extra(state) => {}
            res = set_conn_handler_rx.0.recv() => {
                let new_conn_handler = match res {
                    Some(new_conn_handler) => new_conn_handler,
                    None => break,
                };
                info!(?addr, "Connection handler set");
                conn_handler = Arc::new(new_conn_handler);
                swap(Arc::clone(&conn_handler));
            }
        }
    }
    Ok(())
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

        let now = std::time::Instant::now();
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
        let now = std::time::Instant::now();
        let delay = backoff.retry_at().unwrap().duration_since(now);
        assert!(delay.as_millis() <= 1000);
    }

    #[test]
    fn backoff_never_overflows_the_backoff() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        for _ in 0..200 {
            let err = io::Error::new(io::ErrorKind::WouldBlock, "test");
            let _ = backoff.failed("tcp", addr, err);
        }
        let now = std::time::Instant::now();
        let delay = backoff.retry_at().unwrap().duration_since(now);
        assert!(delay.as_millis() <= u128::from(MAX_BACKOFF_MS), "{delay:?}");
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
        let future = accept_after_retry(Some(Duration::from_secs(60)), {
            let polled = Arc::clone(&polled);
            move || {
                std::future::poll_fn(move |_| {
                    polled.store(true, Ordering::SeqCst);
                    std::task::Poll::Ready(())
                })
            }
        });
        tokio::pin!(future);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert!(!polled.load(Ordering::SeqCst));
    }
}
