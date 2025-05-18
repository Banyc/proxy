use std::{fmt, str::FromStr};

use common::addr::ParseInternetAddrError;

use super::registry::CONCRETE_STREAM_PROTO;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConcreteStreamType {
    Tcp,
    TcpMux,
    Kcp,
    Mptcp,
    Rtp,
    RtpMux,
    RtpMuxFec,
}
impl fmt::Display for ConcreteStreamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        let (_, ty, _) = CONCRETE_STREAM_PROTO
            .iter()
            .find(|(x, _, _)| x == self)
            .unwrap();
        write!(f, "{ty}")
    }
}
impl FromStr for ConcreteStreamType {
    type Err = ParseInternetAddrError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((ty, _, _)) = CONCRETE_STREAM_PROTO.iter().find(|(_, x, _)| *x == s) else {
            return Err(ParseInternetAddrError);
        };
        Ok(*ty)
    }
}
