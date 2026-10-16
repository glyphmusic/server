//! Rate limiters. Counters are short-lived and the only thing the server keeps
//! per client (no personal data beyond an opaque install id / IP in a key).

use crate::traits::{Cache, RateDecision, RateLimiter, Runtime};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Fixed-window limiter held in process memory (native binary, tests).
pub struct MemoryRateLimiter {
    rt: Arc<dyn Runtime>,
    state: Mutex<HashMap<String, (u64, u32)>>,
}

impl MemoryRateLimiter {
    pub fn new(rt: Arc<dyn Runtime>) -> Self {
        Self { rt, state: Mutex::new(HashMap::new()) }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl RateLimiter for MemoryRateLimiter {
    async fn check(&self, key: &str, limit: u32, window_s: u64) -> RateDecision {
        let now = self.rt.now_ms();
        let window_ms = window_s * 1000;
        let mut m = self.state.lock().unwrap();
        if m.len() > 20_000 {
            m.retain(|_, (start, _)| now.saturating_sub(*start) < window_ms);
        }
        let e = m.entry(key.to_string()).or_insert((now, 0));
        if now.saturating_sub(e.0) >= window_ms {
            *e = (now, 0);
        }
        if e.1 >= limit {
            let left_ms = (e.0 + window_ms).saturating_sub(now);
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
