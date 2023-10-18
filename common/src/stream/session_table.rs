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
    enabled: bool,
}

impl SessionTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: Arc::new(RwLock::new(HopSlotMap::with_key())),
            enabled: true,
        }
    }

    #[must_use]
    pub fn new_disabled() -> Self {
        Self {
            map: Arc::new(RwLock::new(HopSlotMap::with_key())),
            enabled: false,
        }
    }

    pub fn set_scope(&self, session: Session) -> SessionGuard {
        let key = self.insert(session);
        SessionGuard { table: self, key }
    }

    #[must_use]
    pub fn insert(&self, session: Session) -> SessionKey {
        if !self.enabled {
            return SessionKey::default();
        }

        let mut map = self.map.write().unwrap();
        map.insert(session)
    }

    pub fn remove(&self, key: SessionKey) -> Option<Session> {
        if !self.enabled {
            return None;
        }

        let mut map = self.map.write().unwrap();
        map.remove(key)
    }

    pub fn sessions(&self) -> Sessions {
        Sessions::new(self.map.read().unwrap())
    }
}

impl Default for SessionTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct SessionGuard<'table> {
    table: &'table SessionTable,
    key: SessionKey,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.table.remove(self.key);
    }
}

#[derive(Debug)]
pub struct Sessions<'lock> {
    map: RwLockReadGuard<'lock, HopSlotMap<SessionKey, Session>>,
}

impl<'lock> Sessions<'lock> {
    pub fn new(map: RwLockReadGuard<'lock, HopSlotMap<SessionKey, Session>>) -> Self {
        Self { map }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        self.map.iter().map(|(_, v)| v)
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
