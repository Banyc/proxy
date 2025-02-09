use serde::{Deserialize, Serialize};

use crate::{addr::BothVerIp, config::Merge};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConfig {
    pub bind: BothVerIp,
}
impl Default for ConnectorConfig {
    fn default() -> Self {
        Self {
            bind: BothVerIp { v4: None, v6: None },
        }
    }
}
impl Merge for ConnectorConfig {
    type Error = String;
    fn merge(mut self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        self.bind.v4 = option_merge(self.bind.v4, other.bind.v4)
            .map_err(|()| String::from("repeated bind.v4"))?;
        self.bind.v6 = option_merge(self.bind.v6, other.bind.v6)
            .map_err(|()| String::from("repeated bind.v6"))?;
        Ok(self)
    }
}

fn option_merge<T>(a: Option<T>, b: Option<T>) -> Result<Option<T>, ()> {
    Ok(match (a, b) {
        (Some(_), Some(_)) => {
            return Err(());
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    })
}
