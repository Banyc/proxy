use std::{collections::HashMap, fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    addr::InternetAddr,
    config::SharableConfig,
    filter::{Matcher, MatcherBuilder},
};

use super::{
    BuildTracer, IntoAddr, ProxyConfigBuildError, ProxyGroup, ProxyGroupBuildContext,
    ProxyGroupBuildError, ProxyGroupBuilder, TraceRtt,
};

#[derive(Debug)]
pub struct ProxyTableBuildContext<'caller, Addr, TracerBuilder> {
    pub matcher: &'caller HashMap<Arc<str>, Matcher>,
    pub proxy_group: &'caller HashMap<Arc<str>, ProxyGroup<Addr>>,
    pub proxy_group_cx: ProxyGroupBuildContext<'caller, Addr, TracerBuilder>,
}
impl<Addr, TracerBuilder> Clone for ProxyTableBuildContext<'_, Addr, TracerBuilder> {
    fn clone(&self) -> Self {
        Self {
            matcher: self.matcher,
            proxy_group: self.proxy_group,
            proxy_group_cx: self.proxy_group_cx.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(transparent)]
pub struct ProxyTableBuilder<AddrStr> {
    #[serde(flatten)]
    pub entries: Vec<ProxyTableEntryBuilder<AddrStr>>,
}
impl<AddrStr> ProxyTableBuilder<AddrStr> {
    pub fn build<Addr, TracerBuilder, Tracer>(
        self,
        cx: ProxyTableBuildContext<'_, Addr, TracerBuilder>,
    ) -> Result<ProxyTable<Addr>, ProxyTableBuildError>
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
        Ok(ProxyTable::new(built))
    }
}

#[derive(Debug, Clone)]
pub struct ProxyTable<Addr> {
    entries: Vec<ProxyTableEntry<Addr>>,
}
impl<Addr> ProxyTable<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    const BLOCK_ACTION: ProxyAction<Addr> = ProxyAction::Block;

    pub fn new(entries: Vec<ProxyTableEntry<Addr>>) -> Self {
        Self { entries }
    }

    pub fn action(&self, addr: &InternetAddr) -> &ProxyAction<Addr> {
        self.entries
            .iter()
            .find(|&entry| entry.matcher().matches(addr))
            .map(|entry| entry.action())
            .unwrap_or(&Self::BLOCK_ACTION)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyTableEntryBuilder<AddrStr> {
    matcher: SharableConfig<MatcherBuilder>,
    action: ProxyActionBuilder<AddrStr>,
}
impl<AddrStr> ProxyTableEntryBuilder<AddrStr> {
    pub fn build<Addr, TracerBuilder, Tracer>(
        self,
        cx: ProxyTableBuildContext<'_, Addr, TracerBuilder>,
    ) -> Result<ProxyTableEntry<Addr>, ProxyTableBuildError>
    where
        Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AddrStr: IntoAddr<Addr = Addr>,
        TracerBuilder: BuildTracer<Tracer = Tracer>,
        Tracer: TraceRtt<Addr = Addr> + Sync + Send + 'static,
    {
        let matcher = match self.matcher {
            SharableConfig::SharingKey(k) => cx
                .matcher
                .get(&k)
                .cloned()
                .ok_or(ProxyTableBuildError::ProxyGroupKeyNotFound(k))?,
            SharableConfig::Private(v) => v.build().map_err(ProxyTableBuildError::Matcher)?,
        };
        let action = self.action.build(cx.proxy_group, cx.proxy_group_cx)?;
        Ok(ProxyTableEntry::new(matcher, action))
    }
}

#[derive(Debug, Clone)]
pub struct ProxyTableEntry<Addr> {
    matcher: Matcher,
    action: ProxyAction<Addr>,
}
impl<Addr> ProxyTableEntry<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new(matcher: Matcher, action: ProxyAction<Addr>) -> Self {
        Self { matcher, action }
    }

    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }

    pub fn action(&self) -> &ProxyAction<Addr> {
        &self.action
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub enum ProxyActionTagBuilder {
    Direct,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum ProxyActionBuilder<AddrStr> {
    Tagged(ProxyActionTagBuilder),
    ProxyGroup(SharableConfig<ProxyGroupBuilder<AddrStr>>),
}
impl<AddrStr> ProxyActionBuilder<AddrStr> {
    pub fn build<Addr, TracerBuilder, Tracer>(
        self,
        proxy_group: &HashMap<Arc<str>, ProxyGroup<Addr>>,
        proxy_group_cx: ProxyGroupBuildContext<'_, Addr, TracerBuilder>,
    ) -> Result<ProxyAction<Addr>, ProxyTableBuildError>
    where
        Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AddrStr: IntoAddr<Addr = Addr>,
        TracerBuilder: BuildTracer<Tracer = Tracer>,
        Tracer: TraceRtt<Addr = Addr> + Sync + Send + 'static,
    {
        Ok(match self {
            ProxyActionBuilder::Tagged(ProxyActionTagBuilder::Direct) => ProxyAction::Direct,
            ProxyActionBuilder::Tagged(ProxyActionTagBuilder::Block) => ProxyAction::Block,
            ProxyActionBuilder::ProxyGroup(p) => ProxyAction::ProxyGroup(Arc::new(match p {
                SharableConfig::SharingKey(k) => proxy_group
                    .get(&k)
                    .cloned()
                    .ok_or(ProxyTableBuildError::ProxyGroupKeyNotFound(k))?,
                SharableConfig::Private(p) => p.build(proxy_group_cx)?,
            })),
        })
    }
}

#[derive(Debug, Clone)]
pub enum ProxyAction<Addr> {
    Direct,
    Block,
    ProxyGroup(Arc<ProxyGroup<Addr>>),
}

#[derive(Debug, Error)]
pub enum ProxyTableBuildError {
    #[error("Proxy group key not found: `{0}`")]
    ProxyGroupKeyNotFound(Arc<str>),
    #[error("Matcher: {0}")]
    Matcher(#[source] regex::Error),
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] ProxyConfigBuildError),
    #[error("{0}")]
    ProxyGroup(#[from] ProxyGroupBuildError),
}
