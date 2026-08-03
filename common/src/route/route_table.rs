use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    addr::InternetAddr,
    config::SharableConfig,
    filter::{Matcher, MatcherBuilder},
    route::ConnSelector,
};

use super::{
    BuildTracer, ConnConfigBuildError, ConnSelectorBuildContext, ConnSelectorBuildError,
    ConnSelectorBuilder, IntoAddr, TraceRtt,
};

#[derive(Debug)]
pub struct RouteTableBuildContext<'caller, Addr, TracerBuilder> {
    pub matcher: &'caller Arc<HashMap<Arc<str>, Matcher>>,
    pub conn_selector: &'caller HashMap<Arc<str>, ConnSelector<Addr>>,
    pub conn_selector_cx: ConnSelectorBuildContext<'caller, Addr, TracerBuilder>,
}
impl<Addr, TracerBuilder> Clone for RouteTableBuildContext<'_, Addr, TracerBuilder> {
    fn clone(&self) -> Self {
        Self {
            matcher: self.matcher,
            conn_selector: self.conn_selector,
            conn_selector_cx: self.conn_selector_cx.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(transparent)]
pub struct RouteTableBuilder<AddrStr> {
    #[serde(flatten)]
    pub entries: Vec<RouteTableEntryBuilder<AddrStr>>,
}
impl<AddrStr> RouteTableBuilder<AddrStr> {
    pub fn build<Addr, TracerBuilder, Tracer>(
        self,
        cx: RouteTableBuildContext<'_, Addr, TracerBuilder>,
    ) -> Result<RouteTable<Addr>, RouteTableBuildError>
    where
        Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AddrStr: IntoAddr<Addr = Addr>,
        TracerBuilder: BuildTracer<Tracer = Tracer>,
        Tracer: TraceRtt<Addr = Addr> + Sync + Send + 'static,
    {
        let mut built = vec![];
        for entry in self.entries {
            let e = entry.build(cx.clone())?;
            built.push(e);
        }
        Ok(RouteTable::new(built, cx.matcher.clone()))
    }
}

#[derive(Debug, Clone)]
pub struct RouteTable<Addr> {
    entries: Vec<RouteTableEntry<Addr>>,
    matchers: Arc<HashMap<Arc<str>, Matcher>>,
}
impl<Addr> RouteTable<Addr>
where
    Addr: fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    const BLOCK_ACTION: RouteAction<Addr> = RouteAction::Block;

    pub fn new(
        entries: Vec<RouteTableEntry<Addr>>,
        matchers: Arc<HashMap<Arc<str>, Matcher>>,
    ) -> Self {
        Self { entries, matchers }
    }

    pub fn action(&self, addr: &InternetAddr) -> &RouteAction<Addr> {
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
pub struct RouteTableEntryBuilder<AddrStr> {
    matcher: SharableConfig<MatcherBuilder>,
    action: RouteActionBuilder<AddrStr>,
}
impl<AddrStr> RouteTableEntryBuilder<AddrStr> {
    pub fn build<Addr, TracerBuilder, Tracer>(
        self,
        cx: RouteTableBuildContext<'_, Addr, TracerBuilder>,
    ) -> Result<RouteTableEntry<Addr>, RouteTableBuildError>
    where
        Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AddrStr: IntoAddr<Addr = Addr>,
        TracerBuilder: BuildTracer<Tracer = Tracer>,
        Tracer: TraceRtt<Addr = Addr> + Sync + Send + 'static,
    {
        let (name, matcher) = match self.matcher {
            SharableConfig::SharingKey(k) => (
                Some(k.clone()),
                cx.matcher
                    .get(&k)
                    .cloned()
                    .ok_or(RouteTableBuildError::ConnSelectorKeyNotFound(k))?,
            ),
            SharableConfig::Private(v) => (None, v.build().map_err(RouteTableBuildError::Matcher)?),
        };
        let action = self.action.build(cx.conn_selector, cx.conn_selector_cx)?;
        Ok(RouteTableEntry::new(name, matcher, action))
    }
}

#[derive(Debug, Clone)]
pub struct RouteTableEntry<Addr> {
    matcher_name: Option<Arc<str>>,
    matcher: Matcher,
    action: RouteAction<Addr>,
}
impl<Addr> RouteTableEntry<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new(
        matcher_name: Option<Arc<str>>,
        matcher: Matcher,
        action: RouteAction<Addr>,
    ) -> Self {
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
    pub fn action(&self) -> &RouteAction<Addr> {
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
pub enum RouteActionBuilder<AddrStr> {
    Tagged(RouteActionTagBuilder),
    ConnSelector(SharableConfig<ConnSelectorBuilder<AddrStr>>),
}
impl<AddrStr> RouteActionBuilder<AddrStr> {
    pub fn build<Addr, TracerBuilder, Tracer>(
        self,
        conn_selector: &HashMap<Arc<str>, ConnSelector<Addr>>,
        conn_selector_cx: ConnSelectorBuildContext<'_, Addr, TracerBuilder>,
    ) -> Result<RouteAction<Addr>, RouteTableBuildError>
    where
        Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AddrStr: IntoAddr<Addr = Addr>,
        TracerBuilder: BuildTracer<Tracer = Tracer>,
        Tracer: TraceRtt<Addr = Addr> + Sync + Send + 'static,
    {
        Ok(match self {
            RouteActionBuilder::Tagged(RouteActionTagBuilder::Direct) => RouteAction::Direct,
            RouteActionBuilder::Tagged(RouteActionTagBuilder::Block) => RouteAction::Block,
            RouteActionBuilder::ConnSelector(p) => RouteAction::ConnSelector(Arc::new(match p {
                SharableConfig::SharingKey(k) => conn_selector
                    .get(&k)
                    .cloned()
                    .ok_or(RouteTableBuildError::ConnSelectorKeyNotFound(k))?,
                SharableConfig::Private(p) => p.build(conn_selector_cx)?,
            })),
        })
    }
}

#[derive(Debug, Clone)]
pub enum RouteAction<Addr> {
    Direct,
    Block,
    ConnSelector(Arc<ConnSelector<Addr>>),
}

#[derive(Debug, Error)]
pub enum RouteTableBuildError {
    #[error("Proxy group key not found: `{0}`")]
    ConnSelectorKeyNotFound(Arc<str>),
    #[error("Matcher: {0}")]
    Matcher(#[source] regex::Error),
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] ConnConfigBuildError),
    #[error("{0}")]
    ConnSelector(#[from] ConnSelectorBuildError),
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        error::AnyError,
        route::{ConnChain, ConnConfig, TraceRtt, WeightedConnChain},
    };

    fn matcher(json: &str) -> Matcher {
        let builder: MatcherBuilder = serde_json::from_str(json).unwrap();
        builder.build().unwrap()
    }

    fn addr(s: &str) -> InternetAddr {
        s.parse().unwrap()
    }

    fn entry(matcher: Matcher, action: RouteAction<SocketAddr>) -> RouteTableEntry<SocketAddr> {
        RouteTableEntry::new(None, matcher, action)
    }

    struct NoTracer;
    impl TraceRtt for NoTracer {
        type Addr = SocketAddr;
        async fn trace_rtt(&self, _chain: &ConnChain<SocketAddr>) -> Result<Duration, AnyError> {
            unreachable!("no tracer in these tests")
        }
    }

    fn chain(weight: usize) -> WeightedConnChain<SocketAddr> {
        WeightedConnChain {
            weight,
            chain: Arc::from(Vec::<ConnConfig<SocketAddr>>::new()),
            payload_crypto: None,
        }
    }

    #[test]
    fn an_empty_table_blocks_everything() {
        let table = RouteTable::<SocketAddr>::new(vec![], Arc::new(HashMap::new()));
        assert!(matches!(table.action(&addr("1.2.3.4:80")), RouteAction::Block));
        assert!(matches!(
            table.action(&addr("[2001:db8::1]:443")),
            RouteAction::Block
        ));
        assert!(matches!(table.action(&addr("example.com:443")), RouteAction::Block));
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
        assert!(matches!(table.action(&addr("10.0.0.5:80")), RouteAction::Direct));
        // 9.9.9.9 only matches the catch-all; still Direct.
        assert!(matches!(table.action(&addr("9.9.9.9:80")), RouteAction::Direct));
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
        assert!(matches!(table.action(&addr("1.2.3.4:80")), RouteAction::Block));
        assert!(matches!(table.action(&addr("1.2.3.4:443")), RouteAction::Direct));
    }

    #[test]
    fn unmatched_traffic_is_blocked() {
        let table = RouteTable::new(
            vec![entry(matcher(r#"{"addr": "10.0.0.5"}"#), RouteAction::Direct)],
            Arc::new(HashMap::new()),
        );
        assert!(matches!(table.action(&addr("10.0.0.6:80")), RouteAction::Block));
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
                RouteAction::<SocketAddr>::Direct,
            )],
            matchers,
        );
        assert!(matches!(
            table.action(&addr("192.168.1.10:80")),
            RouteAction::Direct
        ));
        assert!(matches!(table.action(&addr("8.8.8.8:80")), RouteAction::Block));
    }

    #[test]
    fn a_shared_matcher_name_is_only_consulted_once() {
        let matchers = Arc::new(HashMap::from([(Arc::from("direct"), matcher("{}"))]));
        let table = RouteTable::new(
            vec![
                RouteTableEntry::new(Some("direct".into()), matcher("{}"), RouteAction::<SocketAddr>::Direct),
                RouteTableEntry::new(Some("direct".into()), matcher("{}"), RouteAction::<SocketAddr>::Block),
            ],
            matchers,
        );
        // Both entries carry the same shared matcher name, so the second is skipped
        // entirely and the first entry's Direct action wins over the later Block.
        assert!(matches!(table.action(&addr("1.2.3.4:80")), RouteAction::Direct));
    }

    #[test]
    fn a_matcher_referencing_itself_terminates() {
        let self_ref = matcher(r#""recur""#);
        let matchers = Arc::new(HashMap::from([(Arc::from("recur"), self_ref.clone())]));
        let table = RouteTable::new(
            vec![RouteTableEntry::new(None, self_ref, RouteAction::<SocketAddr>::Direct)],
            matchers,
        );
        // The self reference is stopped by the visited set and cannot match.
        assert!(matches!(table.action(&addr("1.2.3.4:80")), RouteAction::Block));
    }

    #[test]
    fn a_conn_selector_action_is_preserved_when_selected() {
        let selector = ConnSelector::new(
            vec![chain(1)],
            None::<NoTracer>,
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
