//! Native implementations of the core traits.

use async_trait::async_trait;
use glyph_server_core::traits::CacheStats;
use glyph_server_core::{Cache, HttpClient, HttpError, HttpRequest, HttpResponse, Method, Runtime};
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct TokioRuntime;

#[async_trait]
impl Runtime for TokioRuntime {
    fn now_ms(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }
    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }
}

#[async_trait]
impl HttpClient for ReqwestClient {
    async fn send(&self, r: HttpRequest) -> Result<HttpResponse, HttpError> {
        // reqwest errors embed the URL (and thus API keys): drop all detail.
        let fail = |_: reqwest::Error| HttpError("request failed".into());
        let mut b = match r.method {
            Method::Post => self.client.post(&r.url),
            _ => self.client.get(&r.url),
        };
        for (k, v) in &r.headers {
            b = b.header(k, v);
        }
        if let Some(body) = r.body {
            b = b.body(body);
        }
        let resp = b.send().await.map_err(fail)?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(fail)?.to_vec();
        Ok(HttpResponse { status, body })
    }
}

/// In-memory LRU with TTL and an optional on-disk layer (one file per key:
/// 8-byte big-endian expiry in ms, then the value).
pub struct LruDiskCache {
    mem: Mutex<LruCache<String, (u64, Vec<u8>)>>,
    dir: Option<PathBuf>,
    rt: TokioRuntime,
    hits: AtomicU64,
    misses: AtomicU64,
}

/// Admin bookkeeping (lockouts, revoked sessions) and rate-limit counters stay
/// in memory only and survive `cache clear`.
fn is_volatile_key(key: &str) -> bool {
    key.starts_with("admin:") || key.starts_with("rl:")
}

impl LruDiskCache {
    pub fn new(entries: usize, dir: Option<PathBuf>) -> Self {
        if let Some(d) = &dir {
            let _ = std::fs::create_dir_all(d);
        }
        Self {
            mem: Mutex::new(LruCache::new(NonZeroUsize::new(entries.max(16)).unwrap())),
            dir,
            rt: TokioRuntime,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        let d = self.dir.as_ref()?;
        let h = Sha256::digest(key.as_bytes());
        let name: String = h.iter().map(|b| format!("{b:02x}")).collect();
        Some(d.join(name))
    }
}

impl LruDiskCache {
    async fn lookup(&self, key: &str) -> Option<Vec<u8>> {
        let now = self.rt.now_ms();
        {
            let mut m = self.mem.lock().unwrap();
            if let Some((exp, v)) = m.get(key) {
                if *exp > now {
                    return Some(v.clone());
                }
                m.pop(key);
            }
        }
        let p = self.path(key)?;
        let raw = tokio::fs::read(&p).await.ok()?;
        if raw.len() < 8 {
            return None;
        }
        let exp = u64::from_be_bytes(raw[..8].try_into().ok()?);
        if exp <= now {
            let _ = tokio::fs::remove_file(&p).await;
            return None;
        }
        let v = raw[8..].to_vec();
        self.mem.lock().unwrap().put(key.to_string(), (exp, v.clone()));
        Some(v)
    }
}

#[async_trait]
impl Cache for LruDiskCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let r = self.lookup(key).await;
        if !is_volatile_key(key) {
            let c = if r.is_some() { &self.hits } else { &self.misses };
            c.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    async fn stats(&self) -> Option<CacheStats> {
        let disk_entries = self.dir.as_ref().map(|d| std::fs::read_dir(d).map(|r| r.count() as u64).unwrap_or(0));
        Some(CacheStats {
            entries: self.mem.lock().unwrap().len() as u64,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            disk_entries,
        })
    }

    async fn clear(&self) -> bool {
        self.mem.lock().unwrap().clear_except_volatile();
        if let Some(d) = &self.dir {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        true
    }

    async fn put(&self, key: &str, value: Vec<u8>, ttl_s: u64) {
        let exp = self.rt.now_ms() + ttl_s * 1000;
        if let Some(p) = self.path(key) {
            // Only persist entries worth keeping across restarts.
            if ttl_s >= 3600 && !is_volatile_key(key) {
                let mut raw = exp.to_be_bytes().to_vec();
                raw.extend_from_slice(&value);
                let _ = tokio::fs::write(&p, raw).await;
            }
        }
        self.mem.lock().unwrap().put(key.to_string(), (exp, value));
    }
}


trait ClearExt {
    fn clear_except_volatile(&mut self);
}

impl ClearExt for LruCache<String, (u64, Vec<u8>)> {
    fn clear_except_volatile(&mut self) {
        let keep: Vec<(String, (u64, Vec<u8>))> = self
            .iter()
            .filter(|(k, _)| is_volatile_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.clear();
        for (k, v) in keep {
            self.put(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_ttl_and_disk_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let c = LruDiskCache::new(100, Some(dir.path().to_path_buf()));
        c.put("short", b"a".to_vec(), 1).await;
        c.put("long", b"b".to_vec(), 7200).await;
        assert_eq!(c.get("short").await, Some(b"a".to_vec()));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(c.get("short").await, None);
        // A fresh instance (restart) still sees the long-lived entry.
        let c2 = LruDiskCache::new(100, Some(dir.path().to_path_buf()));
        assert_eq!(c2.get("long").await, Some(b"b".to_vec()));
        assert_eq!(c2.get("short").await, None);
    }

    #[tokio::test]
    async fn lru_evicts() {
        let c = LruDiskCache::new(16, None);
        for i in 0..40 {
            c.put(&format!("k{i}"), vec![1], 100).await;
        }
        assert_eq!(c.get("k0").await, None);
        assert!(c.get("k39").await.is_some());
    }
}
