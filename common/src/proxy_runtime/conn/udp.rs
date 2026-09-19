use std::{fmt, net::SocketAddr};

use hdv_derive::HdvSerde;

use crate::{addr::InternetAddrHostPort, proxy_runtime::addr::RouteAddr};

pub const UDP_FLOW_ID_LEN: usize = 16;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UdpFlowId([u8; UDP_FLOW_ID_LEN]);
impl UdpFlowId {
    pub fn random() -> Self {
        Self(rand::random())
    }
    pub(crate) fn from_bytes(bytes: [u8; UDP_FLOW_ID_LEN]) -> Self {
        Self(bytes)
    }
    pub(crate) fn as_bytes(&self) -> &[u8; UDP_FLOW_ID_LEN] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownstreamAddr(pub SocketAddr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpstreamAddr(pub RouteAddr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Flow {
    pub upstream: Option<UpstreamAddr>,
    pub downstream: DownstreamAddr,
}
impl fmt::Display for Flow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(up) = &self.upstream {
            write!(f, "up:{}", up.0)?;
            write!(f, ",")?;
        }
        write!(f, "dn:{}", self.downstream.0)?;
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FlowKey {
    Routed(Flow),
    Identified {
        downstream: DownstreamAddr,
        flow_id: UdpFlowId,
    },
}
impl FlowKey {
    pub fn downstream(&self) -> DownstreamAddr {
        match self {
            Self::Routed(flow) => flow.downstream,
            Self::Identified { downstream, .. } => *downstream,
        }
    }
    pub fn routed_flow(&self) -> Option<&Flow> {
        match self {
            Self::Routed(flow) => Some(flow),
            Self::Identified { .. } => None,
        }
    }
}
#[derive(Debug, Clone, HdvSerde)]
pub struct FlowHdv {
    pub upstream: Option<InternetAddrHostPort>,
    pub downstream: InternetAddrHostPort,
}
impl From<&Flow> for FlowHdv {
    fn from(value: &Flow) -> Self {
        let upstream = value.upstream.as_ref().map(|x| (&x.0.address).into());
        let downstream = value.downstream.0.into();
        Self {
            upstream,
            downstream,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed() -> FlowKey {
        FlowKey::Routed(Flow {
            upstream: None,
            downstream: DownstreamAddr("127.0.0.1:1000".parse().unwrap()),
        })
    }

    fn identified() -> FlowKey {
        FlowKey::Identified {
            downstream: DownstreamAddr("127.0.0.1:2000".parse().unwrap()),
            flow_id: UdpFlowId::from_bytes([7; UDP_FLOW_ID_LEN]),
        }
    }

    /// Both key shapes report their downstream, but only a routed key owns a
    /// flow. Driving both variants pins every arm; a swapped or copied arm
    /// fails here.
    #[test]
    fn a_flow_key_exposes_its_flow_only_when_routed() {
        let routed = routed();
        assert_eq!(
            routed.downstream(),
            DownstreamAddr("127.0.0.1:1000".parse().unwrap())
        );
        assert!(routed.routed_flow().is_some(), "a routed key owns a flow");

        let identified = identified();
        assert_eq!(
            identified.downstream(),
            DownstreamAddr("127.0.0.1:2000".parse().unwrap())
        );
        assert!(
            identified.routed_flow().is_none(),
            "an identified key has no routed flow"
        );
    }
}
