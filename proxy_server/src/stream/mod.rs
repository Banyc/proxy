use std::{io, net::SocketAddr};

use async_trait::async_trait;
use common::{
    crypto::{XorCrypto, XorCryptoCursor},
    error::ProxyProtocolError,
    header::{read_header_async, write_header_async, InternetAddr},
    heartbeat,
    stream::{
        connect_with_pool,
        header::StreamRequestHeader,
        pool::{Pool, PoolBuilder},
        xor::XorStream,
        CreatedStream, IoAddr, IoStream, StreamMetrics, StreamServerHook,
    },
};
use serde::Deserialize;
use tracing::{error, info, instrument};

pub mod kcp;
pub mod tcp;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct StreamProxyServerBuilder {
    pub header_xor_key: Vec<u8>,
    pub payload_xor_key: Option<Vec<u8>>,
    pub stream_pool: PoolBuilder,
}

impl StreamProxyServerBuilder {
    pub async fn build(self) -> io::Result<StreamProxyServer> {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        let payload_crypto = self.payload_xor_key.map(XorCrypto::new);
        let stream_pool = self.stream_pool.build();
        let stream_proxy = StreamProxyServer::new(header_crypto, payload_crypto, stream_pool);
        Ok(stream_proxy)
    }
}

pub struct StreamProxyServer {
    acceptor: StreamProxyAcceptor,
    payload_crypto: Option<XorCrypto>,
}

impl StreamProxyServer {
    pub fn new(
        header_crypto: XorCrypto,
        payload_crypto: Option<XorCrypto>,
        stream_pool: Pool,
    ) -> Self {
        Self {
            acceptor: StreamProxyAcceptor::new(header_crypto, stream_pool),
            payload_crypto,
        }
    }

    #[instrument(skip(self))]
    async fn proxy<S>(&self, mut downstream: S) -> io::Result<()>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream
            .peer_addr()
            .inspect_err(|e| error!(?e, "Failed to get downstream address"))?;

        // Establish proxy chain
        let (mut upstream, upstream_addr, upstream_sock_addr) =
            match self.acceptor.establish(&mut downstream).await {
                Ok(upstream) => upstream,
                Err(e) => {
                    self.handle_proxy_error(&mut downstream, e).await;
                    return Ok(());
                }
            };

        // Copy data
        let res = match &self.payload_crypto {
            Some(crypto) => {
                // Establish encrypted stream
                let mut xor_stream = XorStream::upgrade(downstream, crypto);
                tokio::io::copy_bidirectional(&mut xor_stream, &mut upstream).await
            }
            None => tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await,
        };
        let (bytes_uplink, bytes_downlink) =
            res.inspect_err(|e| error!(?e, ?upstream, "Failed to copy data between streams"))?;

        let end = std::time::Instant::now();
        let metrics = StreamMetrics {
            start,
            end,
            bytes_uplink,
            bytes_downlink,
            upstream_addr,
            upstream_sock_addr,
            downstream_addr,
        };
        info!(%metrics, "Connection closed normally");
        Ok(())
    }

    #[instrument(skip(self, e))]
    async fn handle_proxy_error<S>(&self, stream: &mut S, e: ProxyProtocolError)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        error!(?e, "Connection closed with error");
        // let _ = self
        //     .acceptor
        //     .respond_with_error(stream, e)
        //     .await
        //     .inspect_err(|e| {
        //         let peer_addr = stream.peer_addr().ok();
        //         error!(
        //             ?e,
        //             ?peer_addr,
        //             "Failed to respond with error to downstream after error"
        //         )
        //     });
    }
}

#[async_trait]
impl StreamServerHook for StreamProxyServer {
    #[instrument(skip(self))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        if let Err(e) = self.proxy(stream).await {
            error!(?e, "Connection closed with error");
        }
    }
}

pub struct StreamProxyAcceptor {
    crypto: XorCrypto,
    stream_pool: Pool,
}

impl StreamProxyAcceptor {
    pub fn new(crypto: XorCrypto, stream_pool: Pool) -> Self {
        Self {
            crypto,
            stream_pool,
        }
    }

    #[instrument(skip(self))]
    async fn establish<S>(
        &self,
        downstream: &mut S,
    ) -> Result<(CreatedStream, InternetAddr, SocketAddr), ProxyProtocolError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        // Wait for heartbeat upgrade
        heartbeat::wait_upgrade(downstream).await.inspect_err(|e| {
            let downstream_addr = downstream.peer_addr().ok();
            error!(
                ?e,
                ?downstream_addr,
                "Failed to read heartbeat header from downstream"
            )
        })?;

        // Decode header
        let mut read_crypto_cursor = XorCryptoCursor::new(&self.crypto);
        let header: StreamRequestHeader = read_header_async(downstream, &mut read_crypto_cursor)
            .await
            .inspect_err(|e| {
                let downstream_addr = downstream.peer_addr().ok();
                error!(
                    ?e,
                    ?downstream_addr,
                    "Failed to read header from downstream"
                )
            })?;

        // Connect to upstream
        let (upstream, sock_addr) =
            connect_with_pool(&header.upstream, &self.stream_pool, false).await?;

        // // Write Ok response
        // let resp = ResponseHeader { result: Ok(()) };
        // let mut write_crypto_cursor = XorCryptoCursor::new(&self.crypto);
        // write_header_async(downstream, &resp, &mut write_crypto_cursor)
        //     .await
        //     .inspect_err(
        //         |e| error!(?e, ?header.upstream, "Failed to write response to downstream"),
        //     )?;

        // Return upstream
        Ok((upstream, header.upstream.address, sock_addr))
    }

    #[instrument(skip(self, stream))]
    async fn respond_with_error<S>(
        &self,
        stream: &mut S,
        error: ProxyProtocolError,
    ) -> Result<(), ProxyProtocolError>
    where
        S: IoStream + IoAddr,
    {
        let local_addr = stream
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;

        // Respond with error
        let resp = error.into_response_header(local_addr.into());
        let mut crypto_cursor = XorCryptoCursor::new(&self.crypto);
        write_header_async(stream, &resp, &mut crypto_cursor)
            .await
            .inspect_err(|e| {
                let peer_addr = stream.peer_addr().ok();
                error!(
                    ?e,
                    ?peer_addr,
                    "Failed to write response to downstream after error"
                )
            })?;

        Ok(())
    }
}
