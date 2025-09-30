use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    addr::{InternetAddr, InternetAddrStr},
    config::SharableConfig,
    loading,
    proto::{
        client::{self, udp::UdpProxyClient},
        conn::udp::{Flow, UpstreamAddr},
        context::UdpContext,
        io_copy::udp::{CopyBiError, CopyBidirectional, DownstreamParts, UpstreamParts},
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: InternetAddrStr,
    pub conn_selector: SharableConfig<UdpConnSelectorBuilder>,
    pub speed_limit: Option<f64>,
}
impl UdpAccessServerConfig {
    pub fn into_builder(
        self,
        conn_selector: &HashMap<Arc<str>, UdpConnSelector>,
        cx: UdpConnSelectorBuildContext<'_>,
        udp_context: UdpContext,
    ) -> Result<UdpAccessServerBuilder, BuildError> {
        let conn_selector = match self.conn_selector {
            SharableConfig::SharingKey(key) => conn_selector
                .get(&key)
                .ok_or_else(|| BuildError::ProxyGroupKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(cx)?,
        };

        Ok(UdpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
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
pub struct UdpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: InternetAddrStr,
    conn_selector: UdpConnSelector,
    speed_limit: f64,
    udp_context: UdpContext,
}
impl loading::Build for UdpAccessServerBuilder {
    type ConnHandler = UdpAccessConnHandler;
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
        Ok(UdpAccessConnHandler::new(
            self.conn_selector,
            self.destination.0,
            self.speed_limit,
            self.udp_context,
        ))
    }
}

#[derive(Debug)]
pub struct UdpAccessConnHandler {
    conn_selector: UdpConnSelector,
    destination: InternetAddr,
    speed_limiter: Limiter,
    udp_context: UdpContext,
}
impl loading::HandleConn for UdpAccessConnHandler {}
impl UdpAccessConnHandler {
    pub fn new(
        route_table: UdpConnSelector,
        destination: InternetAddr,
        speed_limit: f64,
        udp_context: UdpContext,
    ) -> Self {
        Self {
            conn_selector: route_table,
            destination,
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
        let upstream =
            UdpProxyClient::establish(chain, self.destination.clone(), &self.udp_context).await?;
        let upstream_remote = upstream.remote_addr().clone();

        let (upstream_read, upstream_write) = upstream.into_split();

        let speed_limiter = self.speed_limiter.clone();
        let session_table = self.udp_context.session_table.clone();
        let upstream_local = upstream_read.inner().local_addr().ok();
        let flow = conn.conn_key().clone();
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
                response_header: None,
            };
            let _ = io_copy
                .serve_as_access_server(session_table, upstream_local, upstream_remote, "UDP")
                .await;
        });
        Ok(())
    }
}
#[derive(Debug, Error)]
pub enum AccessProxyError {
    #[error("Failed to establish proxy chain: {0}")]
    Establish(#[from] client::udp::EstablishError),
    #[error("Failed to copy: {0}")]
    Copy(#[from] CopyBiError),
}
impl UdpServerHandleConn for UdpAccessConnHandler {
    fn parse_upstream_addr(&self, _buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>> {
        Some(Some(UpstreamAddr(self.destination.clone())))
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
