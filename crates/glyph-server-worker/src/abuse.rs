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
#[derive(serde::Deserialize)]
struct QuotaRow { key: String, count: u32, expires: u64 }
fn num(n: u64) -> JsValue { JsValue::from_f64(n as f64) }
fn key(k: &str) -> JsValue { JsValue::from_str(&glyph_server_core::access::token_hash(k)) }

#[async_trait(?Send)]
impl RateLimiter for D1Limiter {
    async fn check_admin(&self, key: &str, limit: u32, window_s: u64) -> RateDecision {
        self.admin.check(key, limit, window_s).await
    }
    async fn check(&self, k: &str, limit: u32, window_s: u64) -> RateDecision {
        // D1 down or over its daily write quota: the Cache API limiter takes
        // over, so the whole site does not answer 429 until the quota resets.
        let Some(db) = &self.db else { return self.admin.check(k, limit, window_s).await; };
        let now = self.rt.now_ms();
        let until = now.saturating_add(window_s.max(1).saturating_mul(1000));
        let Ok(stmt) = db.prepare(
            "INSERT INTO abuse_limits(key,count,expires) VALUES (?1,1,?2) \
             ON CONFLICT(key) DO UPDATE SET \
             count=CASE WHEN expires<=?3 THEN 1 ELSE MIN(count+1,?4) END, \
             expires=CASE WHEN expires<=?3 THEN ?2 ELSE expires END RETURNING count,expires"
        ).bind(&[key(k), num(until), num(now), num(u64::from(limit)+1)]) else { return self.admin.check(k, limit, window_s).await; };
        let Ok(Some(row)) = stmt.first::<Row>(None).await else { return self.admin.check(k, limit, window_s).await; };
        if row.count > limit {
            RateDecision::Limited { retry_after_s: row.expires.saturating_sub(now).div_ceil(1000).max(1) }
        } else { RateDecision::Allowed }
    }

    async fn blocked(&self, k: &str) -> Option<u64> {
        // Unavailable storage falls back to the Cache API bans, never to no check.
        let Some(db) = &self.db else { return self.admin.blocked(k).await; };
        let Ok(stmt) = db.prepare("SELECT count,expires FROM abuse_limits WHERE key=?1")
            .bind(&[key(&format!("ban:{k}"))]) else { return self.admin.blocked(k).await; };
        match stmt.first::<Row>(None).await {
            Ok(Some(row)) => {
                let left = row.expires.saturating_sub(self.rt.now_ms());
                (left > 0).then(|| left.div_ceil(1000))
            }
            Ok(None) => None,
            Err(_) => self.admin.blocked(k).await,
        }
    }

    async fn penalize(&self, k: &str) {
        if matches!(self.check(&format!("strikes:{k}"), 11, 900).await, RateDecision::Limited { .. }) {
            let Some(db) = &self.db else { return self.admin.penalize(k).await; };
            let now = self.rt.now_ms();
            if let Ok(stmt) = db.prepare("INSERT INTO abuse_limits(key,count,expires) VALUES (?1,0,?2) ON CONFLICT(key) DO UPDATE SET expires=CASE WHEN expires<=?3 THEN ?2 ELSE expires END")
                .bind(&[key(&format!("ban:{k}")), num(now.saturating_add(900_000)), num(now)]) {
                if stmt.run().await.is_err() {
                    self.admin.penalize(k).await;
                }
            }
        }
    }

    async fn admit_rules(&self, rules: &[(String, String, u32)], window_s: u64) -> Result<(), (bool, u64)> {
        if rules.is_empty() { return Ok(()); }
        let Some(db) = &self.db else { return self.admin.admit_rules(rules, window_s).await; };
        let now = self.rt.now_ms();
        let until = now.saturating_add(window_s.max(1).saturating_mul(1000));
        let mut principals: Vec<&str> = Vec::with_capacity(rules.len());
        for (_, p, _) in rules {
            if !principals.contains(&p.as_str()) {
                principals.push(p.as_str());
            }
        }
        let ban_placeholders: Vec<String> = (1..=principals.len()).map(|i| format!("?{i}")).collect();
        let ban_sql = format!(
            "SELECT count,expires FROM abuse_limits WHERE key IN ({}) AND expires>?{} ORDER BY expires DESC LIMIT 1",
            ban_placeholders.join(","),
            principals.len() + 1
        );
        let mut ban_args: Vec<JsValue> = principals.iter().map(|p| key(&format!("ban:{p}"))).collect();
        ban_args.push(num(now));
        let Ok(ban_stmt) = db.prepare(&ban_sql).bind(&ban_args) else { return self.admin.admit_rules(rules, window_s).await; };

        let mut values_sql = Vec::with_capacity(rules.len());
        let mut quota_args = Vec::with_capacity(rules.len() + 2);
        quota_args.push(num(until));
        quota_args.push(num(now));
        for (i, (quota_key, _, _)) in rules.iter().enumerate() {
            values_sql.push(format!("(?{},1,?1)", i + 3));
            quota_args.push(key(quota_key));
        }
        let upsert_sql = format!(
            "INSERT INTO abuse_limits(key,count,expires) VALUES {} \
             ON CONFLICT(key) DO UPDATE SET \
             count=CASE WHEN expires<=?2 THEN 1 ELSE MIN(count+1,2147483647) END, \
             expires=CASE WHEN expires<=?2 THEN ?1 ELSE expires END RETURNING key,count,expires",
            values_sql.join(",")
        );
        let Ok(upsert_stmt) = db.prepare(&upsert_sql).bind(&quota_args) else { return self.admin.admit_rules(rules, window_s).await; };

        let Ok(results) = db.batch(vec![ban_stmt, upsert_stmt]).await else { return self.admin.admit_rules(rules, window_s).await; };
        if results.len() != 2 { return self.admin.admit_rules(rules, window_s).await; }
        let Ok(mut bans) = results[0].results::<Row>() else { return self.admin.admit_rules(rules, window_s).await; };
        if let Some(row) = bans.pop() {
            let left = row.expires.saturating_sub(now);
            if left > 0 {
                return Err((true, left.div_ceil(1000)));
            }
        }
        let Ok(rows) = results[1].results::<QuotaRow>() else { return self.admin.admit_rules(rules, window_s).await; };
        if rows.len() != rules.len() { return self.admin.admit_rules(rules, window_s).await; }
        // SQLite RETURNING order is unspecified; associate quotas by their key.
        for (quota_key, principal, limit) in rules {
            let hash = glyph_server_core::access::token_hash(quota_key);
            let Some(row) = rows.iter().find(|row| row.key == hash) else { return self.admin.admit_rules(rules, window_s).await; };
            if row.count > *limit {
                self.penalize(principal).await;
                if let Some(retry) = self.blocked(principal).await {
                    return Err((true, retry));
                }
                return Err((false, row.expires.saturating_sub(now).div_ceil(1000).max(1)));
            }
        }
        Ok(())
    }
}
