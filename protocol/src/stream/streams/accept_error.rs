use std::{io, net::SocketAddr, time::Instant};

#[derive(Debug, Default)]
pub(crate) struct AcceptErrorBackoff {
    consecutive: u32,
    started_at: Option<Instant>,
    first_error: Option<String>,
    last_error: Option<String>,
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
        self.failed_inner(listener, addr, error, true)
    }

    pub(crate) fn failed_dispatching(
        &mut self,
        listener: &'static str,
        addr: SocketAddr,
        error: io::Error,
    ) -> io::Result<()> {
        self.failed_inner(listener, addr, error, false)
    }

    fn failed_inner(
        &mut self,
        listener: &'static str,
        addr: SocketAddr,
        error: io::Error,
        set_retry: bool,
    ) -> io::Result<()> {
        let now = Instant::now();
        self.consecutive += 1;
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        if self.first_error.is_none() {
            self.first_error = Some(format!("{error}"));
        }
        self.last_error = Some(format!("{error}"));
        if set_retry {
            let wait = std::time::Duration::from_millis(100)
                .saturating_mul(self.consecutive.min(50) as u32);
            self.retry_at = Some(now + wait);
        }
        if !self.logged {
            self.logged = true;
            tracing::warn!(
                consecutive = self.consecutive,
                listener,
                %addr,
                error = %self.first_error.as_ref().unwrap(),
                "RTP listener accept error; backoff active"
            );
        }
        Ok(())
    }

    pub(crate) fn retry_at(&self) -> Option<Instant> {
        self.retry_at
    }

    pub(crate) fn reset(&mut self) {
        self.consecutive = 0;
        self.started_at = None;
        self.first_error = None;
        self.last_error = None;
        self.logged = false;
        self.retry_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatching_listener_records_error_without_pausing() {
        let mut backoff = AcceptErrorBackoff::default();
        let addr = "127.0.0.1:1234".parse().unwrap();
        let err = io::Error::new(io::ErrorKind::WouldBlock, "test");
        backoff.failed_dispatching("rtp_test", addr, err);
        assert!(backoff.first_error.is_some(), "error must be recorded");
        assert!(
            backoff.retry_at().is_none(),
            "retry_at must not be set on failed_dispatching"
        );
        assert_eq!(backoff.consecutive, 1);
    }
}
