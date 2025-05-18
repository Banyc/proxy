use std::{fmt, str::FromStr};

use common::addr::ParseInternetAddrError;

// const TYPE_STR_TABLE: &[(ConcreteStreamType, &str)] = &[
//     (ConcreteStreamType::Tcp, "tcp"),
//     (ConcreteStreamType::TcpMux, "tcpmux"),
//     (ConcreteStreamType::Kcp, "kcp"),
//     (ConcreteStreamType::Mptcp, "mptcp"),
//     (ConcreteStreamType::Rtp, "rtp"),
//     (ConcreteStreamType::RtpMux, "rtpmux"),
//     (ConcreteStreamType::RtpMuxFec, "rtpmuxfec"),
// ];
// static TYPE_TO_STR_MAP: LazyLock<HashMap<ConcreteStreamType, &str>> =
//     LazyLock::new(|| HashMap::from_iter(TYPE_STR_TABLE.iter().map(|(k, v)| (*k, *v))));
// static STR_TO_TYPE_MAP: LazyLock<HashMap<&str, ConcreteStreamType>> =
//     LazyLock::new(|| HashMap::from_iter(TYPE_STR_TABLE.iter().map(|(v, k)| (*k, *v))));

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
        match self {
            ConcreteStreamType::Tcp => write!(f, "tcp"),
            ConcreteStreamType::TcpMux => write!(f, "tcpmux"),
            ConcreteStreamType::Kcp => write!(f, "kcp"),
            ConcreteStreamType::Mptcp => write!(f, "mptcp"),
            ConcreteStreamType::Rtp => write!(f, "rtp"),
            ConcreteStreamType::RtpMux => write!(f, "rtpmux"),
            ConcreteStreamType::RtpMuxFec => write!(f, "rtpmuxfec"),
        }
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
            "rtpmuxfec" => Ok(Self::RtpMuxFec),
            _ => Err(ParseInternetAddrError),
        }
    }
}
