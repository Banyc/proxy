use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};

use async_trait::async_trait;
use common::{
    addr::InternetAddr,
    crypto::XorCrypto,
    filter::{self, Filter},
    loading::Hook,
    stream::{
        addr::{StreamAddr, StreamType},
        copy_bidirectional_with_payload_crypto,
        pool::Pool,
        proxy_table::StreamProxyTable,
        tokio_io, CreatedStream, FailedStreamMetrics, IoAddr, IoStream, StreamMetrics,
        StreamServerHook,
    },
};
use proxy_client::stream::StreamEstablishError;
use thiserror::Error;
use tracing::{info, warn};

use crate::socks5::messages::{
    Command, MethodIdentifier, NegotiationRequest, NegotiationResponse, RelayRequest,
    RelayResponse, Reply,
};

pub struct Socks5ServerTcpAccess {
    proxy_table: StreamProxyTable,
    stream_pool: Pool,
    filter: Filter,
    speed_limit: f64,
    udp_listen_addr: Option<InternetAddr>,
}

impl Hook for Socks5ServerTcpAccess {}

#[async_trait]
impl StreamServerHook for Socks5ServerTcpAccess {
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let res = self.proxy(stream).await;
        match res {
            Ok(Some(metrics)) => {
                info!(%metrics, "Proxy finished");
            }
            Ok(None) => (),
            Err(ProxyError::IoCopy { source: e, metrics }) => {
                info!(?e, %metrics, "Proxy error");
            }
            Err(e) => warn!(?e, "Failed to proxy"),
        }
    }
}

impl Socks5ServerTcpAccess {
    async fn proxy<S>(&self, downstream: S) -> Result<Option<StreamMetrics>, ProxyError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream.peer_addr().map_err(ProxyError::DownstreamAddr)?;

        let res = self.establish(downstream).await?;
        let (downstream, (upstream, upstream_addr, upstream_sock_addr), payload_crypto) = match res
        {
            EstablishResult::Direct {
                downstream,
                upstream,
                upstream_addr,
            } => {
                match tokio_io::timed_copy_bidirectional(downstream, upstream, self.speed_limit)
                    .await
                {
                    Ok(metrics) => {
                        info!(%upstream_addr, ?metrics, "Direct finished");
                    }
                    Err(e) => {
                        info!(?e, %upstream_addr, "Direct error");
                    }
                }
                return Ok(None);
            }
            EstablishResult::Udp => return Ok(None),
            EstablishResult::Proxy {
                downstream,
                upstream,
                payload_crypto,
            } => (downstream, upstream, payload_crypto),
        };

        let res = copy_bidirectional_with_payload_crypto(
            downstream,
            upstream,
            payload_crypto.as_ref(),
            self.speed_limit,
        )
        .await;
        let end = std::time::Instant::now();
        let (bytes_uplink, bytes_downlink) = res.map_err(|e| ProxyError::IoCopy {
            source: e,
            metrics: FailedStreamMetrics {
                start,
                end,
                upstream_addr: upstream_addr.clone(),
                upstream_sock_addr,
                downstream_addr: Some(downstream_addr),
            },
        })?;

        let metrics = StreamMetrics {
            start,
            end,
            bytes_uplink,
            bytes_downlink,
            upstream_addr,
            upstream_sock_addr,
            downstream_addr: Some(downstream_addr),
        };
        Ok(Some(metrics))
    }

    async fn establish<S>(&self, stream: S) -> Result<EstablishResult<S>, EstablishError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let (mut stream, relay_request) = steer(stream).await?;

        // Filter
        let destination_str = relay_request.destination.to_string();
        let action = self.filter.filter(&destination_str);
        if matches!(action, filter::Action::Block) {
            let relay_response = RelayResponse {
                reply: Reply::ConnectionNotAllowedByRuleset,
                bind: InternetAddr::SocketAddr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
            };
            relay_response.encode(&mut stream).await?;
            return Err(io::Error::new(io::ErrorKind::Other, "Blocked by filter").into());
        }

        match relay_request.command {
            Command::Connect => (),
            Command::Bind => {
                let relay_response = RelayResponse {
                    reply: Reply::CommandNotSupported,
                    bind: InternetAddr::SocketAddr(
                        SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(),
                    ),
                };
                relay_response.encode(&mut stream).await?;
                return Err(
                    io::Error::new(io::ErrorKind::Other, "Command BIND not supported").into(),
                );
            }
            Command::UdpAssociate => match &self.udp_listen_addr {
                Some(addr) => {
                    let relay_response = RelayResponse {
                        reply: Reply::Succeeded,
                        bind: addr.clone(),
                    };
                    relay_response.encode(&mut stream).await?;
                    return Ok(EstablishResult::Udp);
                }
                None => {
                    let relay_response = RelayResponse {
                        reply: Reply::CommandNotSupported,
                        bind: InternetAddr::SocketAddr(
                            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(),
                        ),
                    };
                    relay_response.encode(&mut stream).await?;
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Command UDP ASSOCIATE not supported",
                    )
                    .into());
                }
            },
        }

        if matches!(action, filter::Action::Direct) {
            let upstream = tokio::net::TcpStream::connect(&destination_str).await?;
            let relay_response = RelayResponse {
                reply: Reply::Succeeded,
                bind: InternetAddr::SocketAddr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
            };
            relay_response.encode(&mut stream).await?;
            return Ok(EstablishResult::Direct {
                downstream: stream,
                upstream,
                upstream_addr: relay_request.destination,
            });
        }

        let (upstream, payload_crypto) =
            match self.establish_proxy_chain(relay_request.destination).await {
                Ok(res) => res,
                Err(e) => {
                    let relay_response = RelayResponse {
                        reply: Reply::GeneralSocksServerFailure,
                        bind: InternetAddr::SocketAddr(
                            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(),
                        ),
                    };
                    relay_response.encode(&mut stream).await?;
                    return Err(e.into());
                }
            };
        let relay_response = RelayResponse {
            reply: Reply::Succeeded,
            bind: InternetAddr::SocketAddr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into()),
        };
        relay_response.encode(&mut stream).await?;
        Ok(EstablishResult::Proxy {
            downstream: stream,
            upstream,
            payload_crypto,
        })
    }

    async fn establish_proxy_chain(
        &self,
        destination: InternetAddr,
    ) -> Result<((CreatedStream, StreamAddr, SocketAddr), Option<XorCrypto>), StreamEstablishError>
    {
        let proxy_chain = self.proxy_table.choose_chain();
        let res = proxy_client::stream::establish(
            &proxy_chain.chain,
            StreamAddr {
                address: destination,
                stream_type: StreamType::Tcp,
            },
            &self.stream_pool,
        )
        .await?;
        Ok((res, proxy_chain.payload_crypto.clone()))
    }
}

pub enum EstablishResult<S> {
    Direct {
        downstream: S,
        upstream: tokio::net::TcpStream,
        upstream_addr: InternetAddr,
    },
    Udp,
    Proxy {
        downstream: S,
        upstream: (CreatedStream, StreamAddr, SocketAddr),
        payload_crypto: Option<XorCrypto>,
    },
}

#[derive(Debug, Error)]
pub enum EstablishError {
    #[error("IO error")]
    Io(#[from] io::Error),
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamEstablishError),
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to establish connection")]
    Establish(#[from] EstablishError),
    #[error("Failed to get downstream address")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to copy data between streams")]
    IoCopy {
        #[source]
        source: tokio_io::CopyBiError,
        metrics: FailedStreamMetrics,
    },
}

async fn steer<S>(mut stream: S) -> io::Result<(S, RelayRequest)>
where
    S: IoStream + IoAddr + std::fmt::Debug,
{
    let negotiation_request = NegotiationRequest::decode(&mut stream).await?;
    if !negotiation_request
        .methods
        .contains(&MethodIdentifier::NoAuth)
    {
        let negotiation_response = NegotiationResponse { method: None };
        negotiation_response.encode(&mut stream).await?;
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "No auth method supported",
        ));
    }
    let negotiation_response = NegotiationResponse {
        method: Some(MethodIdentifier::NoAuth),
    };
    negotiation_response.encode(&mut stream).await?;

    let relay_request = RelayRequest::decode(&mut stream).await?;

    Ok((stream, relay_request))
}
