use std::sync::{Arc, RwLock, RwLockReadGuard};

use slotmap::{new_key_type, HopSlotMap};

#[derive(Debug)]
pub struct SessionTable<T> {
    map: Arc<RwLock<HopSlotMap<SessionKey, T>>>,
    enabled: bool,
}

impl<T> SessionTable<T> {
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

    pub fn set_scope(&self, session: T) -> SessionGuard<T> {
        let key = self.insert(session);
        SessionGuard { table: self, key }
    }

    pub fn set_scope_owned(&self, session: T) -> SessionOwnedGuard<T> {
        let key = self.insert(session);
        SessionOwnedGuard {
            table: self.clone(),
            key,
        }
    }

    #[must_use]
    pub fn insert(&self, session: T) -> SessionKey {
        if !self.enabled {
            return SessionKey::default();
        }

        let mut map = self.map.write().unwrap();
        map.insert(session)
    }

    pub fn remove(&self, key: SessionKey) -> Option<T> {
        if !self.enabled {
            return None;
        }

        let mut map = self.map.write().unwrap();
        map.remove(key)
    }

    pub fn sessions(&self) -> Sessions<T> {
        Sessions::new(self.map.read().unwrap())
    }
}

impl<T> Default for SessionTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for SessionTable<T> {
    fn clone(&self) -> Self {
        Self {
            map: self.map.clone(),
            enabled: self.enabled,
        }
    }
}

#[derive(Debug)]
pub struct SessionGuard<'table, T> {
    table: &'table SessionTable<T>,
    key: SessionKey,
}

impl<T> SessionGuard<'_, T> {
    pub fn inspect_mut(&self, f: fn(&mut T)) {
        inspect_mut(self.table, self.key, f)
    }
}

impl<T> Drop for SessionGuard<'_, T> {
    fn drop(&mut self) {
        self.table.remove(self.key);
    }
}

#[derive(Debug)]
pub struct SessionOwnedGuard<T> {
    table: SessionTable<T>,
    key: SessionKey,
}

impl<T> SessionOwnedGuard<T> {
    pub fn inspect_mut(&self, f: fn(&mut T)) {
        inspect_mut(&self.table, self.key, f)
    }
}

impl<T> Drop for SessionOwnedGuard<T> {
    fn drop(&mut self) {
        self.table.remove(self.key);
    }
}

fn inspect_mut<T>(table: &SessionTable<T>, key: SessionKey, f: fn(&mut T)) {
    let mut map = table.map.write().unwrap();
    let Some(session) = map.get_mut(key) else {
        return;
    };
    f(session)
}

#[derive(Debug)]
pub struct Sessions<'lock, T> {
    map: RwLockReadGuard<'lock, HopSlotMap<SessionKey, T>>,
}

impl<'lock, T> Sessions<'lock, T> {
    pub fn new(map: RwLockReadGuard<'lock, HopSlotMap<SessionKey, T>>) -> Self {
        Self { map }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.map.iter().map(|(_, v)| v)
    }
}

new_key_type! { pub struct SessionKey; }
