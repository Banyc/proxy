use std::{collections::HashMap, fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    addr::InternetAddr,
    config::SharableConfig,
    filter::{Matcher, MatcherBuilder},
};

use super::{
    AddressString, ProxyConfigBuildError, ProxyGroup, ProxyGroupBuildContext, ProxyGroupBuildError,
    ProxyGroupBuilder, Tracer, TracerBuilder,
};

#[derive(Debug)]
pub struct ProxyTableBuildContext<'caller, A, TB> {
    pub matcher: &'caller HashMap<Arc<str>, Matcher>,
    pub proxy_group: &'caller HashMap<Arc<str>, ProxyGroup<A>>,
    pub proxy_group_cx: ProxyGroupBuildContext<'caller, A, TB>,
}
impl<'caller, A, TB> Clone for ProxyTableBuildContext<'caller, A, TB> {
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
pub struct ProxyTableBuilder<AS> {
    #[serde(flatten)]
    pub entries: Vec<ProxyTableEntryBuilder<AS>>,
}
impl<AS> ProxyTableBuilder<AS> {
    pub fn build<A, TB, T>(
        self,
        cx: ProxyTableBuildContext<'_, A, TB>,
    ) -> Result<ProxyTable<A>, ProxyTableBuildError>
    where
        A: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AS: AddressString<Address = A>,
        TB: TracerBuilder<Tracer = T>,
        T: Tracer<Address = A> + Sync + Send + 'static,
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
pub struct ProxyTable<A> {
    entries: Vec<ProxyTableEntry<A>>,
}
impl<A> ProxyTable<A>
where
    A: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    const BLOCK_ACTION: ProxyAction<A> = ProxyAction::Block;

    pub fn new(entries: Vec<ProxyTableEntry<A>>) -> Self {
        Self { entries }
    }

    pub fn action(&self, addr: &InternetAddr) -> &ProxyAction<A> {
        self.entries
            .iter()
            .find(|&entry| entry.matcher().matches(addr))
            .map(|entry| entry.action())
            .unwrap_or(&Self::BLOCK_ACTION)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyTableEntryBuilder<AS> {
    matcher: SharableConfig<MatcherBuilder>,
    action: ProxyActionBuilder<AS>,
}
impl<AS> ProxyTableEntryBuilder<AS> {
    pub fn build<A, TB, T>(
        self,
        cx: ProxyTableBuildContext<'_, A, TB>,
    ) -> Result<ProxyTableEntry<A>, ProxyTableBuildError>
    where
        A: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AS: AddressString<Address = A>,
        TB: TracerBuilder<Tracer = T>,
        T: Tracer<Address = A> + Sync + Send + 'static,
    {
        let matcher = match self.matcher {
            SharableConfig::SharingKey(k) => cx
                .matcher
                .get(&k)
                .cloned()
                .ok_or_else(|| ProxyTableBuildError::ProxyGroupKeyNotFound(k))?,
            SharableConfig::Private(v) => v.build().map_err(ProxyTableBuildError::Matcher)?,
        };
        let action = self.action.build(cx.proxy_group, cx.proxy_group_cx)?;
        Ok(ProxyTableEntry::new(matcher, action))
    }
}

#[derive(Debug, Clone)]
pub struct ProxyTableEntry<A> {
    matcher: Matcher,
    action: ProxyAction<A>,
}
impl<A> ProxyTableEntry<A>
where
    A: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new(matcher: Matcher, action: ProxyAction<A>) -> Self {
        Self { matcher, action }
    }

    pub fn matcher(&self) -> &Matcher {
        &self.matcher
    }

    pub fn action(&self) -> &ProxyAction<A> {
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
pub enum ProxyActionBuilder<AS> {
    Tagged(ProxyActionTagBuilder),
    ProxyGroup(SharableConfig<ProxyGroupBuilder<AS>>),
}
impl<AS> ProxyActionBuilder<AS> {
    pub fn build<A, TB, T>(
        self,
        proxy_group: &HashMap<Arc<str>, ProxyGroup<A>>,
        proxy_group_cx: ProxyGroupBuildContext<'_, A, TB>,
    ) -> Result<ProxyAction<A>, ProxyTableBuildError>
    where
        A: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AS: AddressString<Address = A>,
        TB: TracerBuilder<Tracer = T>,
        T: Tracer<Address = A> + Sync + Send + 'static,
    {
        Ok(match self {
            ProxyActionBuilder::Tagged(ProxyActionTagBuilder::Direct) => ProxyAction::Direct,
            ProxyActionBuilder::Tagged(ProxyActionTagBuilder::Block) => ProxyAction::Block,
            ProxyActionBuilder::ProxyGroup(p) => ProxyAction::ProxyGroup(Arc::new(match p {
                SharableConfig::SharingKey(k) => proxy_group
                    .get(&k)
                    .cloned()
                    .ok_or_else(|| ProxyTableBuildError::ProxyGroupKeyNotFound(k))?,
                SharableConfig::Private(p) => p.build(proxy_group_cx)?,
            })),
        })
    }
}

#[derive(Debug, Clone)]
pub enum ProxyAction<A> {
    Direct,
    Block,
    ProxyGroup(Arc<ProxyGroup<A>>),
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
