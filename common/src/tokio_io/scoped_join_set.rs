use std::ops::{Deref, DerefMut};

use tokio::task::JoinSet;

pub struct ScopedJoinSet<T>
where
    T: 'static,
{
    join_set: JoinSet<T>,
}

impl<T> ScopedJoinSet<T> {
    pub fn new() -> Self {
        Self {
            join_set: JoinSet::new(),
        }
    }
}

impl<T> Default for ScopedJoinSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for ScopedJoinSet<T>
where
    T: 'static,
{
    fn drop(&mut self) {
        self.join_set.abort_all();
    }
}

impl<T> Deref for ScopedJoinSet<T> {
    type Target = JoinSet<T>;

    fn deref(&self) -> &Self::Target {
        &self.join_set
    }
}

impl<T> DerefMut for ScopedJoinSet<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.join_set
    }
}
