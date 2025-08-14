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
