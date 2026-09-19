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

    /// Every concrete stream type must own one wire name, and that name must
    /// parse back to the same variant. The table drives one row per variant
    /// so a copied name or a dropped parse arm fails here.
    #[test]
    fn every_concrete_stream_type_round_trips_through_its_wire_name() {
        let cases = [
            (ConcreteStreamType::Tcp, "tcp"),
            (ConcreteStreamType::TcpMux, "tcpmux"),
            (ConcreteStreamType::Kcp, "kcp"),
            (ConcreteStreamType::Mptcp, "mptcp"),
            (ConcreteStreamType::Rtp, "rtp"),
            (ConcreteStreamType::RtpMux, "rtpmux"),
        ];
        for (ty, wire) in cases {
            assert_eq!(ty.as_str(), wire, "{ty:?} label");
            assert_eq!(ty.to_string(), wire, "{ty:?} display");
            assert_eq!(wire.parse::<ConcreteStreamType>().unwrap(), ty, "{wire}");
        }
    }

    #[test]
    fn rtpmux_is_the_only_rtp_mux_protocol() {
        assert_eq!(
            "rtpmux".parse::<ConcreteStreamType>().unwrap(),
            ConcreteStreamType::RtpMux
        );
        assert!("rtpmuxfec".parse::<ConcreteStreamType>().is_err());
    }
}
