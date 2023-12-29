use std::{
    fmt,
    net::SocketAddr,
    time::{Duration, Instant, SystemTime},
};

use async_speed_limit::Limiter;
use metrics::{counter, gauge};
use scopeguard::defer;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::info;

use crate::{crypto::XorCrypto, stream::xor::XorStream};

use super::{
    addr::StreamAddr,
    session_table::{Session, StreamSessionTable},
    StreamMetrics, StreamProxyMetrics,
};

pub mod tokio_io;

pub const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

pub struct CopyBidirectional<DS, US, ST> {
    pub downstream: DS,
    pub upstream: US,
    pub payload_crypto: Option<XorCrypto>,
    pub speed_limiter: Limiter,
    pub start: Instant,
    pub upstream_addr: StreamAddr<ST>,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

impl<DS, US, ST> CopyBidirectional<DS, US, ST>
where
    US: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    DS: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    ST: Clone + fmt::Display + Sync + Send + 'static,
{
    pub async fn serve_as_proxy_server(
        self,
        log_prefix: &str,
    ) -> (StreamMetrics<ST>, Result<(), tokio_io::CopyBiErrorKind>) {
        let (metrics, res) = self.serve_as_proxy_server_no_logs().await;

        match &res {
            Ok(()) => {
                info!(%metrics, "{log_prefix}: I/O copy finished");
            }
            Err(e) => {
                info!(?e, %metrics, "{log_prefix}: I/O copy error");
            }
        }

        (metrics, res)
    }

    pub async fn serve_as_access_server(
        self,
        destination: StreamAddr<ST>,
        session_table: StreamSessionTable<ST>,
        upstream_local: Option<SocketAddr>,
        log_prefix: &str,
    ) -> (
        StreamProxyMetrics<ST>,
        Result<(), tokio_io::CopyBiErrorKind>,
    ) {
        let session_guard = session_table.set_scope_owned(Session {
            start: SystemTime::now(),
            end: None,
            destination: destination.clone(),
            upstream_local,
        });

        let (metrics, res) = self.serve_as_proxy_server_no_logs().await;

        let metrics = StreamProxyMetrics {
            stream: metrics,
            destination: destination.address,
        };

        match &res {
            Ok(()) => {
                info!(%metrics, "{log_prefix}: I/O copy finished");
            }
            Err(e) => {
                info!(?e, %metrics, "{log_prefix}: I/O copy error");
            }
        }

        session_guard.inspect_mut(|session| {
            session.end = Some(SystemTime::now());
        });
        tokio::spawn(async move {
            let _session_guard = session_guard;
            tokio::time::sleep(DEAD_SESSION_RETENTION_DURATION).await;
        });

        (metrics, res)
    }

    async fn serve_as_proxy_server_no_logs(
        self,
    ) -> (StreamMetrics<ST>, Result<(), tokio_io::CopyBiErrorKind>) {
        let res = copy_bidirectional_with_payload_crypto(
            self.downstream,
            self.upstream,
            self.payload_crypto.as_ref(),
            self.speed_limiter,
        )
        .await;

        let (metrics, res) = get_metrics_from_copy_result(
            self.start,
            self.upstream_addr,
            self.upstream_sock_addr,
            self.downstream_addr,
            res,
        );

        (metrics, res)
    }
}

pub async fn copy_bidirectional_with_payload_crypto<DS, US>(
    downstream: DS,
    upstream: US,
    payload_crypto: Option<&XorCrypto>,
    speed_limiter: Limiter,
) -> tokio_io::TimedCopyBidirectionalResult
where
    US: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    DS: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    counter!("stream.io_copies").increment(1);
    gauge!("stream.current_io_copies").increment(1.);
    defer!(gauge!("stream.current_io_copies").decrement(1.));
    match payload_crypto {
        Some(crypto) => {
            // Establish encrypted stream
            let xor_stream = XorStream::upgrade(upstream, crypto);
            tokio_io::timed_copy_bidirectional(downstream, xor_stream, speed_limiter).await
        }
        None => tokio_io::timed_copy_bidirectional(downstream, upstream, speed_limiter).await,
    }
}

pub fn get_metrics_from_copy_result<ST>(
    start: Instant,
    upstream_addr: StreamAddr<ST>,
    upstream_sock_addr: SocketAddr,
    downstream_addr: Option<SocketAddr>,
    result: tokio_io::TimedCopyBidirectionalResult,
) -> (StreamMetrics<ST>, Result<(), tokio_io::CopyBiErrorKind>) {
    let (bytes_uplink, bytes_downlink) = result.amounts;

    counter!("stream.bytes_uplink").increment(bytes_uplink);
    counter!("stream.bytes_downlink").increment(bytes_downlink);
    let metrics = StreamMetrics {
        start,
        end: result.end,
        bytes_uplink,
        bytes_downlink,
        upstream_addr,
        upstream_sock_addr,
        downstream_addr,
    };

    (metrics, result.io_result)
}
