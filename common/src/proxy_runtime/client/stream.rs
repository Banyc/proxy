use crate::{
    header::{
        codec::{CodecError, timed_read_header_async, timed_write_header_async},
        preamble::{self, PreambleError},
        route::{RouteError, RouteResponse},
    },
    proxy_runtime::{
        addr::RouteAddr, conn::stream::ConnAndAddr, context::StreamRuntime,
        relay::same_key_nonce_ciphertext,
    },
    route::{HopConfig, ProbeRtt, RouteChain, convert_proxies_to_header_crypto_pairs},
    stream_runtime::{
        HasIoAddr, IoConnection, OwnedIoStream,
        pool::{ConnectError, connect_with_pool},
    },
};
use ae::anti_replay::ValidatorRef;
use metrics::counter;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::{instrument, trace};

type PayloadCryptoReader =
    tokio_chacha20::stream::NonceCiphertextReader<tokio::io::ReadHalf<Box<dyn IoConnection>>>;
type PayloadCryptoWriter =
    tokio_chacha20::stream::NonceCiphertextWriter<tokio::io::WriteHalf<Box<dyn IoConnection>>>;
#[derive(Debug)]
struct PayloadCryptoConn {
    stream: tokio_chacha20::stream::DuplexStream<PayloadCryptoReader, PayloadCryptoWriter>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
}

impl PayloadCryptoConn {
    fn wrap(
        stream: Box<dyn IoConnection>,
        crypto: &tokio_chacha20::config::Config,
    ) -> Box<dyn IoConnection> {
        let local_addr = stream.local_addr().ok();
        let peer_addr = stream.peer_addr().ok();
        let (reader, writer) = tokio::io::split(stream);
        let (reader, writer) = same_key_nonce_ciphertext(crypto.key(), reader, writer);
        Box::new(Self {
            stream: tokio_chacha20::stream::DuplexStream::new(reader, writer),
            local_addr,
            peer_addr,
        })
    }
}

impl AsyncRead for PayloadCryptoConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for PayloadCryptoConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl OwnedIoStream for PayloadCryptoConn {}
impl IoConnection for PayloadCryptoConn {}

impl HasIoAddr for PayloadCryptoConn {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.peer_addr.ok_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "peer address unavailable")
        })
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr.ok_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "local address unavailable")
        })
    }
}

#[instrument(skip_all)]
pub async fn establish(
    proxies: &RouteChain,
    destination: RouteAddr,
    stream_context: &StreamRuntime,
) -> Result<ConnAndAddr, StreamEstablishError> {
    if proxies.is_empty() {
        let (stream, sock_addr) = connect_with_pool(
            &destination,
            None,
            stream_context,
            true,
            crate::STREAM_IO_TIMEOUT,
        )
        .await
        .map_err(|source| StreamEstablishError::ConnectDestination {
            source: Box::new(source),
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
        let proxy_key = Some(*proxies[0].header_crypto.key());
        let (stream, sock_addr) = connect_with_pool(
            proxy_addr,
            proxy_key,
            stream_context,
            true,
            crate::STREAM_IO_TIMEOUT,
        )
        .await
        .map_err(|source| StreamEstablishError::ConnectFirstProxyServer {
            source: Box::new(source),
            upstream_addr: proxy_addr.clone(),
        })?;
        (stream, proxy_addr.clone(), sock_addr)
    };
    stream.set_stream_name(&destination.address.to_string());
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, Some(destination));
    for ((header, crypto), proxy) in pairs.iter().zip(proxies) {
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
        if let Some(payload_crypto) = &proxy.payload_crypto {
            stream = PayloadCryptoConn::wrap(stream, payload_crypto);
        }
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
        source: Box<ConnectError>,
        upstream_addr: RouteAddr,
    },
    #[error("Failed to connect to first proxy server: {source}, {upstream_addr}")]
    ConnectFirstProxyServer {
        #[source]
        source: Box<ConnectError>,
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
    fn probe_kind(&self) -> &'static str {
        "stream"
    }
    fn probe_rtt(
        &self,
        chain: &RouteChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::route::ProbeOutcome> + Send>>
    {
        let stream_context = self.stream_context.clone();
        let chain: Vec<HopConfig> = chain.to_vec();
        Box::pin(async move {
            crate::route::ProbeOutcome {
                rtt: probe_rtt(&chain, &stream_context).await.map_err(Into::into),
                // The stream probe has no teardown epilog to observe.
                epilog: None,
            }
        })
    }
    fn recycle(
        &self,
        chain: &RouteChain,
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
        chain: &RouteChain,
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
        chain: &RouteChain,
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
    proxies: &RouteChain,
    stream_context: &StreamRuntime,
) -> Result<Duration, TraceError> {
    if proxies.is_empty() {
        return Ok(Duration::from_secs(0));
    }
    let (mut stream, _addr, _sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let proxy_key = Some(*proxies[0].header_crypto.key());
        let (stream, sock_addr) = connect_with_pool(
            proxy_addr,
            proxy_key,
            stream_context,
            true,
            crate::STREAM_IO_TIMEOUT,
        )
        .await?;
        (stream, proxy_addr.clone(), sock_addr)
    };
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, None);
    let start = Instant::now();
    for (index, ((header, crypto), proxy)) in pairs.iter().zip(proxies).enumerate() {
        preamble::send_upgrade(&mut stream, crate::STREAM_IO_TIMEOUT, crypto).await?;
        timed_write_header_async(&mut stream, header, *crypto.key(), crate::STREAM_IO_TIMEOUT)
            .await?;
        if index + 1 < proxies.len()
            && let Some(payload_crypto) = &proxy.payload_crypto
        {
            stream = PayloadCryptoConn::wrap(stream, payload_crypto);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::{ConnectorConfig, connector_config_cell},
        lifecycle::retention::{RetentionActor, RetentionActorSender},
        proxy_runtime::{
            addr::REVERSE_TUNNEL_TCP_PROTOCOL,
            connect::stream::{NamedStreamConnect, StreamConnect, StreamConnectorTable},
        },
        session::SessionSpawner,
        stream_runtime::pool::StreamConnPool,
    };
    use ae::anti_replay::ReplayValidator;
    use async_trait::async_trait;
    use std::{
        collections::HashMap,
        io,
        net::SocketAddr,
        str::FromStr,
        sync::{Arc, Mutex},
    };
    use swap::Swap;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// A stream whose writes either always fail or fail after the first
    /// `poll_write`. Each `write_header_async` call issues exactly one
    /// `poll_write`, so the two modes deterministically fail the preamble or
    /// the request header respectively.
    #[derive(Debug)]
    struct FailingConn {
        addr: SocketAddr,
        first_write_only: bool,
        wrote: Mutex<bool>,
    }
    impl AsyncRead for FailingConn {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for FailingConn {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if this.first_write_only {
                let mut wrote = this.wrote.lock().unwrap();
                if !*wrote {
                    *wrote = true;
                    return Poll::Ready(Ok(buf.len()));
                }
            }
            Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl HasIoAddr for FailingConn {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.addr)
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.addr)
        }
    }
    impl OwnedIoStream for FailingConn {}
    impl IoConnection for FailingConn {}

    #[derive(Debug, Clone, Copy)]
    enum MockOutcome {
        Refused,
        Failing,
        FirstWriteOnly,
    }

    #[derive(Debug)]
    struct MockConnect {
        outcome: MockOutcome,
        resets: Mutex<Vec<SocketAddr>>,
        reoptimized: Mutex<Vec<SocketAddr>>,
    }
    impl MockConnect {
        fn refused() -> Self {
            Self {
                outcome: MockOutcome::Refused,
                resets: Mutex::new(Vec::new()),
                reoptimized: Mutex::new(Vec::new()),
            }
        }
        fn failing() -> Self {
            Self {
                outcome: MockOutcome::Failing,
                resets: Mutex::new(Vec::new()),
                reoptimized: Mutex::new(Vec::new()),
            }
        }
        fn first_write_only() -> Self {
            Self {
                outcome: MockOutcome::FirstWriteOnly,
                resets: Mutex::new(Vec::new()),
                reoptimized: Mutex::new(Vec::new()),
            }
        }
    }
    #[async_trait]
    impl StreamConnect for MockConnect {
        async fn connect(
            &self,
            addr: SocketAddr,
            _obfuscation_key: Option<[u8; 32]>,
        ) -> io::Result<Box<dyn IoConnection>> {
            match self.outcome {
                MockOutcome::Refused => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
                MockOutcome::Failing => Ok(Box::new(FailingConn {
                    addr,
                    first_write_only: false,
                    wrote: Mutex::new(false),
                })),
                MockOutcome::FirstWriteOnly => Ok(Box::new(FailingConn {
                    addr,
                    first_write_only: true,
                    wrote: Mutex::new(false),
                })),
            }
        }
        fn reset_addr(&self, addr: SocketAddr) {
            self.resets.lock().unwrap().push(addr);
        }
        fn reoptimize(&self, addr: SocketAddr) {
            self.reoptimized.lock().unwrap().push(addr);
        }
        fn session_stats(&self, addr: SocketAddr) -> Option<String> {
            Some(format!("stats@{addr}"))
        }
        fn reports_session_stats(&self) -> bool {
            true
        }
    }

    /// A named connector that reports statistics without ever dialing.
    #[derive(Debug)]
    struct NamedStats;
    #[async_trait]
    impl NamedStreamConnect for NamedStats {
        async fn connect(&self) -> io::Result<Box<dyn IoConnection>> {
            Err(io::Error::other("not dialed in this test"))
        }
        fn session_stats(&self) -> Option<String> {
            Some("peer=10.0.0.9:1,uptime=1s".to_string())
        }
    }

    fn test_runtime(connectors: HashMap<Arc<str>, Arc<dyn StreamConnect>>) -> StreamRuntime {
        // The runtime's actors are not exercised here (nothing spawns a session
        // or retains a guard), so the channel endpoints are held only long
        // enough to construct a valid `StreamRuntime`.
        let (session_spawner, _session_rx) = SessionSpawner::channel();
        let (_retention_actor, retention): (RetentionActor, RetentionActorSender) =
            RetentionActor::new();
        StreamRuntime {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table: Arc::new(StreamConnectorTable::new(
                connector_config_cell(ConnectorConfig::default()).0,
                connectors,
            )),
            replay_validator: Arc::new(ReplayValidator::new(
                VALIDATOR_TIME_FRAME,
                VALIDATOR_CAPACITY,
            )),
            session_spawner,
            retention,
        }
    }

    fn tcp_connectors(mock: Arc<MockConnect>) -> HashMap<Arc<str>, Arc<dyn StreamConnect>> {
        HashMap::from([(Arc::from("tcp"), mock as Arc<dyn StreamConnect>)])
    }

    fn hop(address: &str) -> HopConfig {
        HopConfig {
            name: None,
            address: RouteAddr::from_str(address).unwrap(),
            header_crypto: tokio_chacha20::config::Config::new([0x11; 32].into()),
            payload_crypto: None,
        }
    }

    fn destination(addr: &str) -> RouteAddr {
        RouteAddr {
            address: crate::addr::InternetAddr::from_str(addr).unwrap(),
            protocol: Arc::from("tcp"),
        }
    }

    /// An empty chain connects straight to the destination; a refused
    /// destination is reported as `ConnectDestination`, not as a proxy error.
    #[tokio::test]
    async fn establish_reports_a_destination_connect_failure() {
        let mock = Arc::new(MockConnect::refused());
        let runtime = test_runtime(tcp_connectors(mock));
        let err = establish(&[], destination("127.0.0.1:9"), &runtime)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StreamEstablishError::ConnectDestination { .. }),
            "expected ConnectDestination, got {err:?}"
        );
    }

    /// With a chain, the first hop's dial failure is reported as
    /// `ConnectFirstProxyServer`.
    #[tokio::test]
    async fn establish_reports_a_first_proxy_connect_failure() {
        let mock = Arc::new(MockConnect::refused());
        let runtime = test_runtime(tcp_connectors(mock));
        let chain = vec![hop("tcp://10.0.0.1:9000")];
        let err = establish(&chain, destination("127.0.0.1:9"), &runtime)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StreamEstablishError::ConnectFirstProxyServer { .. }),
            "expected ConnectFirstProxyServer, got {err:?}"
        );
    }

    /// A stream that refuses every write fails the chain at the heartbeat
    /// upgrade, not at the header.
    #[tokio::test]
    async fn establish_reports_a_refused_heartbeat_upgrade() {
        let mock = Arc::new(MockConnect::failing());
        let runtime = test_runtime(tcp_connectors(mock));
        let chain = vec![hop("tcp://10.0.0.1:9000")];
        let err = establish(&chain, destination("127.0.0.1:9"), &runtime)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StreamEstablishError::WriteHeartbeatUpgrade { .. }),
            "expected WriteHeartbeatUpgrade, got {err:?}"
        );
    }

    /// A stream that accepts the (single-write) upgrade but refuses the next
    /// write fails the chain at the request header.
    #[tokio::test]
    async fn establish_reports_a_refused_request_header_write() {
        let mock = Arc::new(MockConnect::first_write_only());
        let runtime = test_runtime(tcp_connectors(mock));
        let chain = vec![hop("tcp://10.0.0.1:9000")];
        let err = establish(&chain, destination("127.0.0.1:9"), &runtime)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StreamEstablishError::WriteStreamRequestHeader { .. }),
            "expected WriteStreamRequestHeader, got {err:?}"
        );
    }

    /// The stream tracer labels itself, probes an empty chain as zero, and
    /// applies each first-hop decision to the resolved first-hop address.
    #[tokio::test]
    async fn stream_tracer_applies_first_hop_decisions_to_the_resolved_addr() {
        let mock = Arc::new(MockConnect::refused());
        let runtime = test_runtime(tcp_connectors(Arc::clone(&mock)));
        let tracer = StreamTracer::new(runtime);
        assert_eq!(tracer.probe_kind(), "stream");

        let chain = vec![hop("tcp://10.0.0.1:9000")];
        tracer.recycle(&chain).await;
        assert_eq!(
            mock.resets.lock().unwrap().as_slice(),
            ["10.0.0.1:9000".parse::<SocketAddr>().unwrap()]
        );
        tracer.reoptimize(&chain).await;
        assert_eq!(
            mock.reoptimized.lock().unwrap().as_slice(),
            ["10.0.0.1:9000".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(
            tracer.session_stats(&chain).await.as_deref(),
            Some("stats@10.0.0.1:9000")
        );

        // An empty chain has no first hop: no probe, no stats, no reset.
        let empty: Vec<HopConfig> = Vec::new();
        assert_eq!(tracer.session_stats(&empty).await, None);
        tracer.recycle(&empty).await;
        tracer.reoptimize(&empty).await;
        let zero = tracer
            .probe_rtt(&empty)
            .await
            .rtt
            .expect("an empty chain probes as zero");
        assert_eq!(zero, Duration::ZERO);
    }

    /// A reverse-tunnel first hop is addressed by its registered name: the
    /// tracer reads the named session stats and neither resets nor
    /// reoptimizes the (non-dialable) virtual hop.
    #[tokio::test]
    async fn stream_tracer_reads_a_reverse_tunnel_hops_named_stats() {
        let runtime = test_runtime(HashMap::new());
        let _registration = runtime.connector_table.register_named(
            Arc::from(REVERSE_TUNNEL_TCP_PROTOCOL),
            Arc::from("private-a"),
            Arc::new(NamedStats),
        );
        let tracer = StreamTracer::new(runtime);
        let chain = vec![hop("revtuntcp://private-a")];
        assert_eq!(
            tracer.session_stats(&chain).await.as_deref(),
            Some("peer=10.0.0.9:1,uptime=1s")
        );
        tracer.recycle(&chain).await;
        tracer.reoptimize(&chain).await;
    }
}
