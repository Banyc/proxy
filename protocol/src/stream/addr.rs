use std::{fmt, str::FromStr};

use common::addr::ParseInternetAddrError;

use super::protos::STREAM_PROTOS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConcreteStreamType {
    Tcp,
    TcpMux,
    Kcp,
    Mptcp,
    Rtp,
    RtpMux,
}
impl fmt::Display for ConcreteStreamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        let (_, ty, _) = STREAM_PROTOS.iter().find(|(x, _, _)| x == self).unwrap();
        write!(f, "{ty}")
    }
}
impl FromStr for ConcreteStreamType {
    type Err = ParseInternetAddrError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((ty, _, _)) = STREAM_PROTOS.iter().find(|(_, x, _)| *x == s) else {
            return Err(ParseInternetAddrError);
        };
        Ok(*ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtpmux_is_the_only_rtp_mux_protocol() {
        assert_eq!(
            "rtpmux".parse::<ConcreteStreamType>().unwrap(),
            ConcreteStreamType::RtpMux
        );
        assert!("rtpmuxfec".parse::<ConcreteStreamType>().is_err());
    }
}
