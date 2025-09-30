use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    config::SharableConfig,
    loading,
    proto::{
        client::{self, udp::UdpProxyClient},
        conn::udp::{Flow, UpstreamAddr},
        context::UdpContext,
        io_copy::udp::{CopyBidirectional, DownstreamParts, UpstreamParts},
        route::{UdpConnSelector, UdpConnSelectorBuildContext, UdpConnSelectorBuilder},
    },
    route::ConnSelectorBuildError,
    udp::{
        Packet,
        server::{UdpServer, UdpServerHandleConn},
    },
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tracing::warn;
use udp_listener::Conn;

use crate::socks5::messages::UdpRequestHeader;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Socks5ServerUdpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub conn_selector: SharableConfig<UdpConnSelectorBuilder>,
    pub speed_limit: Option<f64>,
}
impl Socks5ServerUdpAccessServerConfig {
    pub fn into_builder(
        self,
        conn_selector: &HashMap<Arc<str>, UdpConnSelector>,
        cx: UdpConnSelectorBuildContext<'_>,
        udp_context: UdpContext,
    ) -> Result<Socks5ServerUdpAccessServerBuilder, BuildError> {
        let conn_selector = match self.conn_selector {
            SharableConfig::SharingKey(key) => conn_selector
                .get(&key)
                .ok_or_else(|| BuildError::ProxyGroupKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(cx)?,
        };

        Ok(Socks5ServerUdpAccessServerBuilder {
            listen_addr: self.listen_addr,
            conn_selector,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            udp_context,
        })
    }
}
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy group key not found: {0}")]
    ProxyGroupKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyGroup(#[from] ConnSelectorBuildError),
}

#[derive(Debug, Clone)]
pub struct Socks5ServerUdpAccessServerBuilder {
    listen_addr: Arc<str>,
    conn_selector: UdpConnSelector,
    speed_limit: f64,
    udp_context: UdpContext,
}
impl loading::Build for Socks5ServerUdpAccessServerBuilder {
    type ConnHandler = Socks5ServerUdpAccessConnHandler;
    type Server = UdpServer<Self::ConnHandler>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_conn_handler()?;
        let server = access.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        Ok(Socks5ServerUdpAccessConnHandler::new(
            self.conn_selector,
            self.speed_limit,
            self.udp_context,
        ))
    }
}

#[derive(Debug)]
pub struct Socks5ServerUdpAccessConnHandler {
    conn_selector: UdpConnSelector,
    speed_limiter: Limiter,
    udp_context: UdpContext,
}
impl loading::HandleConn for Socks5ServerUdpAccessConnHandler {}
impl Socks5ServerUdpAccessConnHandler {
    pub fn new(conn_selector: UdpConnSelector, speed_limit: f64, udp_context: UdpContext) -> Self {
        Self {
            conn_selector,
            speed_limiter: Limiter::new(speed_limit),
            udp_context,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = tokio::net::UdpSocket::bind(listen_addr).await?;
        Ok(UdpServer::new(listener, self))
    }

    async fn proxy(&self, conn: Conn<UdpSocket, Flow, Packet>) -> Result<(), AccessProxyError> {
        // Connect to upstream
        let (chain, payload_crypto) = match &self.conn_selector {
            common::route::ConnSelector::Empty => ([].into(), None),
            common::route::ConnSelector::Some(conn_selector1) => {
                let proxy_chain = conn_selector1.choose_chain();
                (
                    proxy_chain.chain.clone(),
                    proxy_chain.payload_crypto.clone(),
                )
            }
        };
        let flow = conn.conn_key().clone();
        let upstream = UdpProxyClient::establish(
            chain,
            flow.upstream.as_ref().unwrap().0.clone(),
            &self.udp_context,
        )
        .await?;
        let upstream_remote = upstream.remote_addr().clone();

        let (upstream_read, upstream_write) = upstream.into_split();

        let response_header = {
            let destination = flow.upstream.as_ref().unwrap().0.clone();
            move || {
                let mut wtr = Vec::new();
                let udp_request_header = UdpRequestHeader {
                    fragment: 0,
                    destination: destination.clone(),
                };
                futures::executor::block_on(async {
                    udp_request_header.encode(&mut wtr).await.unwrap();
                    wtr.into()
                })
            }
        };

        let speed_limiter = self.speed_limiter.clone();
        let session_table = self.udp_context.session_table.clone();
        let upstream_local = upstream_read.inner().local_addr().ok();
        let (dn_read, dn_write) = conn.split();
        tokio::spawn(async move {
            let io_copy = CopyBidirectional {
                flow,
                upstream: UpstreamParts {
                    read: upstream_read,
                    write: upstream_write,
                },
                downstream: DownstreamParts {
                    read: dn_read,
                    write: dn_write,
                },
                speed_limiter,
                payload_crypto,
                response_header: Some(Box::new(response_header)),
            };
            let _ = io_copy
                .serve_as_access_server(session_table, upstream_local, upstream_remote, "SOCKS UDP")
                .await;
        });
        Ok(())
    }
}
#[derive(Debug, Error)]
pub enum AccessProxyError {
    #[error("Failed to establish proxy chain: {0}")]
    Establish(#[from] client::udp::EstablishError),
}
impl UdpServerHandleConn for Socks5ServerUdpAccessConnHandler {
    fn parse_upstream_addr(&self, buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>> {
        let res = futures::executor::block_on(async move { UdpRequestHeader::decode(buf).await });
        let request_header = match res {
            Ok(header) => header,
            Err(e) => {
                warn!(?e, "Failed to decode UDP request header");
                return None;
            }
        };
        if request_header.fragment != 0 {
            return None;
        }

        Some(Some(UpstreamAddr(request_header.destination)))
    }

    async fn handle_flow(&self, accepted: Conn<UdpSocket, Flow, Packet>) {
        let res = self.proxy(accepted).await;
        match res {
            Ok(()) => (),
            Err(e) => {
                warn!(?e, "Failed to proxy");
            }
        }
    }
}
