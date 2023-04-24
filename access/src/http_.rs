#![deny(warnings)]

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use client::tcp_proxy_client::TcpProxyStream;
use common::error::ProxyProtocolError;
use common::header::ProxyConfig;
use common::tcp::{TcpServer, TcpServerHook};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{http, Method, Request, Response};

use thiserror::Error;
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::{error, instrument, trace, warn};

pub struct HttpProxyAccess {
    proxy_configs: Arc<Vec<ProxyConfig>>,
}

impl HttpProxyAccess {
    pub fn new(proxy_configs: Vec<ProxyConfig>) -> Self {
        Self {
            proxy_configs: Arc::new(proxy_configs),
        }
    }

    #[instrument(skip(self, listen_addr))]
    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy(&self, downstream: TcpStream) -> Result<(), Error> {
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
                let tunnel = HttpTunnel::new(Arc::clone(&self.proxy_configs));
                tokio::task::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            if let Err(e) = tunnel.tunnel(upgraded, addr).await {
                                error!(?e, "Tunnel error");
                            };
                        }
                        Err(e) => error!(?e, "Upgrade error"),
                    }
                });

                Ok(Response::new(empty()))
            } else {
                let uri = req.uri().to_string();
                error!(?uri, "CONNECT host is not socket addr");
                let mut resp = Response::new(full("CONNECT must be to a socket address"));
                *resp.status_mut() = http::StatusCode::BAD_REQUEST;

                Ok(resp)
            }
        } else {
            let host = req.uri().host().expect("uri has no host");
            let port = req.uri().port_u16().unwrap_or(80);
            let addr = format!("{}:{}", host, port);

            // Establish ProxyProtocol
            let upstream = TcpProxyStream::establish(&self.proxy_configs, &addr.into())
                .await
                .inspect_err(|e| {
                    error!(?e, "Failed to establish proxy protocol");
                })?;
            let upstream = upstream.into_inner();

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

            let resp = sender
                .send_request(req)
                .await
                .inspect_err(|e| error!(?e, "Failed to send HTTP/1 request to upstream"))?;
            Ok(resp.map(|b| b.boxed()))
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Proxy protocol error")]
    ProxyProtocolError(#[from] ProxyProtocolError),
    #[error("Hyper error")]
    HyperError(#[from] hyper::Error),
}

#[async_trait]
impl TcpServerHook for HttpProxyAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream(&self, stream: TcpStream) {
        let res = self.proxy(stream).await;
        if let Err(e) = res {
            error!(?e, "Failed to proxy");
        }
    }
}

struct HttpTunnel {
    proxy_configs: Arc<Vec<ProxyConfig>>,
}

impl HttpTunnel {
    pub fn new(proxy_configs: Arc<Vec<ProxyConfig>>) -> Self {
        Self { proxy_configs }
    }

    // Create a TCP connection to host:port, build a tunnel between the connection and
    // the upgraded connection
    #[instrument(skip(self, upgraded))]
    pub async fn tunnel(
        &self,
        mut upgraded: Upgraded,
        addr: String,
    ) -> Result<(), ProxyProtocolError> {
        // Establish ProxyProtocol
        let upstream = TcpProxyStream::establish(&self.proxy_configs, &addr.into())
            .await
            .inspect_err(|e| {
                error!(?e, "Failed to establish proxy protocol");
            })?;
        let mut upstream = upstream.into_inner();

        // Proxying data
        let (from_client, from_server) =
            tokio::io::copy_bidirectional(&mut upgraded, &mut upstream)
                .await
                .inspect_err(|e| {
                    error!(?e, "Failed to copy data");
                })?;

        // Print message when done
        trace!(
            ?from_client,
            ?from_server,
            "Tunnel established, closing connection"
        );

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
