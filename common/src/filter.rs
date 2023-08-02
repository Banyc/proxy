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
    matchers: HashMap<Arc<str>, MatcherBuilder>,
    builders: HashMap<Arc<str>, FilterBuilder>,
) -> Result<HashMap<Arc<str>, Filter>, FilterBuildError> {
    let matchers = {
        let mut built = HashMap::new();
        for (k, v) in matchers {
            let v = v.build()?;
            built.insert(k, v);
        }
        built
    };

    let mut rounds = 0;
    let mut filters = HashMap::new();
    let mut last_key_not_found = None;
    while filters.len() < builders.len() {
        if rounds >= builders.len() {
            return Err(FilterBuildError::KeyNotFound(last_key_not_found.unwrap()));
        }
        rounds += 1;

        for (k, v) in &builders {
            let v = match v.clone().build(&filters, &matchers) {
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
    pub fn build(
        self,
        filters: &HashMap<Arc<str>, Filter>,
        matchers: &HashMap<Arc<str>, Matcher>,
    ) -> Result<Filter, FilterBuildError> {
        let mut rules = Vec::new();
        if let Some(self_match_acts) = self.rules {
            for rule in self_match_acts {
                match rule {
                    SharableConfig::SharingKey(key) => {
                        rules.extend(
                            filters
                                .get(&key)
                                .ok_or_else(|| FilterBuildError::KeyNotFound(key.clone()))?
                                .rules
                                .iter()
                                .cloned(),
                        );
                    }
                    SharableConfig::Private(rule) => {
                        rules.push(rule.build(matchers)?);
                    }
                }
            }
        }
        Ok(Filter {
            rules: rules.into(),
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
            if match_act.matcher.0.is_match_domain_name(domain_name, port) {
                return match_act.action;
            }
        }
        Action::Proxy
    }

    pub fn filter_ip(&self, addr: SocketAddr) -> Action {
        for match_act in self.rules.as_ref() {
            if match_act.matcher.0.is_match_ip(addr) {
                return match_act.action;
            }
        }
        Action::Proxy
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleBuilder {
    matcher: SharableConfig<MatcherBuilder>,
    action: Action,
}

impl RuleBuilder {
    pub fn build(self, matchers: &HashMap<Arc<str>, Matcher>) -> Result<Rule, FilterBuildError> {
        let matcher = match self.matcher {
            SharableConfig::SharingKey(k) => matchers
                .get(&k)
                .ok_or_else(|| FilterBuildError::KeyNotFound(Arc::clone(&k)))?
                .clone(),
            SharableConfig::Private(matcher) => matcher.build()?,
        };
        Ok(Rule {
            matcher,
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
    Many(Vec<MatcherBuilderKind>),
}

impl MatcherBuilderKind {
    pub fn build(self) -> Result<Matcher, regex::Error> {
        Ok(match self {
            Self::Single {
                addr_matcher,
                port_matcher,
            } => Matcher(MatcherKind::Single {
                addr_matcher: addr_matcher.build()?,
                port_matcher: port_matcher.build(),
            }),
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

#[derive(Debug, Clone)]
enum MatcherKind {
    Single {
        addr_matcher: AddrListMatcher,
        port_matcher: PortListMatcher,
    },
    Many(Arc<[MatcherKind]>),
}

impl MatcherKind {
    pub fn is_match_domain_name(&self, addr: &str, port: u16) -> bool {
        match self {
            MatcherKind::Single {
                addr_matcher,
                port_matcher,
            } => {
                if !port_matcher.is_match(port) {
                    return false;
                }
                addr_matcher.is_match_domain_name(addr)
            }
            MatcherKind::Many(matchers) => matchers
                .iter()
                .any(|matcher| matcher.is_match_domain_name(addr, port)),
        }
    }

    pub fn is_match_ip(&self, addr: SocketAddr) -> bool {
        match self {
            MatcherKind::Single {
                addr_matcher,
                port_matcher,
            } => {
                if !port_matcher.is_match(addr.port()) {
                    return false;
                }
                addr_matcher.is_match_ip(addr.ip())
            }
            MatcherKind::Many(matchers) => matchers.iter().any(|matcher| matcher.is_match_ip(addr)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum AddrListMatcherBuilder {
    Many(Vec<AddrMatcherBuilder>),
    Single(AddrMatcherBuilder),
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

impl Default for AddrListMatcherBuilder {
    fn default() -> Self {
        Self::Any
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum PortListMatcherBuilder {
    Many(Vec<PortMatcherBuilder>),
    Single(PortMatcherBuilder),
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

impl Default for PortListMatcherBuilder {
    fn default() -> Self {
        Self::Any
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
enum PortMatcherBuilder {
    Single(u16),
    Range(RangeInclusive<u16>),
    Ranges(Arc<[RangeInclusive<u16>]>),
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

impl PortMatcherBuilder {
    pub fn build(self) -> PortMatcher {
        match self {
            Self::Single(port) => PortMatcher(vec![port..=port].into()),
            Self::Range(range) => PortMatcher(vec![range].into()),
            Self::Ranges(ranges) => PortMatcher(ranges),
        }
    }
}

#[derive(Debug, Clone)]
struct PortMatcher(Arc<[RangeInclusive<u16>]>);

impl PortMatcher {
    pub fn is_match(&self, port: u16) -> bool {
        self.0.iter().any(|range| range.contains(&port))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Proxy,
    Block,
    Direct,
}
