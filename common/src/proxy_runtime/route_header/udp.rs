use std::io::{self, Read, Write};

use ae::anti_replay::{TimeValidator, ValidatorRef};
use metrics::counter;
use tokio::net::UdpSocket;
use tracing::warn;
use udp_listener::ConnWrite;

use crate::{
    header::{
        codec::{CodecError, read_header, write_header},
        route::RouteResponse,
    },
    proxy_runtime::{
        conn::udp::{UDP_FLOW_ID_LEN, UdpFlowId, UpstreamAddr},
        header::UdpRequestHeader,
    },
};

pub async fn echo(
    buf: &[u8],
    dn_writer: &ConnWrite<UdpSocket>,
    header_crypto: &tokio_chacha20::config::Config,
) {
    let resp = RouteResponse { result: Ok(()) };
    let mut wtr = Vec::new();
    write_header(&mut wtr, &resp, *header_crypto.key()).unwrap();
    wtr.write_all(buf).unwrap();
    if let Err(e) = dn_writer.send(&wtr).await {
        warn!(?e, ?dn_writer, "Failed to send response to downstream");
    };
    counter!("udp.echoes").increment(1);
}

const ROUTED_REQUEST: u8 = 0;
const COMPACT_REQUEST: u8 = 1;
impl UdpFlowId {
    pub fn write_routed(self, output: &mut Vec<u8>) {
        output.push(ROUTED_REQUEST);
        output.extend_from_slice(self.as_bytes());
    }
    pub fn write_compact(self, output: &mut Vec<u8>) {
        output.push(COMPACT_REQUEST);
        output.extend_from_slice(self.as_bytes());
    }
    fn read(input: &mut io::Cursor<&[u8]>) -> Result<Self, CodecError> {
        let mut id = [0; UDP_FLOW_ID_LEN];
        input.read_exact(&mut id)?;
        Ok(Self::from_bytes(id))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpRequestRoute {
    Routed {
        flow_id: UdpFlowId,
        upstream: Option<UpstreamAddr>,
    },
    Compact {
        flow_id: UdpFlowId,
    },
}

pub fn decode_request_route(
    buf: &mut io::Cursor<&[u8]>,
    header_crypto: &tokio_chacha20::config::Config,
    time_validator: &TimeValidator,
) -> Result<UdpRequestRoute, CodecError> {
    let mut kind = [0; 1];
    buf.read_exact(&mut kind)?;
    let flow_id = UdpFlowId::read(buf)?;
    match kind[0] {
        ROUTED_REQUEST => {
            let validator = ValidatorRef::Time(time_validator);
            let header: UdpRequestHeader = read_header(buf, *header_crypto.key(), &validator)?;
            Ok(UdpRequestRoute::Routed {
                flow_id,
                upstream: header.upstream.map(UpstreamAddr),
            })
        }
        COMPACT_REQUEST => Ok(UdpRequestRoute::Compact { flow_id }),
        kind => Err(CodecError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported UDP request kind {kind}"),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        anti_replay::{VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
        header::route::RouteRequest,
        proxy_runtime::addr::RouteAddr,
    };

    #[test]
    fn routed_and_compact_requests_share_the_same_flow_id() {
        let id = UdpFlowId::from_bytes([9; UDP_FLOW_ID_LEN]);
        let crypto = tokio_chacha20::config::Config::new([7; tokio_chacha20::KEY_BYTES].into());
        let validator = TimeValidator::new(VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL);

        let mut buf = Vec::new();
        id.write_routed(&mut buf);
        write_header(
            &mut buf,
            &RouteRequest {
                upstream: Some(RouteAddr::udp(
                    "127.0.0.1:9"
                        .parse::<std::net::SocketAddr>()
                        .unwrap()
                        .into(),
                )),
            },
            *crypto.key(),
        )
        .unwrap();
        buf.extend_from_slice(b"first");

        let mut cursor = io::Cursor::new(&buf[..]);
        let route = decode_request_route(&mut cursor, &crypto, &validator).unwrap();
        match route {
            UdpRequestRoute::Routed { flow_id, upstream } => {
                assert_eq!(flow_id, id);
                assert!(upstream.is_some());
            }
            other => panic!("expected a routed request, got {other:?}"),
        }
        assert_eq!(&buf[cursor.position() as usize..], b"first");

        let mut buf = Vec::new();
        id.write_compact(&mut buf);
        buf.extend_from_slice(b"later");

        let mut cursor = io::Cursor::new(&buf[..]);
        let route = decode_request_route(&mut cursor, &crypto, &validator).unwrap();
        match route {
            UdpRequestRoute::Compact { flow_id } => assert_eq!(flow_id, id),
            other => panic!("expected a compact request, got {other:?}"),
        }
        assert_eq!(&buf[cursor.position() as usize..], b"later");
    }

    #[test]
    fn unknown_request_kind_is_rejected() {
        let crypto = tokio_chacha20::config::Config::new([7; tokio_chacha20::KEY_BYTES].into());
        let validator = TimeValidator::new(VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL);
        let mut buf = vec![0x7f];
        buf.extend_from_slice(&[0; UDP_FLOW_ID_LEN]);
        let mut cursor = io::Cursor::new(&buf[..]);
        assert!(decode_request_route(&mut cursor, &crypto, &validator).is_err());
    }
}
