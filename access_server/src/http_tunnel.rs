use std::{io, sync::Arc, time::Instant};

use async_trait::async_trait;
use bytes::Bytes;
use common::{
    addr::InternetAddr,
    loading,
    stream::{
        addr::{StreamAddr, StreamType},
        config::{StreamProxyTable, StreamProxyTableBuilder},
        copy_bidirectional_with_payload_crypto,
        pool::{Pool, PoolBuilder},
        streams::{tcp::TcpServer, xor::XorStream},
        tokio_io, FailedStreamMetrics, FailedTunnelMetrics, IoStream, StreamMetrics,
        StreamServerHook, TunnelMetrics,
    },
};
use filter::{Filter, FilterBuilder};
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::{
    body::Incoming, http, service::service_fn, upgrade::Upgraded, Method, Request, Response,
};
use proxy_client::stream::{establish, StreamEstablishError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::ToSocketAddrs,
};
use tracing::{error, info, instrument, trace, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpProxyAccessBuilder {
    listen_addr: String,
    proxy_table: StreamProxyTableBuilder,
    stream_pool: PoolBuilder,
    filter: FilterBuilder,
}

#[async_trait]
impl loading::Builder for HttpProxyAccessBuilder {
    type Hook = HttpProxyAccess;
    type Server = TcpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<TcpServer<HttpProxyAccess>> {
        let stream_pool = self.stream_pool.build();
        let access = HttpProxyAccess::new(
            self.proxy_table.build(),
            stream_pool,
            self.filter.build().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Failed to build filter: {}", e),
                )
            })?,
        );
        let server = access.build(self.listen_addr).await?;
        Ok(server)
    }

    fn key(&self) -> &str {
        &self.listen_addr
    }

    fn build_hook(self) -> io::Result<HttpProxyAccess> {
        let stream_pool = self.stream_pool.build();
        let access = HttpProxyAccess::new(
            self.proxy_table.build(),
            stream_pool,
            self.filter.build().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Failed to build filter: {}", e),
                )
            })?,
        );
        Ok(access)
    }
}

#[derive(Debug)]
pub struct HttpProxyAccess {
    proxy_table: Arc<StreamProxyTable>,
    stream_pool: Pool,
    filter: Filter,
}

impl HttpProxyAccess {
    pub fn new(proxy_table: StreamProxyTable, stream_pool: Pool, filter: Filter) -> Self {
        Self {
            proxy_table: Arc::new(proxy_table),
            stream_pool,
            filter,
        }
    }

    #[instrument(skip(self, listen_addr))]
    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy<S>(&self, downstream: S) -> Result<(), TunnelError>
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
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
        trace!(?req, "Received request");

        if Method::CONNECT == req.method() {
            return self.proxy_connect(req).await;
        }

        let start = std::time::Instant::now();

        let method = req.method().clone();
        let host = req.uri().host().ok_or(TunnelError::HttpNoHost)?;
        let port = req.uri().port_u16().unwrap_or(80);
        let addr = format!("{}:{}", host, port);

        let action = self.filter.filter(&addr);
        match action {
            filter::Action::Proxy => (),
            filter::Action::Block => {
                trace!(?addr, "Blocked {}", method);
                return Ok(respond_with_rejection());
            }
            filter::Action::Direct => {
                let upstream = tokio::net::TcpStream::connect(&addr)
                    .await
                    .map_err(TunnelError::Direct)?;
                let res = tls_http(upstream, req).await;
                info!(?addr, "Direct {} finished", method);
                return res;
            }
        }

        // Establish proxy chain
        let proxy_chain = self.proxy_table.choose_chain();
        let (upstream, upstream_addr, upstream_sock_addr) = establish(
            &proxy_chain.chain,
            StreamAddr {
                address: addr.clone().into(),
                stream_type: StreamType::Tcp,
            },
            &self.stream_pool,
        )
        .await?;

        let res = match &proxy_chain.payload_crypto {
            Some(crypto) => {
                // Establish encrypted stream
                let xor_stream = XorStream::upgrade(upstream, crypto);

                tls_http(xor_stream, req).await
            }
            None => tls_http(upstream, req).await,
        };

        let end = std::time::Instant::now();
        let metrics = FailedTunnelMetrics {
            stream: FailedStreamMetrics {
                start,
                end,
                upstream_addr,
                upstream_sock_addr,
                downstream_addr: None,
            },
            destination: addr.into(),
        };
        info!(%metrics, "{} finished", method);

        res
    }

    async fn proxy_connect(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
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
        let addr = match host_addr(req.uri()) {
            Some(addr) => addr,
            None => {
                let uri = req.uri().to_string();
                error!(?uri, "CONNECT host is not socket addr");
                let mut resp = Response::new(full("CONNECT must be to a socket address"));
                *resp.status_mut() = http::StatusCode::BAD_REQUEST;

                return Ok(resp);
            }
        };
        let action = self.filter.filter(&addr);
        let http_connect = match action {
            filter::Action::Proxy => Some(HttpConnect::new(
                Arc::clone(&self.proxy_table),
                self.stream_pool.clone(),
            )),
            filter::Action::Block => {
                trace!(?addr, "Blocked CONNECT");
                return Ok(respond_with_rejection());
            }
            filter::Action::Direct => None,
        };

        tokio::task::spawn(async move {
            upgrade(req, addr, http_connect).await;
        });

        // Return STATUS_OK
        Ok(Response::new(empty()))
    }
}

async fn tls_http<S>(
    upstream: S,
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError>
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

async fn upgrade(req: Request<Incoming>, addr: String, http_connect: Option<HttpConnect>) {
    let upgraded = match hyper::upgrade::on(req).await {
        Ok(upgraded) => upgraded,
        Err(e) => {
            error!(?e, ?addr, "Upgrade error");
            return;
        }
    };

    // Proxy
    if let Some(http_connect) = http_connect {
        match http_connect.proxy(upgraded, addr.into()).await {
            Ok(metrics) => {
                info!(%metrics, "CONNECT finished");
            }
            Err(HttpConnectError::IoCopy { source: e, metrics }) => {
                info!(?e, %metrics, "CONNECT error");
            }
            Err(e) => error!(?e, "CONNECT error"),
        };
        return;
    }

    // Direct
    let upstream = match tokio::net::TcpStream::connect(&addr).await {
        Ok(upstream) => upstream,
        Err(e) => {
            error!(?e, ?addr, "Failed to connect to upstream directly");
            return;
        }
    };
    match tokio_io::timed_copy_bidirectional(upgraded, upstream).await {
        Ok(metrics) => {
            info!(?addr, ?metrics, "Direct CONNECT finished");
        }
        Err(e) => {
            info!(?e, ?addr, "Direct CONNECT error");
        }
    }
}

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Hyper error")]
    HyperError(#[from] hyper::Error),
    #[error("No host in HTTP request")]
    HttpNoHost,
    #[error("Direct connection error")]
    Direct(#[source] io::Error),
}

impl loading::Hook for HttpProxyAccess {}

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

struct HttpConnect {
    proxy_table: Arc<StreamProxyTable>,
    stream_pool: Pool,
}

impl HttpConnect {
    pub fn new(proxy_table: Arc<StreamProxyTable>, stream_pool: Pool) -> Self {
        Self {
            proxy_table,
            stream_pool,
        }
    }

    // Create a TCP connection to host:port, build a tunnel between the connection and
    // the upgraded connection
    #[instrument(skip(self, upgraded))]
    pub async fn proxy(
        &self,
        upgraded: Upgraded,
        address: InternetAddr,
    ) -> Result<TunnelMetrics, HttpConnectError> {
        let start = Instant::now();

        // Establish proxy chain
        let destination = StreamAddr {
            address: address.clone(),
            stream_type: StreamType::Tcp,
        };
        let proxy_chain = self.proxy_table.choose_chain();
        let (upstream, upstream_addr, upstream_sock_addr) =
            establish(&proxy_chain.chain, destination, &self.stream_pool).await?;

        // Proxy data
        let res = copy_bidirectional_with_payload_crypto(
            upgraded,
            upstream,
            proxy_chain.payload_crypto.as_ref(),
        )
        .await;
        let end = Instant::now();
        let (from_client, from_server) = res.map_err(|e| HttpConnectError::IoCopy {
            source: e,
            metrics: FailedTunnelMetrics {
                stream: FailedStreamMetrics {
                    start,
                    end,
                    upstream_addr: upstream_addr.clone(),
                    upstream_sock_addr,
                    downstream_addr: None,
                },
                destination: address.clone(),
            },
        })?;

        let metrics = TunnelMetrics {
            stream: StreamMetrics {
                start,
                end,
                bytes_uplink: from_client,
                bytes_downlink: from_server,
                upstream_addr,
                upstream_sock_addr,
                downstream_addr: None,
            },
            destination: address,
        };
        Ok(metrics)
    }
}

#[derive(Debug, Error)]
pub enum HttpConnectError {
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Failed to copy data between streams")]
    IoCopy {
        #[source]
        source: tokio_io::CopyBiError,
        metrics: FailedTunnelMetrics,
    },
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

fn respond_with_rejection() -> Response<BoxBody<Bytes, hyper::Error>> {
    Response::builder()
        .status(503)
        .body(full("Blocked"))
        .unwrap()
}
