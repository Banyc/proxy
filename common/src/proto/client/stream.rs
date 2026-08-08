use std::time::{Duration, Instant};

use crate::{
    error::AnyError,
    header::{
        codec::{CodecError, timed_read_header_async, timed_write_header_async},
        preamble::{self, PreambleError},
        route::{RouteError, RouteResponse},
    },
    proto::{addr::RouteAddr, conn::stream::ConnAndAddr, context::StreamRuntime},
    route::{ConnChain, ConnConfig, ProbeRtt, convert_proxies_to_header_crypto_pairs},
    stream::pool::{ConnectError, connect_with_pool},
};
use ae::anti_replay::ValidatorRef;
use metrics::counter;
use thiserror::Error;
use tracing::{instrument, trace};

#[instrument(skip(proxies, stream_context))]
pub async fn establish(
    proxies: &ConnChain,
    destination: RouteAddr,
    stream_context: &StreamRuntime,
) -> Result<ConnAndAddr, StreamEstablishError> {
    if proxies.is_empty() {
        let (stream, sock_addr) =
            connect_with_pool(&destination, stream_context, true, crate::STREAM_IO_TIMEOUT)
                .await
                .map_err(|source| StreamEstablishError::ConnectDestination {
                    source,
                    upstream_addr: destination.clone(),
                })?;
        stream.set_stream_name(&destination.address.to_string());
        return Ok(ConnAndAddr {
            stream,
            addr: destination,
            sock_addr,
        });
    }

    let (mut stream, addr, sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) =
            connect_with_pool(proxy_addr, stream_context, true, crate::STREAM_IO_TIMEOUT)
                .await
                .map_err(|source| StreamEstablishError::ConnectFirstProxyServer {
                    source,
                    upstream_addr: proxy_addr.clone(),
                })?;
        (stream, proxy_addr.clone(), sock_addr)
    };

    stream.set_stream_name(&destination.address.to_string());

    let pairs = convert_proxies_to_header_crypto_pairs(proxies, Some(destination));

    for (header, crypto) in &pairs {
        trace!(?header, "Writing headers to stream");
        preamble::send_upgrade(&mut stream, crate::STREAM_IO_TIMEOUT, crypto)
            .await
            .map_err(|e| StreamEstablishError::WriteHeartbeatUpgrade {
                source: e,
                upstream_addr: addr.clone(),
            })?;
        timed_write_header_async(&mut stream, header, *crypto.key(), crate::STREAM_IO_TIMEOUT)
            .await
            .map_err(|e| StreamEstablishError::WriteStreamRequestHeader {
                source: e,
                upstream_addr: addr.clone(),
            })?;
    }

    Ok(ConnAndAddr {
        stream,
        addr,
        sock_addr,
    })
}

#[derive(Debug, Error)]
pub enum StreamEstablishError {
    #[error("Failed to connect to destination: {source}, {upstream_addr}")]
    ConnectDestination {
        #[source]
        source: ConnectError,
        upstream_addr: RouteAddr,
    },
    #[error("Failed to connect to first proxy server: {source}, {upstream_addr}")]
    ConnectFirstProxyServer {
        #[source]
        source: ConnectError,
        upstream_addr: RouteAddr,
    },
    #[error("Failed to write heartbeat upgrade to upstream: {source}, {upstream_addr}")]
    WriteHeartbeatUpgrade {
        #[source]
        source: PreambleError,
        upstream_addr: RouteAddr,
    },
    #[error("Failed to read stream request header to upstream: {source}, {upstream_addr}")]
    WriteStreamRequestHeader {
        #[source]
        source: CodecError,
        upstream_addr: RouteAddr,
    },
}

#[derive(Debug, Clone)]
pub struct StreamTracer {
    stream_context: StreamRuntime,
}
impl StreamTracer {
    pub fn new(stream_context: StreamRuntime) -> Self {
        Self { stream_context }
    }
}
impl ProbeRtt for StreamTracer {
    fn probe_rtt(
        &self,
        chain: &ConnChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Duration, AnyError>> + Send>>
    {
        let stream_context = self.stream_context.clone();
        let chain: Vec<ConnConfig> = chain.to_vec();
        Box::pin(async move { probe_rtt(&chain, &stream_context).await.map_err(Into::into) })
    }
    fn recycle(
        &self,
        chain: &ConnChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let Some(first) = chain.first() else {
            return Box::pin(async {});
        };
        let addr = first.address.clone();
        let protocol = first.address.protocol.clone();
        let connector_table = self.stream_context.connector_table.clone();
        Box::pin(async move {
            if addr.reverse_tunnel().is_some() {
                return;
            }
            let Ok(sock_addrs) = addr.address.to_socket_addrs().await else {
                return;
            };
            for sock_addr in sock_addrs.iter() {
                connector_table.reset_addr(protocol.as_ref(), *sock_addr);
            }
        })
    }
    fn reoptimize(
        &self,
        chain: &ConnChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let Some(first) = chain.first() else {
            return Box::pin(async {});
        };
        let addr = first.address.clone();
        let protocol = first.address.protocol.clone();
        let connector_table = self.stream_context.connector_table.clone();
        Box::pin(async move {
            if addr.reverse_tunnel().is_some() {
                return;
            }
            let Ok(sock_addrs) = addr.address.to_socket_addrs().await else {
                return;
            };
            for sock_addr in sock_addrs.iter() {
                connector_table.reoptimize(protocol.as_ref(), *sock_addr);
            }
        })
    }
    fn session_stats(
        &self,
        chain: &ConnChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>> {
        let Some(first) = chain.first() else {
            return Box::pin(async { None });
        };
        let addr = first.address.clone();
        let stream_type = first.address.protocol.clone();
        let connector_table = self.stream_context.connector_table.clone();
        Box::pin(async move {
            if let Some((_, name)) = addr.reverse_tunnel() {
                return connector_table.named_session_stats(&stream_type, name);
            }
            if !connector_table.reports_session_stats(stream_type.as_ref()) {
                return None;
            }
            let Ok(sock_addrs) = addr.address.to_socket_addrs().await else {
                return None;
            };
            sock_addrs.iter().find_map(|sock_addr| {
                connector_table.session_stats(stream_type.as_ref(), *sock_addr)
            })
        })
    }
}
pub async fn probe_rtt(
    proxies: &ConnChain,
    stream_context: &StreamRuntime,
) -> Result<Duration, TraceError> {
    if proxies.is_empty() {
        return Ok(Duration::from_secs(0));
    }

    let (mut stream, _addr, _sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) =
            connect_with_pool(proxy_addr, stream_context, true, crate::STREAM_IO_TIMEOUT).await?;
        (stream, proxy_addr.clone(), sock_addr)
    };

    let pairs = convert_proxies_to_header_crypto_pairs(proxies, None);

    let start = Instant::now();

    for (header, crypto) in &pairs {
        preamble::send_upgrade(&mut stream, crate::STREAM_IO_TIMEOUT, crypto).await?;
        timed_write_header_async(&mut stream, header, *crypto.key(), crate::STREAM_IO_TIMEOUT)
            .await?;
    }

    let validator = ValidatorRef::Replay(&stream_context.replay_validator);
    let resp: RouteResponse = timed_read_header_async(
        &mut stream,
        *pairs.last().unwrap().1.key(),
        &validator,
        crate::STREAM_IO_TIMEOUT,
    )
    .await?;
    if let Err(err) = resp.result {
        return Err(TraceError::Response { err });
    }

    let end = Instant::now();

    counter!("stream.rtt_probes").increment(1);
    Ok(end.duration_since(start))
}
#[derive(Debug, Error)]
pub enum TraceError {
    #[error("Connect error: {0}")]
    ConnectError(#[from] ConnectError),
    #[error("Heartbeat error: {0}")]
    PreambleError(#[from] PreambleError),
    #[error("Codec error: {0}")]
    Header(#[from] CodecError),
    #[error("Upstream responded with an error: {err}")]
    Response { err: RouteError },
}
