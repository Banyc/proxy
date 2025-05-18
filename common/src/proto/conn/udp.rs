use std::net::SocketAddr;

use hdv_derive::HdvSerde;

use crate::addr::{InternetAddr, InternetAddrHdv};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownstreamAddr(pub SocketAddr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpstreamAddr(pub InternetAddr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Flow {
    pub upstream: Option<UpstreamAddr>,
    pub downstream: DownstreamAddr,
}
#[derive(Debug, Clone, HdvSerde)]
pub struct FlowHdv {
    pub upstream: Option<InternetAddrHdv>,
    pub downstream: InternetAddrHdv,
}
impl From<&Flow> for FlowHdv {
    fn from(value: &Flow) -> Self {
        let upstream = value.upstream.as_ref().map(|x| (&x.0).into());
        let downstream = value.downstream.0.into();
        Self {
            upstream,
            downstream,
        }
    }
}
