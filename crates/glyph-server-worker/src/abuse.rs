//! D1 serializes counter updates; Cache API/KV read-modify-write is insufficient.
use std::sync::Arc;
use async_trait::async_trait;
use glyph_server_core::{RateDecision, RateLimiter, Runtime};
use worker::{D1Database, wasm_bindgen::JsValue};

pub struct D1Limiter {
    pub db: Option<D1Database>,
    pub rt: Arc<dyn Runtime>,
    pub admin: Arc<dyn RateLimiter>,
}
#[derive(serde::Deserialize)]
struct Row { count: u32, expires: u64 }
fn num(n: u64) -> JsValue { JsValue::from_f64(n as f64) }
fn key(k: &str) -> JsValue { JsValue::from_str(&glyph_server_core::access::token_hash(k)) }

#[async_trait(?Send)]
impl RateLimiter for D1Limiter {
    async fn check_admin(&self, key: &str, limit: u32, window_s: u64) -> RateDecision {
        self.admin.check(key, limit, window_s).await
    }
    async fn check(&self, k: &str, limit: u32, window_s: u64) -> RateDecision {
        let denied = RateDecision::Limited { retry_after_s: 60 };
        let Some(db) = &self.db else { return denied; };
        let now = self.rt.now_ms();
        let until = now.saturating_add(window_s.max(1).saturating_mul(1000));
        let Ok(stmt) = db.prepare(
            "INSERT INTO abuse_limits(key,count,expires) VALUES (?1,1,?2) \
             ON CONFLICT(key) DO UPDATE SET \
             count=CASE WHEN expires<=?3 THEN 1 ELSE MIN(count+1,?4) END, \
             expires=CASE WHEN expires<=?3 THEN ?2 ELSE expires END RETURNING count,expires"
        ).bind(&[key(k), num(until), num(now), num(u64::from(limit)+1)]) else { return denied; };
        let Ok(cleanup) = db.prepare("DELETE FROM abuse_limits WHERE key IN (SELECT key FROM abuse_limits WHERE expires<=?1 LIMIT 64)")
            .bind(&[num(now)]) else { return denied; };
        let Ok(results) = super::storage::guarded(db, 64*1024, db.batch(vec![cleanup, stmt])).await else { return denied; };
        let Some(row) = results.last().and_then(|r| r.results::<Row>().ok()).and_then(|mut r| r.pop()) else { return denied; };
        if row.count > limit {
            RateDecision::Limited { retry_after_s: row.expires.saturating_sub(now).div_ceil(1000).max(1) }
        } else { RateDecision::Allowed }
    }

    async fn blocked(&self, k: &str) -> Option<u64> {
        // Unavailable storage never silently grants access.
        let Some(db) = &self.db else { return Some(60); };
        let Ok(stmt) = db.prepare("SELECT count,expires FROM abuse_limits WHERE key=?1")
            .bind(&[key(&format!("ban:{k}"))]) else { return Some(60); };
        match stmt.first::<Row>(None).await {
            Ok(Some(row)) => {
                let left = row.expires.saturating_sub(self.rt.now_ms());
                (left > 0).then(|| left.div_ceil(1000))
            }
            Ok(None) => None,
            Err(_) => Some(60),
        }
    }

    async fn penalize(&self, k: &str) {
        if matches!(self.check(&format!("strikes:{k}"), 11, 900).await, RateDecision::Limited { .. }) {
            let Some(db) = &self.db else { return; };
            let now = self.rt.now_ms();
            if let Ok(stmt) = db.prepare("INSERT INTO abuse_limits(key,count,expires) VALUES (?1,0,?2) ON CONFLICT(key) DO UPDATE SET expires=CASE WHEN expires<=?3 THEN ?2 ELSE expires END")
                .bind(&[key(&format!("ban:{k}")), num(now.saturating_add(900_000)), num(now)]) {
                let _ = super::storage::guarded(db, 64*1024, stmt.run()).await;
            }
        }
    }
}
