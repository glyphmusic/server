//! Rate limiters. Counters are short-lived and the only thing the server keeps
//! per client (no personal data beyond an opaque install id / IP in a key).

use crate::traits::{Cache, RateDecision, RateLimiter, Runtime};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Fixed-window limiter held in process memory (native binary, tests).
pub struct MemoryRateLimiter {
    rt: Arc<dyn Runtime>,
    state: Mutex<HashMap<String, (u64, u32)>>,
    bans: Mutex<HashMap<String, u64>>,
}

impl MemoryRateLimiter {
    pub fn new(rt: Arc<dyn Runtime>) -> Self {
        Self { rt, state: Mutex::new(HashMap::new()), bans: Mutex::new(HashMap::new()) }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl RateLimiter for MemoryRateLimiter {
    async fn blocked(&self, key: &str) -> Option<u64> {
        let now = self.rt.now_ms();
        let mut bans = self.bans.lock().unwrap();
        bans.retain(|_, until| *until > now);
        bans.get(key).map(|until| (until - now).div_ceil(1000))
    }

    async fn penalize(&self, key: &str) {
        if matches!(self.check(&format!("strikes:{key}"), 11, 900).await, RateDecision::Limited { .. }) {
            let now = self.rt.now_ms();
            let mut bans = self.bans.lock().unwrap();
            bans.retain(|_, until| *until > now);
            if bans.len() < 20_000 { bans.entry(key.into()).or_insert(now + 900_000); }
        }
    }

    async fn check(&self, key: &str, limit: u32, window_s: u64) -> RateDecision {
        let now = self.rt.now_ms();
        let window_ms = window_s * 1000;
        let mut m = self.state.lock().unwrap();
        if m.len() >= 20_000 {
            m.retain(|_, (until, _)| *until > now);
            if m.len() >= 20_000 && !m.contains_key(key) {
                return RateDecision::Limited { retry_after_s: 60 };
            }
        }
        let e = m.entry(key.to_string()).or_insert((now.saturating_add(window_ms), 0));
        if now >= e.0 {
            *e = (now.saturating_add(window_ms), 0);
        }
        if e.1 >= limit {
            let left_ms = e.0.saturating_sub(now);
            return RateDecision::Limited { retry_after_s: left_ms.div_ceil(1000).max(1) };
        }
        e.1 += 1;
        RateDecision::Allowed
    }
}

/// Fixed-window limiter on top of any [`Cache`] (Workers KV). Approximate:
/// KV is eventually consistent and read-modify-write is not atomic, which is
/// fine for abuse protection but not for exact accounting.
pub struct CacheRateLimiter {
    cache: Arc<dyn Cache>,
    rt: Arc<dyn Runtime>,
}

impl CacheRateLimiter {
    pub fn new(cache: Arc<dyn Cache>, rt: Arc<dyn Runtime>) -> Self {
        Self { cache, rt }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl RateLimiter for CacheRateLimiter {
    async fn blocked(&self, key: &str) -> Option<u64> {
        let bytes = self.cache.get(&format!("abuse-ban:{key}")).await?;
        let until = u64::from_le_bytes(bytes.as_slice().try_into().ok()?);
        let left = until.saturating_sub(self.rt.now_ms());
        (left > 0).then(|| left.div_ceil(1000))
    }

    async fn penalize(&self, key: &str) {
        if matches!(self.check(&format!("strikes:{key}"), 11, 900).await, RateDecision::Limited { .. }) {
            let until = self.rt.now_ms().saturating_add(900_000);
            self.cache.put(&format!("abuse-ban:{key}"), until.to_le_bytes().to_vec(), 900).await;
        }
    }
    async fn check(&self, key: &str, limit: u32, window_s: u64) -> RateDecision {
        let now = self.rt.now_ms();
        let window_ms = window_s * 1000;
        let idx = now / window_ms;
        let k = format!("rl:{key}:{idx}");
        let count = self
            .cache
            .get(&k)
            .await
            .and_then(|b| <[u8; 4]>::try_from(b.as_slice()).ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0);
        if count >= limit {
            let left_ms = ((idx + 1) * window_ms).saturating_sub(now);
            return RateDecision::Limited { retry_after_s: left_ms.div_ceil(1000).max(1) };
        }
        self.cache.put(&k, (count + 1).to_le_bytes().to_vec(), window_s.max(60) * 2).await;
        RateDecision::Allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::MemoryCache;
    use crate::testutil::MockRuntime;
    use futures::executor::block_on;

    #[test]
    fn both_limiters_ban_repeated_violations_without_extending_on_reads() {
        let rt = Arc::new(MockRuntime::default());
        let cache = Arc::new(MemoryCache::new(rt.clone(), 100));
        let limiters: Vec<Box<dyn RateLimiter>> = vec![
            Box::new(MemoryRateLimiter::new(rt.clone())),
            Box::new(CacheRateLimiter::new(cache, rt.clone())),
        ];
        block_on(async {
            for rl in &limiters {
                for _ in 0..11 { rl.penalize("ip:one").await; }
                assert_eq!(rl.blocked("ip:one").await, None);
                rl.penalize("ip:one").await;
                assert_eq!(rl.blocked("ip:one").await, Some(900));
                assert_eq!(rl.blocked("ip:other").await, None);
            }
            rt.advance(60_000);
            for rl in &limiters { assert_eq!(rl.blocked("ip:one").await, Some(840)); }
            rt.advance(840_000);
            for rl in &limiters { assert_eq!(rl.blocked("ip:one").await, None); }
        });
    }

    #[test]
    fn memory_cleanup_preserves_long_windows_and_bounds_cardinality() {
        let rt = Arc::new(MockRuntime::default());
        let rl = MemoryRateLimiter::new(rt.clone());
        block_on(async {
            rl.check("long", 1, 900).await;
            for i in 0..20_000 { rl.check(&format!("short:{i}"), 1, 60).await; }
            assert!(rl.state.lock().unwrap().len() <= 20_000);
            rt.advance(61_000);
            assert_eq!(rl.check("new", 1, 60).await, RateDecision::Allowed);
            assert!(matches!(rl.check("long", 1, 900).await, RateDecision::Limited { .. }));
        });
    }

    #[test]
    fn memory_limiter_blocks_then_resets() {
        let rt = Arc::new(MockRuntime::default());
        let rl = MemoryRateLimiter::new(rt.clone());
        block_on(async {
            for _ in 0..3 {
                assert_eq!(rl.check("a", 3, 60).await, RateDecision::Allowed);
            }
            rt.advance(10_000);
            assert_eq!(rl.check("a", 3, 60).await, RateDecision::Limited { retry_after_s: 50 });
            // other keys are independent
            assert_eq!(rl.check("b", 3, 60).await, RateDecision::Allowed);
            rt.advance(50_000);
            assert_eq!(rl.check("a", 3, 60).await, RateDecision::Allowed);
        });
    }

    #[test]
    fn cache_limiter_counts_per_window() {
        let rt = Arc::new(MockRuntime::default());
        let cache: Arc<dyn Cache> = Arc::new(MemoryCache::new(rt.clone(), 100));
        let rl = CacheRateLimiter::new(cache, rt.clone());
        block_on(async {
            assert_eq!(rl.check("ip1", 2, 60).await, RateDecision::Allowed);
            assert_eq!(rl.check("ip1", 2, 60).await, RateDecision::Allowed);
            assert!(matches!(rl.check("ip1", 2, 60).await, RateDecision::Limited { .. }));
            rt.advance(61_000);
            assert_eq!(rl.check("ip1", 2, 60).await, RateDecision::Allowed);
        });
    }
}
