use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::RangeInclusive,
    sync::Arc,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{addr::InternetAddr, config::SharableConfig};

pub fn build_from_map(
    builders: HashMap<Arc<str>, FilterBuilder>,
) -> Result<HashMap<Arc<str>, Filter>, FilterBuildError> {
    let mut rounds = 0;
    let mut filters = HashMap::new();
    let mut last_key_not_found = None;
    while filters.len() < builders.len() {
        if rounds >= builders.len() {
            return Err(FilterBuildError::KeyNotFound(last_key_not_found.unwrap()));
        }
        rounds += 1;

        for (k, v) in &builders {
            let v = match v.clone().build(&filters) {
                Ok(v) => v,
                Err(FilterBuildError::KeyNotFound(key)) => {
                    last_key_not_found = Some(key.clone());
                    continue;
                }
                Err(e) => return Err(e),
            };
            filters.insert(k.clone(), v);
        }
    }
    Ok(filters)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(transparent)]
pub struct FilterBuilder {
    pub rules: Option<Vec<SharableConfig<RuleBuilder>>>,
}

impl FilterBuilder {
    pub fn build(self, filters: &HashMap<Arc<str>, Filter>) -> Result<Filter, FilterBuildError> {
        let mut match_acts = Vec::new();
        if let Some(self_match_acts) = self.rules {
            for match_act in self_match_acts {
                match match_act {
                    SharableConfig::SharingKey(key) => {
                        match_acts.extend(
                            filters
                                .get(&key)
                                .ok_or_else(|| FilterBuildError::KeyNotFound(key.clone()))?
                                .rules
                                .iter()
                                .cloned(),
                        );
                    }
                    SharableConfig::Private(match_act) => {
                        match_acts.push(match_act.build()?);
                    }
                }
            }
        }
        Ok(Filter {
            rules: match_acts.into(),
        })
    }
}

#[derive(Debug, Error)]
pub enum FilterBuildError {
    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("Key not found: {0}")]
    KeyNotFound(Arc<str>),
}

#[derive(Debug, Clone)]
pub struct Filter {
    rules: Arc<[Rule]>,
}

impl Filter {
    pub fn filter(&self, addr: &InternetAddr) -> Action {
        match addr {
            InternetAddr::SocketAddr(addr) => self.filter_ip(*addr),
            InternetAddr::String(addr) => {
                let (domain_name, port) = addr.split_once(':').unwrap();
                let port = port.parse::<u16>().unwrap();
                self.filter_domain_name(domain_name, port)
            }
        }
    }

    pub fn filter_domain_name(&self, domain_name: &str, port: u16) -> Action {
        for match_act in self.rules.as_ref() {
            if match_act.matcher.is_match_domain_name(domain_name, port) {
                return match_act.action;
            }
        }
        Action::Proxy
    }

    pub fn filter_ip(&self, addr: SocketAddr) -> Action {
        for match_act in self.rules.as_ref() {
            if match_act.matcher.is_match_ip(addr) {
                return match_act.action;
            }
        }
        Action::Proxy
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleBuilder {
    matcher: MatcherBuilder,
    action: Action,
}

impl RuleBuilder {
    pub fn build(self) -> Result<Rule, regex::Error> {
        Ok(Rule {
            matcher: self.matcher.build()?,
            action: self.action,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    matcher: Matcher,
    action: Action,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
// #[serde(rename_all = "snake_case")]
#[serde(untagged)]
enum MatcherBuilder {
    Single {
        #[serde(rename = "addr")]
        #[serde(default)]
        addr_matcher: AddrMatcherBuilder,
        #[serde(rename = "port")]
        #[serde(default)]
        port_matcher: PortMatcherBuilder,
    },
    Many(Vec<MatcherBuilder>),
}

impl MatcherBuilder {
    pub fn build(self) -> Result<Matcher, regex::Error> {
        Ok(match self {
            MatcherBuilder::Single {
                addr_matcher,
                port_matcher,
            } => Matcher::Single {
                addr_matcher: addr_matcher.build()?,
                port_matcher: port_matcher.build(),
            },
            MatcherBuilder::Many(matchers) => Matcher::Many(
                matchers
                    .into_iter()
                    .map(|matcher| matcher.build())
                    .collect::<Result<_, _>>()?,
            ),
        })
    }
}

#[derive(Debug, Clone)]
enum Matcher {
    Single {
        addr_matcher: AddrMatcher,
        port_matcher: PortMatcher,
    },
    Many(Arc<[Matcher]>),
}

impl Matcher {
    pub fn is_match_domain_name(&self, addr: &str, port: u16) -> bool {
        match self {
            Matcher::Single {
                addr_matcher,
                port_matcher,
            } => {
                if !port_matcher.is_match(port) {
                    return false;
                }
                addr_matcher.is_match_domain_name(addr)
            }
            Matcher::Many(matchers) => matchers
                .iter()
                .any(|matcher| matcher.is_match_domain_name(addr, port)),
        }
    }

    pub fn is_match_ip(&self, addr: SocketAddr) -> bool {
        match self {
            Matcher::Single {
                addr_matcher,
                port_matcher,
            } => {
                if !port_matcher.is_match(addr.port()) {
                    return false;
                }
                addr_matcher.is_match_ip(addr.ip())
            }
            Matcher::Many(matchers) => matchers.iter().any(|matcher| matcher.is_match_ip(addr)),
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
    Ipv4Ranges(Arc<[RangeInclusive<Ipv4Addr>]>),
    Ipv6Ranges(Arc<[RangeInclusive<Ipv6Addr>]>),
    Any,
}

impl AddrMatcherBuilder {
    pub fn build(self) -> Result<AddrMatcher, regex::Error> {
        Ok(match self {
            AddrMatcherBuilder::Ipv4(addr) => AddrMatcher::Ipv4(vec![addr..=addr].into()),
            AddrMatcherBuilder::Ipv6(addr) => AddrMatcher::Ipv6(vec![addr..=addr].into()),
            AddrMatcherBuilder::DomainName(domain_name) => {
                AddrMatcher::DomainName(Regex::new(&domain_name)?)
            }
            AddrMatcherBuilder::Ipv4Range(range) => AddrMatcher::Ipv4(vec![range].into()),
            AddrMatcherBuilder::Ipv6Range(range) => AddrMatcher::Ipv6(vec![range].into()),
            AddrMatcherBuilder::Ipv4Ranges(ranges) => AddrMatcher::Ipv4(ranges),
            AddrMatcherBuilder::Ipv6Ranges(ranges) => AddrMatcher::Ipv6(ranges),
            AddrMatcherBuilder::Any => AddrMatcher::Any,
        })
    }
}

impl Default for AddrMatcherBuilder {
    fn default() -> Self {
        Self::Any
    }
}

#[derive(Debug, Clone)]
enum AddrMatcher {
    DomainName(Regex),
    Ipv4(Arc<[RangeInclusive<Ipv4Addr>]>),
    Ipv6(Arc<[RangeInclusive<Ipv6Addr>]>),
    Any,
}

impl AddrMatcher {
    pub fn is_match_domain_name(&self, addr: &str) -> bool {
        match self {
            AddrMatcher::DomainName(regex) => regex.is_match(addr),
            AddrMatcher::Ipv4(_) => false,
            AddrMatcher::Ipv6(_) => false,
            AddrMatcher::Any => true,
        }
    }

    pub fn is_match_ip(&self, addr: IpAddr) -> bool {
        match (self, addr) {
            (AddrMatcher::DomainName(_), _) => false,
            (AddrMatcher::Ipv4(ranges), IpAddr::V4(addr)) => {
                ranges.iter().any(|range| range.contains(&addr))
            }
            (AddrMatcher::Ipv6(ranges), IpAddr::V6(addr)) => {
                ranges.iter().any(|range| range.contains(&addr))
            }
            (AddrMatcher::Any, _) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum PortMatcherBuilder {
    Single(u16),
    Range(RangeInclusive<u16>),
    Ranges(Arc<[RangeInclusive<u16>]>),
    Any,
}

impl PortMatcherBuilder {
    pub fn build(self) -> PortMatcher {
        match self {
            PortMatcherBuilder::Single(port) => PortMatcher::Ranges(vec![port..=port].into()),
            PortMatcherBuilder::Range(range) => PortMatcher::Ranges(vec![range].into()),
            PortMatcherBuilder::Ranges(ranges) => PortMatcher::Ranges(ranges),
            PortMatcherBuilder::Any => PortMatcher::Any,
        }
    }
}

impl Default for PortMatcherBuilder {
    fn default() -> Self {
        Self::Any
    }
}

#[derive(Debug, Clone)]
enum PortMatcher {
    Ranges(Arc<[RangeInclusive<u16>]>),
    Any,
}

impl PortMatcher {
    pub fn is_match(&self, port: u16) -> bool {
        match self {
            PortMatcher::Ranges(ranges) => ranges.iter().any(|range| range.contains(&port)),
            PortMatcher::Any => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Proxy,
    Block,
    Direct,
}
