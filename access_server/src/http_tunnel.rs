use std::io;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use common::addr::any_addr;
use common::crypto::XorCrypto;
use common::error::ProxyProtocolError;
use common::header::InternetAddr;
use common::stream::header::{ProxyConfigBuilder, StreamProxyConfig, StreamType};
use common::stream::pool::Pool;
use common::stream::tcp::TcpServer;
use common::stream::xor::XorStream;
use common::stream::{self, IoStream, StreamMetrics, StreamServerHook};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{http, Method, Request, Response};

use proxy_client::stream::establish;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::ToSocketAddrs;
use tracing::{error, info, instrument, trace, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpProxyAccessBuilder {
    listen_addr: String,
    proxy_configs: Vec<ProxyConfigBuilder>,
    payload_xor_key: Option<Vec<u8>>,
    stream_pool: Option<Vec<String>>,
}

impl HttpProxyAccessBuilder {
    pub async fn build(self) -> io::Result<TcpServer<HttpProxyAccess>> {
        let stream_pool = Pool::new();
        if let Some(addrs) = self.stream_pool {
            stream_pool.add_many_queues(addrs.into_iter().map(|v| v.into()));
        }
        let access = HttpProxyAccess::new(
            self.proxy_configs.into_iter().map(|x| x.build()).collect(),
            self.payload_xor_key.map(XorCrypto::new),
            stream_pool,
        );
        let server = access.build(self.listen_addr).await?;
        Ok(server)
    }
}

pub struct HttpProxyAccess {
    proxy_configs: Arc<Vec<StreamProxyConfig>>,
    payload_crypto: Option<Arc<XorCrypto>>,
    stream_pool: Pool,
}

impl HttpProxyAccess {
    pub fn new(
        proxy_configs: Vec<StreamProxyConfig>,
        payload_crypto: Option<XorCrypto>,
        stream_pool: Pool,
    ) -> Self {
        Self {
            proxy_configs: Arc::new(proxy_configs),
            payload_crypto: payload_crypto.map(Arc::new),
            stream_pool,
        }
    }

    #[instrument(skip(self, listen_addr))]
    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy<S>(&self, downstream: S) -> Result<(), Error>
    where
        S: IoStream,
    {
        hyper::server::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .serve_connection(downstream, service_fn(move |req| self.proxy_svc(req)))
            .with_upgrades()
            .await?;
        Ok(())
    }

    #[instrument(skip(self, req))]
    async fn proxy_svc(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Error> {
        trace!(?req, "Received request");

        if Method::CONNECT == req.method() {
            // Received an HTTP request like:
            // ```
            // CONNECT www.domain.com:443 HTTP/1.1
            // Host: www.domain.com:443
            // Proxy-Connection: Keep-Alive
            // ```
            //
            // When HTTP method is CONNECT we should return an empty body
            // then we can eventually upgrade the connection and talk a new protocol.
            //
            // Note: only after client received an empty body with STATUS_OK can the
            // connection be upgraded, so we can't return a response inside
            // `on_upgrade` future.
            if let Some(addr) = host_addr(req.uri()) {
                let tunnel = HttpTunnel::new(
                    Arc::clone(&self.proxy_configs),
                    self.payload_crypto.clone(),
                    self.stream_pool.clone(),
                );
                tokio::task::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            if let Err(e) = tunnel.tunnel(upgraded, addr.into()).await {
                                error!(?e, "Tunnel error");
                            };
                        }
                        Err(e) => error!(?e, "Upgrade error"),
                    }
                });

                // Return STATUS_OK
                Ok(Response::new(empty()))
            } else {
                let uri = req.uri().to_string();
                error!(?uri, "CONNECT host is not socket addr");
                let mut resp = Response::new(full("CONNECT must be to a socket address"));
                *resp.status_mut() = http::StatusCode::BAD_REQUEST;

                Ok(resp)
            }
        } else {
            let host = req
                .uri()
                .host()
                .ok_or(Error::HttpNoHost)
                .inspect_err(|e| error!(?e, "No host in HTTP request"))?;
            let port = req.uri().port_u16().unwrap_or(80);
            let addr = format!("{}:{}", host, port);

            // Establish ProxyProtocol
            let (upstream, _) = establish(
                &self.proxy_configs,
                stream::header::RequestHeader {
                    address: addr.into(),
                    stream_type: StreamType::Tcp,
                },
                &self.stream_pool,
            )
            .await
            .inspect_err(|e| {
                error!(?e, "Failed to establish proxy protocol");
            })?;

            match &self.payload_crypto {
                Some(crypto) => {
                    // Establish encrypted stream
                    let xor_stream = XorStream::upgrade(upstream, crypto);

                    self.tls_http(xor_stream, req).await
                }
                None => self.tls_http(upstream, req).await,
            }
        }
    }

    async fn tls_http<S>(
        &self,
        upstream: S,
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Error>
    where
        S: AsyncWrite + AsyncRead + Send + Unpin,
    {
        // Establish TLS connection
        let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(upstream)
            .await
            .inspect_err(|e| error!(?e, "Failed to establish HTTP/1 handshake to upstream"))?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                warn!(?err, "Connection failed");
            }
        });

        // Send HTTP/1 request
        let resp = sender
            .send_request(req)
            .await
            .inspect_err(|e| error!(?e, "Failed to send HTTP/1 request to upstream"))?;
        Ok(resp.map(|b| b.boxed()))
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Proxy protocol error")]
    ProxyProtocolError(#[from] ProxyProtocolError),
    #[error("Hyper error")]
    HyperError(#[from] hyper::Error),
    #[error("Http error")]
    HttpNoHost,
}

#[async_trait]
impl StreamServerHook for HttpProxyAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream,
    {
        let res = self.proxy(stream).await;
        if let Err(e) = res {
            error!(?e, "Failed to proxy");
        }
    }
}

struct HttpTunnel {
    proxy_configs: Arc<Vec<StreamProxyConfig>>,
    payload_crypto: Option<Arc<XorCrypto>>,
    stream_pool: Pool,
}

impl HttpTunnel {
    pub fn new(
        proxy_configs: Arc<Vec<StreamProxyConfig>>,
        payload_crypto: Option<Arc<XorCrypto>>,
        stream_pool: Pool,
    ) -> Self {
        Self {
            proxy_configs,
            payload_crypto,
            stream_pool,
        }
    }

    // Create a TCP connection to host:port, build a tunnel between the connection and
    // the upgraded connection
    #[instrument(skip(self, upgraded))]
    pub async fn tunnel(
        &self,
        mut upgraded: Upgraded,
        address: InternetAddr,
    ) -> Result<(), ProxyProtocolError> {
        let start = Instant::now();

        // Establish ProxyProtocol
        let destination = stream::header::RequestHeader {
            address: address.clone(),
            stream_type: StreamType::Tcp,
        };
        let (mut upstream, upstream_sock_addr) =
            establish(&self.proxy_configs, destination, &self.stream_pool)
                .await
                .inspect_err(|e| {
                    error!(?e, "Failed to establish proxy protocol");
                })?;

        let downstream_addr = any_addr(&upstream_sock_addr.ip());

        // Proxying data
        let res = match &self.payload_crypto {
            Some(crypto) => {
                // Establish encrypted stream
                let mut xor_stream = XorStream::upgrade(upstream, crypto);
                tokio::io::copy_bidirectional(&mut upgraded, &mut xor_stream).await
            }
            None => tokio::io::copy_bidirectional(&mut upgraded, &mut upstream).await,
        };
        let (from_client, from_server) = res.inspect_err(|e| {
            error!(?e, "Failed to copy data");
        })?;

        // Print message when done
        let end = Instant::now();
        let metrics = StreamMetrics {
            start,
            end,
            bytes_uplink: from_client,
            bytes_downlink: from_server,
            upstream_addr: address,
            upstream_sock_addr,
            downstream_addr,
        };
        info!(%metrics, "Tunnel closed normally");

        Ok(())
    }
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}
