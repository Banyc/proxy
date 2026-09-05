use std::{fmt, str::FromStr};

use common::addr::ParseInternetAddrError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConcreteStreamType {
    Tcp,
    TcpMux,
    Kcp,
    Mptcp,
    Rtp,
    RtpMux,
}
impl ConcreteStreamType {
    /// The wire protocol name, e.g. `"rtpmux"`. The rtp/rtpmux variants are
    /// not part of [`super::protos::STREAM_PROTOS`] (which only carries the
    /// non-rtp builders), so the mapping is explicit here.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::TcpMux => "tcpmux",
            Self::Kcp => "kcp",
            Self::Mptcp => "mptcp",
            Self::Rtp => "rtp",
            Self::RtpMux => "rtpmux",
        }
    }
}
impl fmt::Display for ConcreteStreamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
impl FromStr for ConcreteStreamType {
    type Err = ParseInternetAddrError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tcp" => Ok(Self::Tcp),
            "tcpmux" => Ok(Self::TcpMux),
            "kcp" => Ok(Self::Kcp),
            "mptcp" => Ok(Self::Mptcp),
            "rtp" => Ok(Self::Rtp),
            "rtpmux" => Ok(Self::RtpMux),
            _ => Err(ParseInternetAddrError),
        }
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
