use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{addr::InternetAddr, config::SharableConfig, matcher::Matcher};

use super::{
    HopConfigBuildError, ProbeFutures, Registries, RouteSelector, RouteSelectorBuildError,
    RouteSelectorBuilder,
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
        probes: &mut ProbeFutures,
    ) -> Result<RouteTable, RouteTableBuildError> {
        let mut built = vec![];
        for entry in self.entries {
            let e = entry.resolve(registries, probes)?;
            built.push(e);
        }
        Ok(RouteTable::new(built, registries.matcher.clone()))
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
        probes: &mut ProbeFutures,
    ) -> Result<RouteTableEntry, RouteTableBuildError> {
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
        let action = self.action.resolve(registries, probes)?;
        Ok(RouteTableEntry::new(name, matcher, action))
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
    Sharable(SharableConfig<RouteSelectorBuilder>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate) enum RouteActionBuilder {
    Tagged(RouteActionTagBuilder),
    RouteSelector(RouteActionSelector),
}
impl RouteActionBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
        probes: &mut ProbeFutures,
    ) -> Result<RouteAction, RouteTableBuildError> {
        let action = match self {
            RouteActionBuilder::Tagged(RouteActionTagBuilder::Direct) => RouteAction::Direct,
            RouteActionBuilder::Tagged(RouteActionTagBuilder::Block) => RouteAction::Block,
            RouteActionBuilder::RouteSelector(RouteActionSelector::Named { conn_selector }) => {
                forbid_reserved_selector_name(&conn_selector)?;
                let selector = registries
                    .conn_selector
                    .get(&conn_selector)
                    .cloned()
                    .ok_or(RouteTableBuildError::RouteSelectorKeyNotFound(
                        conn_selector,
                    ))?;
                RouteAction::RouteSelector(Arc::new(selector))
            }
            RouteActionBuilder::RouteSelector(RouteActionSelector::Sharable(
                SharableConfig::SharingKey(k),
            )) => {
                forbid_reserved_selector_name(&k)?;
                let selector = registries
                    .conn_selector
                    .get(&k)
                    .cloned()
                    .ok_or(RouteTableBuildError::RouteSelectorKeyNotFound(k))?;
                RouteAction::RouteSelector(Arc::new(selector))
            }
            RouteActionBuilder::RouteSelector(RouteActionSelector::Sharable(
                SharableConfig::Private(p),
            )) => {
                let selector = p.resolve(registries, probes)?;
                RouteAction::RouteSelector(Arc::new(selector))
            }
        };
        Ok(action)
    }
}

fn forbid_reserved_selector_name(name: &Arc<str>) -> Result<(), RouteTableBuildError> {
    if matches!(name.as_ref(), "direct" | "block") {
        return Err(RouteTableBuildError::ReservedRouteSelectorName(
            name.clone(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum RouteAction {
    Direct,
    Block,
    RouteSelector(Arc<RouteSelector>),
}

#[derive(Debug, Error)]
pub enum RouteTableBuildError {
    #[error("Conn selector key not found: `{0}`")]
    RouteSelectorKeyNotFound(Arc<str>),
    #[error("Matcher key not found: `{0}`")]
    MatcherKeyNotFound(Arc<str>),
    #[error("Matcher: {0}")]
    Matcher(#[source] regex::Error),
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] HopConfigBuildError),
    #[error("{0}")]
    RouteSelector(#[from] RouteSelectorBuildError),
    #[error("Conn selector name `{0}` is reserved (use the `direct`/`block` action instead)")]
    ReservedRouteSelectorName(Arc<str>),
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        matcher::Matcher,
        route::{HopConfig, ProbeRtt, RouteSelector, WeightedRouteChain},
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

    fn chain(weight: usize) -> WeightedRouteChain {
        WeightedRouteChain {
            weight,
            chain: Arc::from(Vec::<HopConfig>::new()),
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
        let mut probes = ProbeFutures::new();
        let selector = RouteSelector::new(
            vec![chain(1)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            None,
            CancellationToken::new(),
            &mut probes,
        )
        .unwrap();
        let table = RouteTable::new(
            vec![RouteTableEntry::new(
                None,
                matcher("{}"),
                RouteAction::RouteSelector(Arc::new(selector)),
            )],
            Arc::new(HashMap::new()),
        );
        assert!(matches!(
            table.action(&addr("1.2.3.4:80")),
            RouteAction::RouteSelector(_)
        ));
    }
}
