use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

use crate::{addr::BothVerIp, config::Merge, notify::Notify};

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

/// A shared handle to the process connector configuration.
///
/// One handle is created per process and shared by the stream connector
/// table, the UDP connector, and every mux UDP dialer. Consumers observe the
/// configuration through [`Self::current`]; only the server reload path
/// holds replacement authority, via [`Self::replace`]. The underlying
/// `Arc<RwLock<...>>` cell is never exposed, so no consumer can fork the
/// configuration into a divergent cell.
#[derive(Debug, Clone)]
pub struct ConnectorConfigHandle(Arc<RwLock<ConnectorConfig>>);

impl ConnectorConfigHandle {
    /// A fresh cell holding `config`.
    pub fn new(config: ConnectorConfig) -> Self {
        Self(Arc::new(RwLock::new(config)))
    }

    /// A snapshot of the current configuration.
    pub fn current(&self) -> ConnectorConfig {
        self.0.read().unwrap().clone()
    }

    /// Replace the shared configuration in a single write, visible to every
    /// consumer of this handle. Reserved for the server reload path.
    pub fn replace(&self, config: ConnectorConfig) {
        *self.0.write().unwrap() = config;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_handle_clone_observes_the_same_replacement() {
        let handle = ConnectorConfigHandle::new(ConnectorConfig::default());
        let stream_handle = handle.clone();
        let udp_handle = handle.clone();
        let replaced = ConnectorConfig {
            bind: BothVerIp {
                v4: Some("192.0.2.1".parse().unwrap()),
                v6: None,
            },
        };
        handle.replace(replaced);
        assert_eq!(
            stream_handle.current().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
        assert_eq!(
            udp_handle.current().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
    }
}

#[derive(Debug, Clone)]
pub struct ConnectorResetSignal(pub Notify);
