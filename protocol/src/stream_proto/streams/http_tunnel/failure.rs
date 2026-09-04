use std::{
    net::SocketAddr,
    sync::OnceLock,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tracing::warn;

use super::{HttpRequestContext, TunnelError};

pub(crate) type RequestErrorContext = Arc<Mutex<Option<HttpFailureReporter>>>;

#[derive(Debug, Clone)]
pub(crate) struct HttpDownstreamContext {
    pub(crate) remote: Option<SocketAddr>,
    pub(crate) local: Option<SocketAddr>,
}

#[derive(Debug)]
pub(crate) struct HttpRequestFailure {
    pub(crate) request: HttpRequestContext,
    pub(crate) destination: OnceLock<String>,
    pub(crate) reported: AtomicBool,
}

#[derive(Debug, Clone)]
pub(crate) struct HttpFailureReporter {
    pub(crate) failure: Arc<HttpRequestFailure>,
    pub(crate) downstream: HttpDownstreamContext,
    pub(crate) listener: Arc<str>,
}

impl HttpFailureReporter {
    pub(crate) fn set_destination(&self, destination: impl Into<String>) {
        let _ = self.failure.destination.set(destination.into());
    }

    fn destination(&self) -> Option<String> {
        self.failure
            .destination
            .get()
            .cloned()
            .or_else(|| self.failure.request.authority.clone())
    }

    pub(crate) fn report(&self, error: &TunnelError, attempted_upstream: Option<&str>) {
        if self
            .failure
            .reported
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let request = &self.failure.request;
        let destination = self.destination();
        let up = attempted_upstream
            .map(str::to_owned)
            .or_else(|| error.upstream_addr().map(|addr| addr.to_string()));
        warn!(
            event = "http_tunnel_proxy_failed",
            error = %error,
            dn = ?common::OptLog(self.downstream.remote),
            dn_local = ?common::OptLog(self.downstream.local),
            listener = %self.listener,
            method = %request.method,
            uri = %request.uri,
            host = ?common::OptLog(request.host.as_deref()),
            destination = ?common::OptLog(destination),
            up = ?common::OptLog(up),
            "HTTP tunnel proxy failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::Method;

    #[test]
    fn failure_reporter_emits_only_once() {
        let failure = Arc::new(HttpRequestFailure {
            request: HttpRequestContext {
                method: Method::GET,
                uri: "/".parse().unwrap(),
                host: None,
                authority: None,
            },
            destination: OnceLock::new(),
            reported: AtomicBool::new(false),
        });
        let reporter = HttpFailureReporter {
            failure: Arc::clone(&failure),
            downstream: HttpDownstreamContext {
                remote: None,
                local: None,
            },
            listener: Arc::from("test"),
        };
        reporter.report(&TunnelError::HttpNoHost, None);
        assert!(failure.reported.load(Ordering::Relaxed));
        reporter.report(&TunnelError::HttpNoPort, None);
        assert!(failure.reported.load(Ordering::Relaxed));
    }
}
