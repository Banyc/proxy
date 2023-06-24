use std::{collections::HashMap, sync::Arc};

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::SharableConfig;

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
#[serde(transparent)]
pub struct FilterBuilder {
    pub match_acts: Option<Vec<SharableConfig<MatchActBuilder>>>,
}

impl FilterBuilder {
    pub fn build(self, filters: &HashMap<Arc<str>, Filter>) -> Result<Filter, FilterBuildError> {
        let mut match_acts = Vec::new();
        if let Some(self_match_acts) = self.match_acts {
            for match_act in self_match_acts {
                match match_act {
                    SharableConfig::SharingKey(key) => {
                        match_acts.extend(
                            filters
                                .get(&key)
                                .ok_or_else(|| FilterBuildError::KeyNotFound(key.clone()))?
                                .match_acts
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
            match_acts: match_acts.into(),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchActBuilder {
    #[serde(rename = "match")]
    pub match_pattern: Arc<str>,
    pub action: Action,
}

impl MatchActBuilder {
    fn build(self) -> Result<MatchAct, regex::Error> {
        Ok(MatchAct {
            matcher: Regex::new(&self.match_pattern)?,
            action: self.action,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Filter {
    match_acts: Arc<[MatchAct]>,
}

#[derive(Debug, Clone)]
struct MatchAct {
    matcher: Regex,
    action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Proxy,
    Block,
    Direct,
}

impl Filter {
    pub fn filter(&self, addr: &str) -> Action {
        for match_act in self.match_acts.as_ref() {
            if match_act.matcher.is_match(addr) {
                return match_act.action;
            }
        }
        Action::Proxy
    }
}
