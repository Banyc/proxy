use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use async_speed_limit::Limiter;
use metrics::{counter, gauge};
use monitor_table::table::RowOwnedGuard;
use scopeguard::defer;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_io::BytesCopied;
use tokio_throughput::{ReadGauge, WriteGauge};
use tracing::info;

use crate::{
    log::Timing,
    proto::{
        addr::StreamAddr,
        log::stream::{LOGGER, StreamLog, StreamProxyLog},
        metrics::stream::{Session, StreamSessionTable},
    },
};

pub mod tokio_io;

pub const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

pub struct CopyBidirectional<Downstream, Upstream> {
    pub downstream: Downstream,
    pub upstream: Upstream,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
    pub speed_limiter: Limiter,
    pub conn_context: ConnContext,
}

impl<Downstream, Upstream> CopyBidirectional<Downstream, Upstream>
where
    Upstream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    Downstream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub async fn serve_as_proxy_server(
        self,
        log_prefix: &str,
    ) -> Result<(), tokio_io::CopyBiError> {
        let session = self.session();
        let destination = self.conn_context.destination.clone();

        let (metrics, res) = self
            .serve(session, EncryptionDirection::Decrypt, log_prefix)
            .await;
        log(metrics, destination);

        res
    }

    pub async fn serve_as_access_server(
        self,
        log_prefix: &str,
    ) -> Result<(), tokio_io::CopyBiError> {
        let session = self.session();
        let destination = self.conn_context.destination.clone();

        let (metrics, res) = self
            .serve(session, EncryptionDirection::Encrypt, log_prefix)
            .await;
        log(metrics, destination);

        res
    }

    fn session(&self) -> Option<(RowOwnedGuard<Session>, ReadGauge, WriteGauge)> {
        self.conn_context.session_table.as_ref().map(|s| {
            let (up_handle, up) = tokio_throughput::gauge();
            let (dn_handle, dn) = tokio_throughput::gauge();
            let r = ReadGauge(up);
            let w = WriteGauge(dn);

            let session = Session {
                start: SystemTime::now(),
                end: None,
                destination: self.conn_context.destination.clone(),
                upstream_local: self.conn_context.upstream_local,
                upstream_remote: self.conn_context.upstream_remote.clone(),
                downstream_local: Arc::clone(&self.conn_context.downstream_local),
                downstream_remote: self.conn_context.downstream_remote,
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
            monitor_table::table::RowOwnedGuard<Session>,
            ReadGauge,
            WriteGauge,
        )>,
        en_dir: EncryptionDirection,
        log_prefix: &str,
    ) -> (StreamLog, Result<(), tokio_io::CopyBiError>) {
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

        let (log, res) = get_log_from_copy_result(self.conn_context, res);
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

pub struct ConnContext {
    pub start: (Instant, SystemTime),
    pub upstream_remote: StreamAddr,
    pub upstream_remote_sock: SocketAddr,
    pub downstream_remote: Option<SocketAddr>,
    pub downstream_local: Arc<str>,
    pub upstream_local: Option<SocketAddr>,
    pub session_table: Option<StreamSessionTable>,
    pub destination: Option<StreamAddr>,
}

fn log(log: StreamLog, destination: Option<StreamAddr>) {
    match destination {
        Some(d) => {
            let log = StreamProxyLog {
                stream: log,
                destination: d.address,
            };
            let record = (&log).into();
            if let Some(x) = LOGGER.lock().unwrap().as_ref() {
                x.write(&record);
            }
        }
        None => {
            let record = (&log).into();
            if let Some(x) = LOGGER.lock().unwrap().as_ref() {
                x.write(&record);
            }
        }
    }
}

pub enum EncryptionDirection {
    Encrypt,
    Decrypt,
}

pub async fn copy_bidirectional_with_payload_crypto<Downstream, Upstream>(
    downstream: Downstream,
    upstream: Upstream,
    payload_crypto: Option<&tokio_chacha20::config::Config>,
    speed_limiter: Limiter,
    en_dir: EncryptionDirection,
) -> tokio_io::TimedCopyBidirectionalResult
where
    Upstream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    Downstream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
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
                        tokio_chacha20::stream::WholeStream::from_key_halves_x(*crypto.key(), r, w);

                    tokio_io::timed_copy_bidirectional(downstream, upstream, speed_limiter).await
                }
                EncryptionDirection::Decrypt => {
                    // Establish encrypted stream
                    let (r, w) = tokio::io::split(downstream);
                    let downstream =
                        tokio_chacha20::stream::WholeStream::from_key_halves_x(*crypto.key(), r, w);

                    tokio_io::timed_copy_bidirectional(downstream, upstream, speed_limiter).await
                }
            }
        }
        None => tokio_io::timed_copy_bidirectional(downstream, upstream, speed_limiter).await,
    }
}

fn get_log_from_copy_result(
    conn_context: ConnContext,
    result: tokio_io::TimedCopyBidirectionalResult,
) -> (StreamLog, Result<(), tokio_io::CopyBiError>) {
    let BytesCopied {
        a_to_b: bytes_uplink,
        b_to_a: bytes_downlink,
    } = result.amounts;

    counter!("stream.up.bytes").increment(bytes_uplink);
    counter!("stream.dn.bytes").increment(bytes_downlink);
    let timing = Timing {
        start: conn_context.start,
        end: result.end,
    };
    let log = StreamLog {
        timing,
        bytes_uplink,
        bytes_downlink,
        upstream_addr: conn_context.upstream_remote,
        upstream_sock_addr: conn_context.upstream_remote_sock,
        downstream_addr: conn_context.downstream_remote,
    };

    (log, result.io_result)
}
