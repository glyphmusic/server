//! Cache helpers: typed JSON access, a two-level cache, and a simple in-memory
//! implementation (used by tests and as a fallback).

use crate::traits::{Cache, MaybeSendSync, Runtime};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const MINUTE: u64 = 60;
pub const HOUR: u64 = 3600;
pub const DAY: u64 = 86_400;

/// Keep keys short and safe for KV (max 512 bytes): long keys are hashed.
pub fn norm_key(key: &str) -> String {
    if key.len() <= 200 {
        return key.to_string();
    }
    let digest = Sha256::digest(key.as_bytes());
    // By chars: a byte slice could split a multi-byte char and panic.
    let mut s: String = key.chars().take(40).map(|c| if c.is_ascii() { c } else { '_' }).collect();
    s.push('~');
    for b in digest.iter().take(16) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub async fn get_json<T: DeserializeOwned>(cache: &dyn Cache, key: &str) -> Option<T> {
    let bytes = cache.get(&norm_key(key)).await?;
    serde_json::from_slice(&bytes).ok()
}

pub async fn put_json<T: Serialize>(cache: &dyn Cache, key: &str, value: &T, ttl_s: u64) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        cache.put(&norm_key(key), bytes, ttl_s).await;
    }
}

/// Reads L1 first then L2 (promoting hits); writes to both.
/// Worker: L1 = Cache API (per data center), L2 = KV (global).
pub struct TieredCache {
    pub l1: Arc<dyn Cache>,
    pub l2: Arc<dyn Cache>,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Cache for TieredCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        if let Some(v) = self.l1.get(key).await {
            return Some(v);
        }
        let v = self.l2.get(key).await?;
        // Promote with a short TTL since the remaining L2 lifetime is unknown.
        self.l1.put(key, v.clone(), 10 * MINUTE).await;
        Some(v)
    }
    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64) {
        self.l1.put(key, value.clone(), ttl_s).await;
        self.l2.put(key, value, ttl_s).await;
    }
}

/// Ignores writes with a TTL below `min_ttl_s`. Used to keep short-lived
/// entries out of Workers KV, whose free plan allows only 1,000 writes/day.
pub struct MinTtlCache {
    pub inner: Arc<dyn Cache>,
    pub min_ttl_s: u64,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Cache for MinTtlCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        // Short-lived keys (search<=55m, skip=10m, token<=58m) are never stored
        // when min_ttl_s >= 3600; skip the L2 roundtrip on L1 misses.
        if self.min_ttl_s >= HOUR && (key.starts_with("search:") || key.starts_with("skip:") || key.starts_with("token:")) {
            return None;
        }
        self.inner.get(key).await
    }
    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64) {
        if ttl_s >= self.min_ttl_s {
            self.inner.put(key, value, ttl_s).await;
        }
    }
}

/// Unbounded-ish in-memory cache with TTL (capped entry count, drops expired first).
pub struct MemoryCache {
    rt: Arc<dyn Runtime>,
    max_entries: usize,
    map: Mutex<HashMap<String, (u64, Vec<u8>)>>,
}

impl MemoryCache {
    pub fn new(rt: Arc<dyn Runtime>, max_entries: usize) -> Self {
        Self { rt, max_entries, map: Mutex::new(HashMap::new()) }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let now = self.rt.now_ms();
        let mut m = self.map.lock().unwrap();
        match m.get(key) {
            Some((exp, v)) if *exp > now => Some(v.clone()),
            Some(_) => {
                m.remove(key);
                None
            }
            None => None,
        }
    }
    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64) {
        let now = self.rt.now_ms();
        let mut m = self.map.lock().unwrap();
        if m.len() >= self.max_entries {
            m.retain(|_, (exp, _)| *exp > now);
            if m.len() >= self.max_entries {
                m.clear();
            }
        }
        m.insert(key.to_string(), (now + ttl_s * 1000, value));
    }
}

// Silence unused warning on targets where the bound is a no-op.
#[allow(dead_code)]
fn _assert_bounds<T: MaybeSendSync>() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use futures::executor::block_on;

    #[test]
    fn long_key_with_multibyte_char_at_cut_does_not_panic() {
        let key = format!("{}é{}", "a".repeat(39), "b".repeat(300));
        let k = norm_key(&key);
        assert!(k.starts_with(&format!("{}_~", "a".repeat(39))));
    }

    #[test]
    fn long_keys_are_hashed_and_stable() {
        let long = "x".repeat(500);
        let a = norm_key(&long);
        assert!(a.len() < 100);
        assert_eq!(a, norm_key(&long));
        assert_ne!(a, norm_key(&"y".repeat(500)));
        assert_eq!(norm_key("short"), "short");
    }

    #[test]
    fn memory_cache_expires() {
        let rt = Arc::new(MockRuntime::default());
        let c = MemoryCache::new(rt.clone(), 10);
        block_on(async {
            c.put("a", b"1".to_vec(), 5).await;
            assert_eq!(c.get("a").await, Some(b"1".to_vec()));
            rt.advance(6000);
            assert_eq!(c.get("a").await, None);
        });
    }

    #[test]
    fn min_ttl_filters_short_entries() {
        let rt = Arc::new(MockRuntime::default());
        let inner: Arc<dyn Cache> = Arc::new(MemoryCache::new(rt.clone(), 10));
        let c = MinTtlCache { inner: inner.clone(), min_ttl_s: 3600 };
        block_on(async {
            c.put("short", b"1".to_vec(), 600).await;
            c.put("long", b"1".to_vec(), 7200).await;
            assert_eq!(inner.get("short").await, None);
            assert!(inner.get("long").await.is_some());
        });
    }

    #[test]
    fn tiered_promotes() {
        let rt = Arc::new(MockRuntime::default());
        let l1: Arc<dyn Cache> = Arc::new(MemoryCache::new(rt.clone(), 10));
        let l2: Arc<dyn Cache> = Arc::new(MemoryCache::new(rt.clone(), 10));
        let t = TieredCache { l1: l1.clone(), l2: l2.clone() };
        block_on(async {
            l2.put("k", b"v".to_vec(), 100).await;
            assert_eq!(l1.get("k").await, None);
            assert_eq!(t.get("k").await, Some(b"v".to_vec()));
            assert_eq!(l1.get("k").await, Some(b"v".to_vec()));
        });
    }
}
