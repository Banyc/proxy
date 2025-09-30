use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::{Deref, RangeInclusive},
    sync::Arc,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::addr::{InternetAddr, InternetAddrKind};

#[derive(Debug, Error)]
pub enum FilterBuildError {
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("Key not found: {0}")]
    KeyNotFound(Arc<str>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatcherBuilder(MatcherBuilderKind);
impl MatcherBuilder {
    pub fn build(self) -> Result<Matcher, regex::Error> {
        self.0.build()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum MatcherBuilderKind {
    Single {
        #[serde(rename = "addr")]
        #[serde(default)]
        addr_matcher: AddrListMatcherBuilder,
        #[serde(rename = "port")]
        #[serde(default)]
        port_matcher: PortListMatcherBuilder,
    },
    OtherMatcher(String),
    Many(Vec<MatcherBuilderKind>),
}
impl MatcherBuilderKind {
    pub fn build(self) -> Result<Matcher, regex::Error> {
        Ok(match self {
            Self::Single {
                addr_matcher,
                port_matcher,
            } => Matcher(MatcherKind::Single(LeafMatcher {
                addr_matcher: addr_matcher.build()?,
                port_matcher: port_matcher.build(),
            })),
            Self::OtherMatcher(matcher) => Matcher(MatcherKind::OtherMatcher(matcher.into())),
            Self::Many(matchers) => Matcher(MatcherKind::Many(
                matchers
                    .into_iter()
                    .map(|matcher| matcher.build().map(|m| m.0))
                    .collect::<Result<_, _>>()?,
            )),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Matcher(MatcherKind);
impl Matcher {
    pub fn matches(
        &self,
        addr: &InternetAddr,
        others: &HashMap<Arc<str>, Matcher>,
        visited: &mut HashSet<Arc<str>>,
    ) -> bool {
        self.0.matches(addr, others, visited)
    }
}

#[derive(Debug, Clone)]
enum MatcherKind {
    Single(LeafMatcher),
    OtherMatcher(Arc<str>),
    Many(Arc<[MatcherKind]>),
}
impl MatcherKind {
    pub fn matches(
        &self,
        addr: &InternetAddr,
        others: &HashMap<Arc<str>, Matcher>,
        visited: &mut HashSet<Arc<str>>,
    ) -> bool {
        match self {
            MatcherKind::Single(leaf_matcher) => leaf_matcher.matches(addr),
            MatcherKind::OtherMatcher(name) => {
                let Some(other) = others.get(name) else {
                    return false;
                };
                if visited.contains(name) {
                    return false;
                }
                visited.insert(name.clone());
                other.matches(addr, others, visited)
            }
            MatcherKind::Many(matcher_kinds) => matcher_kinds
                .iter()
                .any(|x| x.matches(addr, others, visited)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LeafMatcher {
    addr_matcher: AddrListMatcher,
    port_matcher: PortListMatcher,
}
impl LeafMatcher {
    pub fn matches(&self, addr: &InternetAddr) -> bool {
        match addr.deref() {
            InternetAddrKind::SocketAddr(addr) => self.is_match_ip(*addr),
            InternetAddrKind::DomainName { addr, port } => self.is_match_domain_name(addr, *port),
        }
    }
    fn is_match_domain_name(&self, addr: &str, port: u16) -> bool {
        if !self.port_matcher.is_match(port) {
            return false;
        }
        self.addr_matcher.is_match_domain_name(addr)
    }
    fn is_match_ip(&self, addr: SocketAddr) -> bool {
        if !self.port_matcher.is_match(addr.port()) {
            return false;
        }
        self.addr_matcher.is_match_ip(addr.ip())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum AddrListMatcherBuilder {
    Many(Vec<AddrMatcherBuilder>),
    Single(AddrMatcherBuilder),
    #[default]
    Any,
}
impl AddrListMatcherBuilder {
    pub fn build(self) -> Result<AddrListMatcher, regex::Error> {
        Ok(match self {
            Self::Many(matchers) => AddrListMatcher::Some(
                matchers
                    .into_iter()
                    .map(|matcher| matcher.build())
                    .collect::<Result<_, _>>()?,
            ),
            Self::Single(matcher) => AddrListMatcher::Some(vec![matcher.build()?].into()),
            Self::Any => AddrListMatcher::Any,
        })
    }
}

#[derive(Debug, Clone)]
enum AddrListMatcher {
    Some(Arc<[AddrMatcher]>),
    Any,
}
impl AddrListMatcher {
    pub fn is_match_domain_name(&self, addr: &str) -> bool {
        match self {
            Self::Some(matchers) => matchers
                .iter()
                .any(|matcher| matcher.is_match_domain_name(addr)),
            Self::Any => true,
        }
    }

    pub fn is_match_ip(&self, addr: IpAddr) -> bool {
        match self {
            Self::Some(matchers) => matchers.iter().any(|matcher| matcher.is_match_ip(addr)),
            Self::Any => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum AddrMatcherBuilder {
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
    DomainName(String),
    Ipv4Range(RangeInclusive<Ipv4Addr>),
    Ipv6Range(RangeInclusive<Ipv6Addr>),
}
impl AddrMatcherBuilder {
    pub fn build(self) -> Result<AddrMatcher, regex::Error> {
        Ok(match self {
            Self::Ipv4(addr) => AddrMatcher::Ipv4(addr..=addr),
            Self::Ipv6(addr) => AddrMatcher::Ipv6(addr..=addr),
            Self::DomainName(domain_name) => AddrMatcher::DomainName(Regex::new(&domain_name)?),
            Self::Ipv4Range(range) => AddrMatcher::Ipv4(range),
            Self::Ipv6Range(range) => AddrMatcher::Ipv6(range),
        })
    }
}

#[derive(Debug, Clone)]
enum AddrMatcher {
    DomainName(Regex),
    Ipv4(RangeInclusive<Ipv4Addr>),
    Ipv6(RangeInclusive<Ipv6Addr>),
}
impl AddrMatcher {
    pub fn is_match_domain_name(&self, addr: &str) -> bool {
        match self {
            Self::DomainName(regex) => regex.is_match(addr),
            Self::Ipv4(_) => false,
            Self::Ipv6(_) => false,
        }
    }

    pub fn is_match_ip(&self, addr: IpAddr) -> bool {
        match (self, addr) {
            (Self::DomainName(_), _) => false,
            (Self::Ipv4(range), IpAddr::V4(addr)) => range.contains(&addr),
            (Self::Ipv6(range), IpAddr::V6(addr)) => range.contains(&addr),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum PortListMatcherBuilder {
    Many(Vec<PortMatcherBuilder>),
    Single(PortMatcherBuilder),
    #[default]
    Any,
}
impl PortListMatcherBuilder {
    pub fn build(self) -> PortListMatcher {
        match self {
            Self::Many(matchers) => PortListMatcher::Some(
                matchers
                    .into_iter()
                    .map(|matcher| matcher.build())
                    .collect::<_>(),
            ),
            Self::Single(matcher) => PortListMatcher::Some(vec![matcher.build()].into()),
            Self::Any => PortListMatcher::Any,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum PortMatcherBuilder {
    Single(u16),
    Range(RangeInclusive<u16>),
}
impl PortMatcherBuilder {
    pub fn build(self) -> PortMatcher {
        match self {
            Self::Single(port) => PortMatcher(port..=port),
            Self::Range(range) => PortMatcher(range),
        }
    }
}

#[derive(Debug, Clone)]
enum PortListMatcher {
    Some(Arc<[PortMatcher]>),
    Any,
}
impl PortListMatcher {
    pub fn is_match(&self, port: u16) -> bool {
        match self {
            Self::Some(matcher) => matcher.iter().any(|range| range.is_match(port)),
            Self::Any => true,
        }
    }
}

#[derive(Debug, Clone)]
struct PortMatcher(RangeInclusive<u16>);
impl PortMatcher {
    pub fn is_match(&self, port: u16) -> bool {
        self.0.contains(&port)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Proxy,
    Block,
    Direct,
}
