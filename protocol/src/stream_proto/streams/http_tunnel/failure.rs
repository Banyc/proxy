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

    /// Counts every event the reporting thread emits, so the test below
    /// observes the emission itself rather than only the `reported` flag
    /// that the reporter sets before it logs. The flag alone cannot tell one
    /// emission from two.
    struct CountEvents(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl tracing::Subscriber for CountEvents {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// The reporter must emit exactly one failure event however many times it
    /// is called for the same request: the second call is not a new failure.
    #[test]
    fn failure_reporter_emits_exactly_one_event() {
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
        let events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subscriber = CountEvents(std::sync::Arc::clone(&events));
        let _guard = tracing::subscriber::set_default(subscriber);
        reporter.report(&TunnelError::HttpNoHost, None);
        reporter.report(&TunnelError::HttpNoPort, None);
        assert_eq!(
            events.load(Ordering::Relaxed),
            1,
            "the reporter must emit one failure event per request"
        );
    }
}
