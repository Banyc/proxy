use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

use crate::{addr::DualStackBind, config::Merge, notify::Notify};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConfig {
    pub bind: DualStackBind,
}
impl Default for ConnectorConfig {
    fn default() -> Self {
        Self {
            bind: DualStackBind { v4: None, v6: None },
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

/// The process connector configuration, split into two capabilities:
///
/// - [`ConnectorConfigReader`] — cloneable, shared by the stream connector
///   table, the UDP connector, and every mux UDP dialer, exposing only
///   [`Self::current`].
/// - [`ConnectorConfigUpdater`] — not cloneable, held solely by the server
///   reload path, exposing only [`Self::replace`].
///
/// Both halves share one `Arc<RwLock<...>>` cell that is never exposed, so
/// consumers cannot fork the configuration into a divergent cell, and the
/// replacement authority cannot be duplicated through a clone.
#[derive(Debug, Clone)]
pub struct ConnectorConfigReader(Arc<RwLock<ConnectorConfig>>);

impl ConnectorConfigReader {
    /// A snapshot of the current configuration.
    pub fn current(&self) -> ConnectorConfig {
        self.0.read().unwrap().clone()
    }
}

#[derive(Debug)]
pub struct ConnectorConfigUpdater(Arc<RwLock<ConnectorConfig>>);

impl ConnectorConfigUpdater {
    /// Replace the shared configuration in a single write, visible to every
    /// reader. The sole updater is retained by the server reload path.
    pub fn replace(&self, config: ConnectorConfig) {
        *self.0.write().unwrap() = config;
    }
}

/// Create the shared connector-configuration cell, handing out the reader
/// capability (cloneable, for every connector) and the sole updater
/// capability (for the server reload path).
pub fn connector_config_cell(
    config: ConnectorConfig,
) -> (ConnectorConfigReader, ConnectorConfigUpdater) {
    let cell = Arc::new(RwLock::new(config));
    (
        ConnectorConfigReader(Arc::clone(&cell)),
        ConnectorConfigUpdater(cell),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reader_clone_observes_the_same_replacement() {
        let (reader, updater) = connector_config_cell(ConnectorConfig::default());
        let stream_reader = reader.clone();
        let udp_reader = reader.clone();
        let replaced = ConnectorConfig {
            bind: DualStackBind {
                v4: Some("192.0.2.1".parse().unwrap()),
                v6: None,
            },
        };
        updater.replace(replaced);
        assert_eq!(
            stream_reader.current().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
        assert_eq!(
            udp_reader.current().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
    }

    /// `Merge` adds keys, it never overrides them: a later config file may
    /// supply the bind family an earlier file left unset, but the family the
    /// earlier file already set must survive untouched. No other test in the
    /// workspace merges a `ConnectorConfig`, so without this the merge
    /// precedence of the dial bind is entirely unexercised.
    #[test]
    fn merging_connector_binds_adopts_only_the_family_the_earlier_file_left_unset() {
        let earlier = ConnectorConfig {
            bind: DualStackBind {
                v4: Some("192.0.2.1".parse().unwrap()),
                v6: None,
            },
        };
        let later = ConnectorConfig {
            bind: DualStackBind {
                v4: None,
                v6: Some("2001:db8::1".parse().unwrap()),
            },
        };
        let merged = earlier.merge(later).unwrap();
        assert_eq!(
            merged.bind.v4,
            Some("192.0.2.1".parse().unwrap()),
            "the earlier file's family must survive a later file that leaves it unset"
        );
        assert_eq!(
            merged.bind.v6,
            Some("2001:db8::1".parse().unwrap()),
            "the family the earlier file left unset is adopted from the later file"
        );
        let neither = ConnectorConfig {
            bind: DualStackBind { v4: None, v6: None },
        };
        let merged = neither.merge(ConnectorConfig::default()).unwrap();
        assert_eq!((merged.bind.v4, merged.bind.v6), (None, None));
    }

    /// A later file that re-binds a family an earlier file already bound is an
    /// attempted override and must be rejected, naming the family that
    /// clashed. `merge_map`'s equivalent rejection is pinned in `server`, but
    /// the connector bind's own rejection has no sibling pin anywhere.
    #[test]
    fn merging_connector_binds_rejects_a_family_bound_in_both_files() {
        let both = |v4: &str, v6: &str| ConnectorConfig {
            bind: DualStackBind {
                v4: Some(v4.parse().unwrap()),
                v6: Some(v6.parse().unwrap()),
            },
        };
        let err = both("192.0.2.1", "2001:db8::1")
            .merge(both("198.51.100.1", "2001:db8::2"))
            .unwrap_err();
        assert_eq!(err, "repeated bind.v4", "the IPv4 clash is named");

        let v6_only = |v6: &str| ConnectorConfig {
            bind: DualStackBind {
                v4: None,
                v6: Some(v6.parse().unwrap()),
            },
        };
        let err = v6_only("2001:db8::1")
            .merge(v6_only("2001:db8::2"))
            .unwrap_err();
        assert_eq!(
            err, "repeated bind.v6",
            "an IPv6-only clash names the v6 bind"
        );
    }
}

#[derive(Debug, Clone)]
pub struct ConnectorResetSignal(pub Notify);
