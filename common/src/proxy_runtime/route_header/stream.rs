use std::{fmt, net::SocketAddr};

use ae::anti_replay::{ReplayValidator, ValidatorRef};
use metrics::counter;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::{
    header::{
        codec::{CodecError, timed_read_header_async, timed_write_header_async},
        preamble::{self, PreambleError},
        route::RouteResponse,
    },
    proxy_runtime::{addr::RouteAddr, header::StreamRequestHeader},
    stream_runtime::IoConnection,
};

pub async fn read_route_header<Downstream>(
    downstream: &mut Downstream,
    crypto: &tokio_chacha20::config::Config,
    replay_validator: &ReplayValidator,
) -> Result<Option<RouteAddr>, SteerError>
where
    Downstream: IoConnection + std::fmt::Debug,
{
    let validator = ValidatorRef::Replay(replay_validator);
    // Wait for heartbeat upgrade
    preamble::wait_upgrade(downstream, crate::STREAM_IO_TIMEOUT, crypto, &validator)
        .await
        .map_err(|e| {
            let downstream_addr = downstream.peer_addr().ok();
            SteerError::ReadHeartbeatUpgrade {
                source: e,
                downstream_addr,
            }
        })?;

    // Decode header
    let header: StreamRequestHeader = timed_read_header_async(
        downstream,
        *crypto.key(),
        &validator,
        crate::STREAM_IO_TIMEOUT,
    )
    .await
    .map_err(|e| {
        let downstream_addr = downstream.peer_addr().ok();
        SteerError::ReadStreamRequestHeader {
            source: e,
            downstream_addr,
        }
    })?;

    // Echo
    let addr = match header.upstream {
        Some(upstream) => upstream,
        None => {
            let resp = RouteResponse { result: Ok(()) };
            timed_write_header_async(downstream, &resp, *crypto.key(), crate::STREAM_IO_TIMEOUT)
                .await
                .map_err(|e| {
                    let downstream_addr = downstream.peer_addr().ok();
                    SteerError::WriteEchoResponse {
                        source: e,
                        downstream_addr,
                    }
                })?;
            let _ = tokio::time::timeout(crate::STREAM_IO_TIMEOUT, downstream.flush()).await;

            counter!("stream.echoes").increment(1);
            return Ok(None);
        }
    };
    Ok(Some(addr))
}
#[derive(Debug, Error)]
pub enum SteerError {
    ReadHeartbeatUpgrade {
        #[source]
        source: PreambleError,
        downstream_addr: Option<SocketAddr>,
    },
    ReadStreamRequestHeader {
        #[source]
        source: CodecError,
        downstream_addr: Option<SocketAddr>,
    },
    WriteEchoResponse {
        #[source]
        source: CodecError,
        downstream_addr: Option<SocketAddr>,
    },
}
impl fmt::Display for SteerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadHeartbeatUpgrade {
                source,
                downstream_addr,
            } => {
                write!(
                    f,
                    "Failed to read heartbeat header from downstream: {source}"
                )?;
                write_downstream_addr(f, downstream_addr)
            }
            Self::ReadStreamRequestHeader {
                source,
                downstream_addr,
            } => {
                write!(
                    f,
                    "Failed to read stream request header from downstream: {source}"
                )?;
                write_downstream_addr(f, downstream_addr)
            }
            Self::WriteEchoResponse {
                source,
                downstream_addr,
            } => {
                write!(f, "Failed to write echo response to downstream: {source}")?;
                write_downstream_addr(f, downstream_addr)
            }
        }
    }
}
fn write_downstream_addr(f: &mut fmt::Formatter<'_>, addr: &Option<SocketAddr>) -> fmt::Result {
    if let Some(addr) = addr {
        write!(f, ", {addr}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_runtime::{HasIoAddr, IoConnection, OwnedIoStream};
    use std::{
        io,
        net::SocketAddr,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// A downstream that is already at EOF: `wait_upgrade` cannot read a
    /// preamble and must report a heartbeat-read failure.
    #[derive(Debug)]
    struct EofConn;
    impl AsyncRead for EofConn {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for EofConn {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl HasIoAddr for EofConn {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok("10.0.0.1:5000".parse().unwrap())
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("10.0.0.2:6000".parse().unwrap())
        }
    }
    impl OwnedIoStream for EofConn {}
    impl IoConnection for EofConn {}

    fn crypto() -> tokio_chacha20::config::Config {
        tokio_chacha20::config::Config::new([0x11; 32].into())
    }

    /// A downstream that closes before the upgrade preamble must be reported
    /// as a heartbeat-read failure that names the downstream address.
    #[tokio::test]
    async fn a_downstream_that_closes_before_the_upgrade_reports_a_heartbeat_error() {
        let mut downstream = EofConn;
        let validator = ReplayValidator::new(
            crate::anti_replay::VALIDATOR_TIME_FRAME,
            crate::anti_replay::VALIDATOR_CAPACITY,
        );
        let err = read_route_header(&mut downstream, &crypto(), &validator)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SteerError::ReadHeartbeatUpgrade { .. }),
            "expected ReadHeartbeatUpgrade, got {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("Failed to read heartbeat header from downstream: "),
            "{rendered}"
        );
        assert!(rendered.ends_with(", 10.0.0.1:5000"), "{rendered}");
    }

    /// Every `SteerError` variant renders its own message and appends the
    /// downstream address only when one is known.
    #[test]
    fn every_steer_error_variant_renders_its_documented_message() {
        let addr: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let cases = [
            (
                SteerError::ReadHeartbeatUpgrade {
                    source: PreambleError::Timeout(std::time::Duration::from_secs(1)),
                    downstream_addr: Some(addr),
                },
                "Failed to read heartbeat header from downstream: Timeout: 1s, 10.0.0.1:5000",
            ),
            (
                SteerError::ReadStreamRequestHeader {
                    source: CodecError::Io(io::Error::new(io::ErrorKind::InvalidData, "bad")),
                    downstream_addr: None,
                },
                "Failed to read stream request header from downstream: IO error: bad",
            ),
            (
                SteerError::WriteEchoResponse {
                    source: CodecError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "gone")),
                    downstream_addr: Some(addr),
                },
                "Failed to write echo response to downstream: IO error: gone, 10.0.0.1:5000",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }
}
