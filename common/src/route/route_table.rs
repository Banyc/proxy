use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{addr::InternetAddr, config::SharableConfig, matcher::Matcher};

use super::{
    ConnConfigBuildError, ConnSelector, ConnSelectorBuildError, ConnSelectorBuilder, Registries,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(transparent)]
pub struct RouteTableBuilder {
    #[serde(flatten)]
    pub entries: Vec<RouteTableEntryBuilder>,
}
impl RouteTableBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
    ) -> Result<(RouteTable, tokio::task::JoinSet<()>), RouteTableBuildError> {
        let mut built = vec![];
        let mut drivers = tokio::task::JoinSet::new();
        for entry in self.entries {
            let (e, mut driver) = entry.resolve(registries)?;
            if !driver.is_empty() {
                drivers.spawn(async move { while driver.join_next().await.is_some() {} });
            }
            built.push(e);
        }
        Ok((RouteTable::new(built, registries.matcher.clone()), drivers))
    }
}

#[derive(Debug, Clone)]
pub struct RouteTable {
    entries: Vec<RouteTableEntry>,
    matchers: Arc<HashMap<Arc<str>, Matcher>>,
}
impl RouteTable {
    const BLOCK_ACTION: RouteAction = RouteAction::Block;

    pub fn new(entries: Vec<RouteTableEntry>, matchers: Arc<HashMap<Arc<str>, Matcher>>) -> Self {
        Self { entries, matchers }
    }

    pub fn action(&self, addr: &InternetAddr) -> &RouteAction {
        let mut visited = HashSet::new();
        self.entries
            .iter()
            .find(|&entry| {
                if let Some(name) = entry.matcher_name() {
                    if visited.contains(name) {
                        return false;
                    }
                    visited.insert(name.clone());
                }
                entry.matcher().matches(addr, &self.matchers, &mut visited)
            })
            .map(|entry| entry.action())
            .unwrap_or(&Self::BLOCK_ACTION)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteTableEntryBuilder {
    matcher: SharableConfig<Matcher>,
    action: RouteActionBuilder,
}
impl RouteTableEntryBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
    ) -> Result<(RouteTableEntry, tokio::task::JoinSet<()>), RouteTableBuildError> {
        let (name, matcher) = match self.matcher {
            SharableConfig::SharingKey(k) => (
                Some(k.clone()),
                registries
                    .matcher
                    .get(&k)
                    .cloned()
                    .ok_or(RouteTableBuildError::MatcherKeyNotFound(k))?,
            ),
            SharableConfig::Private(v) => (None, v),
        };
        let (action, drivers) = self.action.resolve(registries)?;
        Ok((RouteTableEntry::new(name, matcher, action), drivers))
    }
}

#[derive(Debug, Clone)]
pub struct RouteTableEntry {
    matcher_name: Option<Arc<str>>,
    matcher: Matcher,
    action: RouteAction,
}
impl RouteTableEntry {
    pub fn new(matcher_name: Option<Arc<str>>, matcher: Matcher, action: RouteAction) -> Self {
        Self {
            matcher_name,
            matcher,
            action,
        }
    }

    pub fn matcher_name(&self) -> Option<&Arc<str>> {
        self.matcher_name.as_ref()
    }
    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }
    pub fn action(&self) -> &RouteAction {
        &self.action
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub enum RouteActionTagBuilder {
    Direct,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate) enum RouteActionSelector {
    Named { conn_selector: Arc<str> },
    Sharable(SharableConfig<ConnSelectorBuilder>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate) enum RouteActionBuilder {
    Tagged(RouteActionTagBuilder),
    ConnSelector(RouteActionSelector),
}
impl RouteActionBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
    ) -> Result<(RouteAction, tokio::task::JoinSet<()>), RouteTableBuildError> {
        let (action, drivers) = match self {
            RouteActionBuilder::Tagged(RouteActionTagBuilder::Direct) => {
                (RouteAction::Direct, tokio::task::JoinSet::new())
            }
            RouteActionBuilder::Tagged(RouteActionTagBuilder::Block) => {
                (RouteAction::Block, tokio::task::JoinSet::new())
            }
            RouteActionBuilder::ConnSelector(RouteActionSelector::Named { conn_selector }) => {
                forbid_reserved_selector_name(&conn_selector)?;
                let selector = registries
                    .conn_selector
                    .get(&conn_selector)
                    .cloned()
                    .ok_or(RouteTableBuildError::ConnSelectorKeyNotFound(conn_selector))?;
                (
                    RouteAction::ConnSelector(Arc::new(selector)),
                    tokio::task::JoinSet::new(),
                )
            }
            RouteActionBuilder::ConnSelector(RouteActionSelector::Sharable(
                SharableConfig::SharingKey(k),
            )) => {
                forbid_reserved_selector_name(&k)?;
                let selector = registries
                    .conn_selector
                    .get(&k)
                    .cloned()
                    .ok_or(RouteTableBuildError::ConnSelectorKeyNotFound(k))?;
                (
                    RouteAction::ConnSelector(Arc::new(selector)),
                    tokio::task::JoinSet::new(),
                )
            }
            RouteActionBuilder::ConnSelector(RouteActionSelector::Sharable(
                SharableConfig::Private(p),
            )) => {
                let (selector, drivers) = p.resolve(registries)?;
                (RouteAction::ConnSelector(Arc::new(selector)), drivers)
            }
        };
        Ok((action, drivers))
    }
}

fn forbid_reserved_selector_name(name: &Arc<str>) -> Result<(), RouteTableBuildError> {
    if matches!(name.as_ref(), "direct" | "block") {
        return Err(RouteTableBuildError::ReservedConnSelectorName(name.clone()));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum RouteAction {
    Direct,
    Block,
    ConnSelector(Arc<ConnSelector>),
}

#[derive(Debug, Error)]
pub enum RouteTableBuildError {
    #[error("Conn selector key not found: `{0}`")]
    ConnSelectorKeyNotFound(Arc<str>),
    #[error("Matcher key not found: `{0}`")]
    MatcherKeyNotFound(Arc<str>),
    #[error("Matcher: {0}")]
    Matcher(#[source] regex::Error),
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] ConnConfigBuildError),
    #[error("{0}")]
    ConnSelector(#[from] ConnSelectorBuildError),
    #[error("Conn selector name `{0}` is reserved (use the `direct`/`block` action instead)")]
    ReservedConnSelectorName(Arc<str>),
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        matcher::Matcher,
        route::{ConnConfig, ConnSelector, ProbeRtt, WeightedConnChain},
    };

    fn matcher(json: &str) -> Matcher {
        serde_json::from_str(json).unwrap()
    }

    fn addr(s: &str) -> InternetAddr {
        s.parse().unwrap()
    }

    fn entry(matcher: Matcher, action: RouteAction) -> RouteTableEntry {
        RouteTableEntry::new(None, matcher, action)
    }

    fn chain(weight: usize) -> WeightedConnChain {
        WeightedConnChain {
            weight,
            chain: Arc::from(Vec::<ConnConfig>::new()),
            payload_crypto: None,
        }
    }

    #[test]
    fn an_empty_table_blocks_everything() {
        let table = RouteTable::new(vec![], Arc::new(HashMap::new()));
        assert!(matches!(
            table.action(&addr("1.2.3.4:80")),
            RouteAction::Block
        ));
        assert!(matches!(
            table.action(&addr("[2001:db8::1]:443")),
            RouteAction::Block
        ));
        assert!(matches!(
            table.action(&addr("example.com:443")),
            RouteAction::Block
        ));
    }

    #[test]
    fn the_first_matching_entry_wins() {
        let table = RouteTable::new(
            vec![
                entry(matcher("{}"), RouteAction::Direct),
                entry(matcher(r#"{"addr": "10.0.0.5"}"#), RouteAction::Block),
            ],
            Arc::new(HashMap::new()),
        );
        // 10.0.0.5 matches both entries; the earlier Direct action wins.
        assert!(matches!(
            table.action(&addr("10.0.0.5:80")),
            RouteAction::Direct
        ));
        // 9.9.9.9 only matches the catch-all; still Direct.
        assert!(matches!(
            table.action(&addr("9.9.9.9:80")),
            RouteAction::Direct
        ));
    }

    #[test]
    fn a_non_matching_entry_falls_through_to_a_later_one() {
        let table = RouteTable::new(
            vec![
                entry(matcher(r#"{"port": 80}"#), RouteAction::Block),
                entry(matcher("{}"), RouteAction::Direct),
            ],
            Arc::new(HashMap::new()),
        );
        assert!(matches!(
            table.action(&addr("1.2.3.4:80")),
            RouteAction::Block
        ));
        assert!(matches!(
            table.action(&addr("1.2.3.4:443")),
            RouteAction::Direct
        ));
    }

    #[test]
    fn unmatched_traffic_is_blocked() {
        let table = RouteTable::new(
            vec![entry(
                matcher(r#"{"addr": "10.0.0.5"}"#),
                RouteAction::Direct,
            )],
            Arc::new(HashMap::new()),
        );
        assert!(matches!(
            table.action(&addr("10.0.0.6:80")),
            RouteAction::Block
        ));
    }

    #[test]
    fn an_entry_can_reference_a_shared_matcher_by_name() {
        let matchers = Arc::new(HashMap::from([(
            Arc::from("lan"),
            matcher(r#"{"addr": {"start": "192.168.0.0", "end": "192.168.255.255"}}"#),
        )]));
        let table = RouteTable::new(
            vec![RouteTableEntry::new(
                None,
                matcher(r#""lan""#),
                RouteAction::Direct,
            )],
            matchers,
        );
        assert!(matches!(
            table.action(&addr("192.168.1.10:80")),
            RouteAction::Direct
        ));
        assert!(matches!(
            table.action(&addr("8.8.8.8:80")),
            RouteAction::Block
        ));
    }

    #[test]
    fn a_shared_matcher_name_is_only_consulted_once() {
        let matchers = Arc::new(HashMap::from([(Arc::from("direct"), matcher("{}"))]));
        let table = RouteTable::new(
            vec![
                RouteTableEntry::new(Some("direct".into()), matcher("{}"), RouteAction::Direct),
                RouteTableEntry::new(Some("direct".into()), matcher("{}"), RouteAction::Block),
            ],
            matchers,
        );
        // Both entries carry the same shared matcher name, so the second is skipped
        // entirely and the first entry's Direct action wins over the later Block.
        assert!(matches!(
            table.action(&addr("1.2.3.4:80")),
            RouteAction::Direct
        ));
    }

    #[test]
    fn a_matcher_referencing_itself_terminates() {
        let self_ref = matcher(r#""recur""#);
        let matchers = Arc::new(HashMap::from([(Arc::from("recur"), self_ref.clone())]));
        let table = RouteTable::new(
            vec![RouteTableEntry::new(None, self_ref, RouteAction::Direct)],
            matchers,
        );
        // The self reference is stopped by the visited set and cannot match.
        assert!(matches!(
            table.action(&addr("1.2.3.4:80")),
            RouteAction::Block
        ));
    }

    #[test]
    fn a_conn_selector_action_is_preserved_when_selected() {
        let (selector, _drivers) = ConnSelector::new(
            vec![chain(1)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            None,
            CancellationToken::new(),
        )
        .unwrap();
        let table = RouteTable::new(
            vec![RouteTableEntry::new(
                None,
                matcher("{}"),
                RouteAction::ConnSelector(Arc::new(selector)),
            )],
            Arc::new(HashMap::new()),
        );
        assert!(matches!(
            table.action(&addr("1.2.3.4:80")),
            RouteAction::ConnSelector(_)
        ));
    }
}
