use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct CacheCell<T> {
    item: Option<T>,
    last_update: Instant,
    lifetime: Duration,
}

impl<T> CacheCell<T> {
    pub fn new(item: Option<T>, lifetime: Duration) -> Self {
        Self {
            item,
            last_update: Instant::now(),
            lifetime,
        }
    }

    pub fn get(&self) -> Option<&T> {
        if self.last_update.elapsed() > self.lifetime {
            return None;
        }
        self.item.as_ref()
    }

    pub fn set(&mut self, item: T) {
        self.item = Some(item);
        self.last_update = Instant::now();
    }
}
