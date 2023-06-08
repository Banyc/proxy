use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FilterBuilder {
    pub match_acts: Option<Vec<MatchActBuilder>>,
}

impl FilterBuilder {
    pub fn build(self) -> Result<Filter, regex::Error> {
        let mut match_acts = Vec::new();
        if let Some(self_match_acts) = self.match_acts {
            for match_act in self_match_acts {
                match_acts.push(match_act.build()?);
            }
        }
        Ok(Filter { match_acts })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchActBuilder {
    #[serde(rename = "match")]
    pub match_pattern: String,
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

pub struct Filter {
    match_acts: Vec<MatchAct>,
}

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
        for match_act in &self.match_acts {
            if match_act.matcher.is_match(addr) {
                return match_act.action;
            }
        }
        Action::Proxy
    }
}
