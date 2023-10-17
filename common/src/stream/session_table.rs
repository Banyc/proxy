use std::{
    fmt,
    net::SocketAddr,
    sync::{Arc, RwLock, RwLockReadGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use slotmap::{new_key_type, HopSlotMap};

use super::addr::StreamAddr;

#[derive(Debug, Clone)]
pub struct SessionTable {
    map: Arc<RwLock<HopSlotMap<SessionKey, Session>>>,
}

impl SessionTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: Arc::new(RwLock::new(HopSlotMap::with_key())),
        }
    }

    #[must_use]
    pub fn insert(&self, session: Session) -> SessionKey {
        let mut map = self.map.write().unwrap();
        map.insert(session)
    }

    pub fn remove(&self, key: SessionKey) -> Option<Session> {
        let mut map = self.map.write().unwrap();
        map.remove(key)
    }

    pub fn inner(&self) -> RwLockReadGuard<'_, HopSlotMap<SessionKey, Session>> {
        self.map.read().unwrap()
    }
}

impl Default for SessionTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub start: SystemTime,
    pub destination: StreamAddr,
    pub upstream_local: SocketAddr,
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} -> {}",
            self.start.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            self.upstream_local,
            self.destination
        )
    }
}

new_key_type! { pub struct SessionKey; }
