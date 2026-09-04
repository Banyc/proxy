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
