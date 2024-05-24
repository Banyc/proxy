use std::{
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

use async_speed_limit::Limiter;
use metrics::{counter, gauge};
use monitor_table::table::RowOwnedGuard;
use scopeguard::defer;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_throughput::{ReadGauge, WriteGauge};
use tracing::info;

use super::{
    addr::{StreamAddr, StreamType},
    log::{StreamLog, StreamProxyLog, StreamRecord},
    metrics::{Session, StreamSessionTable},
};

pub mod tokio_io;

pub const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

pub struct CopyBidirectional<DS, US, ST> {
    pub downstream: DS,
    pub upstream: US,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
    pub speed_limiter: Limiter,
    pub metrics_context: LogContext<ST>,
}

impl<DS, US, ST> CopyBidirectional<DS, US, ST>
where
    US: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    DS: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    ST: StreamType,
{
    pub async fn serve_as_proxy_server(
        self,
        log_prefix: &str,
    ) -> Result<(), tokio_io::CopyBiErrorKind> {
        let session = self.session();
        let destination = self.metrics_context.destination.clone();

        let (metrics, res) = self
            .serve(session, EncryptionDirection::Decrypt, log_prefix)
            .await;
        log(metrics, destination);

        res
    }

    pub async fn serve_as_access_server(
        self,
        log_prefix: &str,
    ) -> Result<(), tokio_io::CopyBiErrorKind> {
        let session = self.session();
        let destination = self.metrics_context.destination.clone();

        let (metrics, res) = self
            .serve(session, EncryptionDirection::Encrypt, log_prefix)
            .await;
        log(metrics, destination);

        res
    }

    fn session(&self) -> Option<(RowOwnedGuard<Session<ST>>, ReadGauge, WriteGauge)> {
        self.metrics_context.session_table.as_ref().map(|s| {
            let (up_handle, up) = tokio_throughput::gauge();
            let (dn_handle, dn) = tokio_throughput::gauge();
            let r = ReadGauge(up);
            let w = WriteGauge(dn);

            let session = Session {
                start: SystemTime::now(),
                end: None,
                destination: self.metrics_context.destination.clone(),
                upstream_local: self.metrics_context.upstream_local,
                upstream_remote: self.metrics_context.upstream_addr.clone(),
                downstream_remote: self.metrics_context.downstream_addr,
                up_gauge: Some(Mutex::new(up_handle)),
                dn_gauge: Some(Mutex::new(dn_handle)),
            };
            let session = s.set_scope_owned(session);
            (session, r, w)
        })
    }

    async fn serve(
        self,
        session: Option<(
            monitor_table::table::RowOwnedGuard<Session<ST>>,
            ReadGauge,
            WriteGauge,
        )>,
        en_dir: EncryptionDirection,
        log_prefix: &str,
    ) -> (StreamLog<ST>, Result<(), tokio_io::CopyBiErrorKind>) {
        let res = match session {
            Some((session, r, w)) => {
                let downstream = tokio_throughput::WholeStream::new(self.downstream, r, w);
                let res = copy_bidirectional_with_payload_crypto(
                    downstream,
                    self.upstream,
                    self.payload_crypto.as_ref(),
                    self.speed_limiter,
                    en_dir,
                )
                .await;

                session.inspect_mut(|session| {
                    session.end = Some(SystemTime::now());
                });
                tokio::spawn(async move {
                    let _session = session;
                    tokio::time::sleep(DEAD_SESSION_RETENTION_DURATION).await;
                });

                res
            }
            None => {
                copy_bidirectional_with_payload_crypto(
                    self.downstream,
                    self.upstream,
                    self.payload_crypto.as_ref(),
                    self.speed_limiter,
                    en_dir,
                )
                .await
            }
        };

        let (log, res) = get_log_from_copy_result(self.metrics_context, res);
        match &res {
            Ok(()) => {
                info!(%log, "{log_prefix}: I/O copy finished");
            }
            Err(e) => {
                info!(?e, %log, "{log_prefix}: I/O copy error");
            }
        }
        (log, res)
    }
}

pub struct LogContext<ST> {
    pub start: (Instant, SystemTime),
    pub upstream_addr: StreamAddr<ST>,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
    pub upstream_local: Option<SocketAddr>,
    pub session_table: Option<StreamSessionTable<ST>>,
    pub destination: Option<StreamAddr<ST>>,
}

fn log<ST: StreamType>(metrics: StreamLog<ST>, destination: Option<StreamAddr<ST>>) {
    match destination {
        Some(d) => {
            let metrics = StreamProxyLog {
                stream: metrics,
                destination: d.address,
            };
            table_log::log!(&StreamRecord::ProxyLog(&metrics));
        }
        None => table_log::log!(&StreamRecord::Log(&metrics)),
    }
}

pub enum EncryptionDirection {
    Encrypt,
    Decrypt,
}

pub async fn copy_bidirectional_with_payload_crypto<DS, US>(
    downstream: DS,
    upstream: US,
    payload_crypto: Option<&tokio_chacha20::config::Config>,
    speed_limiter: Limiter,
    en_dir: EncryptionDirection,
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
            match en_dir {
                EncryptionDirection::Encrypt => {
                    // Establish encrypted stream
                    let (r, w) = tokio::io::split(upstream);
                    let upstream =
                        tokio_chacha20::stream::WholeStream::from_key_halves(*crypto.key(), r, w);

                    tokio_io::timed_copy_bidirectional(downstream, upstream, speed_limiter).await
                }
                EncryptionDirection::Decrypt => {
                    // Establish encrypted stream
                    let (r, w) = tokio::io::split(downstream);
                    let downstream =
                        tokio_chacha20::stream::WholeStream::from_key_halves(*crypto.key(), r, w);

                    tokio_io::timed_copy_bidirectional(downstream, upstream, speed_limiter).await
                }
            }
        }
        None => tokio_io::timed_copy_bidirectional(downstream, upstream, speed_limiter).await,
    }
}

fn get_log_from_copy_result<ST>(
    log_context: LogContext<ST>,
    result: tokio_io::TimedCopyBidirectionalResult,
) -> (StreamLog<ST>, Result<(), tokio_io::CopyBiErrorKind>) {
    let (bytes_uplink, bytes_downlink) = result.amounts;

    counter!("stream.bytes_uplink").increment(bytes_uplink);
    counter!("stream.bytes_downlink").increment(bytes_downlink);
    let metrics = StreamLog {
        start: log_context.start,
        end: result.end,
        bytes_uplink,
        bytes_downlink,
        upstream_addr: log_context.upstream_addr,
        upstream_sock_addr: log_context.upstream_sock_addr,
        downstream_addr: log_context.downstream_addr,
    };

    (metrics, result.io_result)
}
