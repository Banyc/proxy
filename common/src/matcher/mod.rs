//! Address/port matchers used by access-server route tables and listener
//! `conn_selector` bindings.
//!
//! A [`Matcher`] is built from a `MatcherBuilderKind` that may be a single
//! `addr`/`port` predicate, a named reference to another matcher, or a list
//! of matchers (all-of). The resulting matcher tests a destination address and
//! port, supporting IPv4/IPv6 literals, ranges, and domain-name regexes.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::{Deref, RangeInclusive},
    sync::Arc,
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::addr::{InternetAddr, InternetAddrKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate) enum MatcherBuilderKind {
    Single {
        #[serde(rename = "addr")]
        #[serde(default)]
        addr_matcher: AddrListMatcherBuilder,
        #[serde(rename = "port")]
        #[serde(default)]
        port_matcher: PortListMatcherBuilder,
    },
    NamedRef(String),
    Many(Vec<MatcherBuilderKind>),
}
impl MatcherBuilderKind {
    fn build(self) -> Result<Matcher, regex::Error> {
        Ok(match self {
            Self::Single {
                addr_matcher,
                port_matcher,
            } => Matcher(MatcherKind::Single(LeafMatcher {
                addr_matcher: addr_matcher.build()?,
                port_matcher: port_matcher.build(),
            })),
            Self::NamedRef(matcher) => Matcher(MatcherKind::NamedRef(matcher.into())),
            Self::Many(matchers) => Matcher(MatcherKind::Many(
                matchers
                    .into_iter()
                    .map(|matcher| matcher.build().map(|m| m.0))
                    .collect::<Result<_, _>>()?,
            )),
        })
    }
}
impl From<&Matcher> for MatcherBuilderKind {
    fn from(matcher: &Matcher) -> Self {
        match &matcher.0 {
            MatcherKind::Single(leaf) => MatcherBuilderKind::Single {
                addr_matcher: (&leaf.addr_matcher).into(),
                port_matcher: (&leaf.port_matcher).into(),
            },
            MatcherKind::NamedRef(name) => MatcherBuilderKind::NamedRef(name.to_string()),
            MatcherKind::Many(kinds) => MatcherBuilderKind::Many(
                kinds
                    .iter()
                    .map(|k| MatcherBuilderKind::from(&Matcher(k.clone())))
                    .collect(),
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "MatcherBuilderKind")]
#[serde(into = "MatcherBuilderKind")]
pub struct Matcher(MatcherKind);
impl Matcher {
    pub fn matches(
        &self,
        addr: &InternetAddr,
        registry: &HashMap<Arc<str>, Matcher>,
        cycle_guard: &mut HashSet<Arc<str>>,
    ) -> bool {
        self.0.matches(addr, registry, cycle_guard)
    }
}
impl TryFrom<MatcherBuilderKind> for Matcher {
    type Error = regex::Error;
    fn try_from(kind: MatcherBuilderKind) -> Result<Self, Self::Error> {
        kind.build()
    }
}
impl From<Matcher> for MatcherBuilderKind {
    fn from(matcher: Matcher) -> Self {
        (&matcher).into()
    }
}

#[derive(Debug, Clone)]
enum MatcherKind {
    Single(LeafMatcher),
    NamedRef(Arc<str>),
    Many(Arc<[MatcherKind]>),
}
impl MatcherKind {
    pub fn matches(
        &self,
        addr: &InternetAddr,
        registry: &HashMap<Arc<str>, Matcher>,
        cycle_guard: &mut HashSet<Arc<str>>,
    ) -> bool {
        match self {
            MatcherKind::Single(leaf_matcher) => leaf_matcher.matches(addr),
            MatcherKind::NamedRef(name) => {
                let Some(other) = registry.get(name) else {
                    return false;
                };
                if cycle_guard.contains(name) {
                    return false;
                }
                cycle_guard.insert(name.clone());
                other.matches(addr, registry, cycle_guard)
            }
            MatcherKind::Many(matcher_kinds) => matcher_kinds
                .iter()
                .any(|x| x.matches(addr, registry, cycle_guard)),
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
        if self.addr_matcher.is_match_ip(addr.ip()) {
            return true;
        }
        match addr.ip() {
            IpAddr::V4(_) => false,
            IpAddr::V6(ip) => ip
                .to_ipv4_mapped()
                .is_some_and(|ip| self.addr_matcher.is_match_ip(ip.into())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate) enum AddrListMatcherBuilder {
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
impl From<&AddrListMatcher> for AddrListMatcherBuilder {
    fn from(matcher: &AddrListMatcher) -> Self {
        match matcher {
            AddrListMatcher::Some(matchers) => {
                let mut iter = matchers.iter();
                let first = iter.next();
                let Some(first) = first else {
                    return AddrListMatcherBuilder::Any;
                };
                let mut rest = Vec::new();
                for m in iter {
                    rest.push(AddrMatcherBuilder::from(m));
                }
                if rest.is_empty() {
                    AddrListMatcherBuilder::Single(AddrMatcherBuilder::from(first))
                } else {
                    let mut out = vec![AddrMatcherBuilder::from(first)];
                    out.extend(rest);
                    AddrListMatcherBuilder::Many(out)
                }
            }
            AddrListMatcher::Any => AddrListMatcherBuilder::Any,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum AddrListMatcher {
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
pub(crate) enum AddrMatcherBuilder {
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
            Self::Ipv6(addr) => match addr.to_ipv4_mapped() {
                Some(addr) => AddrMatcher::Ipv4(addr..=addr),
                None => AddrMatcher::Ipv6(addr..=addr),
            },
            Self::DomainName(domain_name) => AddrMatcher::DomainName(Regex::new(&domain_name)?),
            Self::Ipv4Range(range) => AddrMatcher::Ipv4(range),
            Self::Ipv6Range(range) => {
                match (range.start().to_ipv4_mapped(), range.end().to_ipv4_mapped()) {
                    (Some(start), Some(end)) => AddrMatcher::Ipv4(start..=end),
                    _ => AddrMatcher::Ipv6(range),
                }
            }
        })
    }
}
impl From<&AddrMatcher> for AddrMatcherBuilder {
    fn from(matcher: &AddrMatcher) -> Self {
        match matcher {
            AddrMatcher::DomainName(regex) => {
                AddrMatcherBuilder::DomainName(regex.as_str().to_owned())
            }
            AddrMatcher::Ipv4(range) => AddrMatcherBuilder::Ipv4Range(range.clone()),
            AddrMatcher::Ipv6(range) => AddrMatcherBuilder::Ipv6Range(range.clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum AddrMatcher {
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
pub(crate) enum PortListMatcherBuilder {
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
impl From<&PortListMatcher> for PortListMatcherBuilder {
    fn from(matcher: &PortListMatcher) -> Self {
        match matcher {
            PortListMatcher::Some(matchers) => {
                let mut iter = matchers.iter();
                let first = iter.next();
                let Some(first) = first else {
                    return PortListMatcherBuilder::Any;
                };
                let mut rest = Vec::new();
                for m in iter {
                    rest.push(PortMatcherBuilder::from(m));
                }
                if rest.is_empty() {
                    PortListMatcherBuilder::Single(PortMatcherBuilder::from(first))
                } else {
                    let mut out = vec![PortMatcherBuilder::from(first)];
                    out.extend(rest);
                    PortListMatcherBuilder::Many(out)
                }
            }
            PortListMatcher::Any => PortListMatcherBuilder::Any,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate) enum PortMatcherBuilder {
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
impl From<&PortMatcher> for PortMatcherBuilder {
    fn from(matcher: &PortMatcher) -> Self {
        PortMatcherBuilder::Range(matcher.0.clone())
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PortListMatcher {
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
pub(crate) struct PortMatcher(RangeInclusive<u16>);
impl PortMatcher {
    pub fn is_match(&self, port: u16) -> bool {
        self.0.contains(&port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(json: &str) -> Matcher {
        serde_json::from_str(json).unwrap()
    }

    fn matches(m: &Matcher, s: &str) -> bool {
        let addr: InternetAddr = s.parse().unwrap();
        m.matches(&addr, &HashMap::new(), &mut HashSet::new())
    }

    #[test]
    fn a_rule_on_an_ipv4_range_still_catches_the_ipv6_spelling() {
        let m = matcher(r#"{"addr": {"start": "10.0.0.0", "end": "10.255.255.255"}}"#);
        assert!(matches(&m, "10.0.0.1:80"));
        assert!(matches(&m, "[::ffff:10.0.0.1]:80"));
        assert!(!matches(&m, "1.1.1.1:80"));
        assert!(!matches(&m, "[::ffff:1.1.1.1]:80"));
    }
    #[test]
    fn a_rule_written_in_ipv6_keeps_matching() {
        let m = matcher(r#"{"addr": "::ffff:10.0.0.1"}"#);
        assert!(matches(&m, "[::ffff:10.0.0.1]:80"));
        assert!(!matches(&m, "[::ffff:10.0.0.2]:80"));
    }
    #[test]
    fn the_port_still_has_to_match() {
        let m = matcher(r#"{"addr": {"start": "10.0.0.0", "end": "10.255.255.255"}, "port": 80}"#);
        assert!(matches(&m, "[::ffff:10.0.0.1]:80"));
        assert!(!matches(&m, "[::ffff:10.0.0.1]:443"));
    }
    #[test]
    fn a_rule_spelled_in_the_mapped_form_catches_plain_ipv4() {
        let m = matcher(r#"{"addr": "::ffff:10.0.0.1"}"#);
        assert!(matches(&m, "10.0.0.1:80"));
        assert!(!matches(&m, "10.0.0.2:80"));
        let m =
            matcher(r#"{"addr": {"start": "::ffff:10.0.0.0", "end": "::ffff:10.255.255.255"}}"#);
        assert!(matches(&m, "10.0.0.1:80"));
        assert!(matches(&m, "[::ffff:10.0.0.1]:80"));
        assert!(!matches(&m, "1.1.1.1:80"));
    }
    #[test]
    fn a_range_that_only_straddles_the_mapped_block() {
        let m = matcher(r#"{"addr": {"start": "::", "end": "ffff::"}}"#);
        assert!(matches(&m, "[::ffff:10.0.0.1]:80"));
        assert!(!matches(&m, "10.0.0.1:80"));
    }

    #[test]
    fn a_matcher_round_trips_through_json() {
        let m = matcher(r#"{"addr": {"start": "10.0.0.0", "end": "10.255.255.255"}, "port": 80}"#);
        let json = serde_json::to_string(&m).unwrap();
        let m2: Matcher = serde_json::from_str(&json).unwrap();
        assert!(matches(&m2, "10.0.0.1:80"));
        assert!(!matches(&m2, "10.0.0.1:443"));
    }
}
