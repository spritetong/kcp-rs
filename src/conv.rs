use crate::protocol::Kcp;

use ::hashlink::LinkedHashMap;
use ::parking_lot::Mutex;
use ::std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct ConvHistory(Arc<Mutex<Inner>>);

struct Inner {
    map: LinkedHashMap<u32, Instant>,
    timeout: Duration,
}

impl ConvHistory {
    pub fn new(capacity: usize, timeout: Duration) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            map: if capacity == 0 {
                LinkedHashMap::new()
            } else {
                LinkedHashMap::with_capacity(capacity)
            },
            timeout,
        })))
    }

    pub fn allocate(&self, exists: impl Fn(&u32) -> bool) -> u32 {
        let mut guard = self.0.lock();
        Self::update(&mut guard, Instant::now());
        loop {
            let conv = Kcp::rand_conv();
            if !exists(&conv) && !guard.map.contains_key(&conv) {
                break conv;
            }
        }
    }

    pub fn add(&self, conv: u32) {
        let mut guard = self.0.lock();
        let now = Instant::now();
        Self::update(&mut guard, now);
        if Kcp::is_valid_conv(conv) {
            guard.map.insert(conv, now);
        }
    }

    pub fn fill<T>(&self, iter: T)
    where
        T: IntoIterator<Item = u32>,
    {
        let mut guard = self.0.lock();
        let now = Instant::now();
        Self::update(&mut guard, now);
        for conv in iter {
            if Kcp::is_valid_conv(conv) {
                guard.map.insert(conv, now);
            }
        }
    }

    pub fn dump(&self) -> Vec<u32> {
        let mut guard = self.0.lock();
        Self::update(&mut guard, Instant::now());
        guard.map.keys().copied().collect()
    }

    fn update(this: &mut Inner, now: Instant) {
        while let Some((_, time)) = this.map.front() {
            if now.duration_since(*time) < this.timeout {
                break;
            }
            this.map.pop_front();
        }
    }
}
