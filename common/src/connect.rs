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
/// table, the UDP connector, and every mux UDP dialer, so a config reload
/// performs a single replacement and the stream and UDP connectors can
/// never observe different configurations.
#[derive(Debug, Clone)]
pub struct ConnectorConfigHandle(Arc<RwLock<ConnectorConfig>>);

impl ConnectorConfigHandle {
    /// A fresh cell holding `config`.
    pub fn new(config: ConnectorConfig) -> Self {
        Self(Arc::new(RwLock::new(config)))
    }

    /// Replace the shared configuration in a single write, visible to every
    /// consumer of this handle.
    pub fn replace(&self, config: ConnectorConfig) {
        *self.0.write().unwrap() = config;
    }

    /// The underlying cell, for consumers that take an
    /// `Arc<RwLock<ConnectorConfig>>` (the connector builders and the
    /// `UdpConnector`).
    pub fn cell(&self) -> Arc<RwLock<ConnectorConfig>> {
        Arc::clone(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cell_clone_observes_the_same_replacement() {
        let handle = ConnectorConfigHandle::new(ConnectorConfig::default());
        let stream_cell = handle.cell();
        let udp_cell = handle.cell();
        let replaced = ConnectorConfig {
            bind: BothVerIp {
                v4: Some("192.0.2.1".parse().unwrap()),
                v6: None,
            },
        };
        handle.replace(replaced);
        assert_eq!(
            stream_cell.read().unwrap().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
        assert_eq!(
            udp_cell.read().unwrap().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
    }
}

#[derive(Debug, Clone)]
pub struct ConnectorResetSignal(pub Notify);
