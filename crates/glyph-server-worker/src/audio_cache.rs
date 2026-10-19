//! Account actual R2 bytes, including protected uploads. Reserve before writes;
//! release only after confirmed deletion. Ambiguous writes remain charged.
use worker::{D1Database, Bucket, wasm_bindgen::JsValue};

pub const RESERVE: &str = r#"WITH budget AS (
 SELECT COALESCE((SELECT json_extract(value,'$.audio_cache_max_gb')*1000000000
 FROM server_settings WHERE key='limits'),?5) AS cap
)
INSERT INTO audio_cache(key,size,created,state,generation)
SELECT ?1,?2,?3,'pending',?4 FROM budget
WHERE (SELECT ready FROM audio_cache_inventory WHERE id=1)=1
AND COALESCE((SELECT SUM(size) FROM audio_cache),0)+?2 <= cap-MIN(500000000,cap/2)
ON CONFLICT(key) DO NOTHING RETURNING key"#;
pub const CLAIM: &str = r#"UPDATE audio_cache SET state='deleting'
WHERE key=(SELECT key FROM audio_cache WHERE state='ready'
AND (key GLOB 'audio/*' OR key GLOB 'covers/*') AND
(created<=?1 OR (SELECT COALESCE(SUM(size),0) FROM audio_cache)>?2)
ORDER BY (created<=?1) DESC,hits,last_access,created,key LIMIT 1)
AND state='ready' RETURNING key,generation"#;
pub const OBSERVE: &str = r#"INSERT INTO audio_cache(key,size,created,state,generation,seen)
SELECT json_extract(value,'$.key'),json_extract(value,'$.size'),json_extract(value,'$.created'),'ready',COALESCE(json_extract(value,'$.generation'),'inventory'),?2
FROM json_each(?1) WHERE 1
ON CONFLICT(key) DO UPDATE SET size=excluded.size,created=excluded.created,seen=excluded.seen,
state=CASE WHEN audio_cache.state='pending' AND audio_cache.generation=excluded.generation AND audio_cache.size=excluded.size THEN 'ready' ELSE audio_cache.state END
WHERE audio_cache.state='ready' OR (audio_cache.state='pending' AND audio_cache.generation=excluded.generation AND audio_cache.size=excluded.size)"#;

fn n(v: u64) -> JsValue { JsValue::from_f64(v as f64) }
fn s(v: &str) -> JsValue { JsValue::from_str(v) }
fn now() -> u64 { worker::Date::now().as_millis() }
pub fn target(cap: u64) -> u64 { cap.saturating_sub(500_000_000.min(cap/2)) }
#[derive(serde::Deserialize)]
struct Entry { key: String, generation: String }
#[derive(serde::Deserialize)]
struct Inventory { cursor: Option<String>, ready: u32, scan_id: u64 }

/// One persisted inventory page. Hourly scans recount actual objects, not just
/// the ledger. New writes pause while a scan is incomplete; reads continue.
pub async fn inventory(db: &D1Database, bucket: &Bucket) -> worker::Result<bool> {
    let state = db.prepare("SELECT cursor,ready,scan_id FROM audio_cache_inventory WHERE id=1").first::<Inventory>(None).await?
        .ok_or_else(|| worker::Error::RustError("storage inventory missing".into()))?;
    if state.ready == 1 { return Ok(true); }
    super::storage::guarded(db, 4*1024*1024, inventory_inner(db, bucket, state)).await
}
async fn inventory_inner(db: &D1Database, bucket: &Bucket, state: Inventory) -> worker::Result<bool> {
    let mut list = bucket.list().limit(1000).include(vec![worker::Include::CustomMetadata]);
    if let Some(cursor) = &state.cursor { list = list.cursor(cursor.clone()); }
    let page = list.execute().await?;
    let objects: Vec<_> = page.objects().iter().map(|obj| serde_json::json!({
        "key":obj.key(),"size":obj.size(),"created":obj.uploaded().as_millis(),
        "generation":obj.custom_metadata().ok().and_then(|m|m.get("generation").cloned())
    })).collect();
    db.prepare(OBSERVE).bind(&[s(&serde_json::to_string(&objects)?),n(state.scan_id)])?.run().await?;
    db.prepare("UPDATE audio_cache_inventory SET cursor=?1,ready=?2,finished=CASE WHEN ?2=1 THEN ?4 ELSE finished END WHERE id=1 AND cursor IS ?3 AND ready=0 AND scan_id=?5")
        .bind(&[page.cursor().map(|v| s(&v)).unwrap_or(JsValue::NULL), n(u64::from(!page.truncated())),
            state.cursor.as_deref().map(s).unwrap_or(JsValue::NULL),n(now()),n(state.scan_id)])?.run().await?;
    if !page.truncated() {
        // Only completed scans may discard absent ready objects. Pending or
        // interrupted writes retain their reservations conservatively.
        db.prepare("DELETE FROM audio_cache WHERE state='ready' AND seen<>?1 AND created<?1 AND (SELECT ready FROM audio_cache_inventory WHERE id=1)=1")
            .bind(&[n(state.scan_id)])?.run().await?;
    }
    Ok(!page.truncated())
}

pub async fn touch(db: &D1Database, key: &str) {
    if let Ok(stmt) = db.prepare("UPDATE audio_cache SET hits=MIN(hits+1,2147483647),last_access=?1 WHERE key=?2 AND state='ready'")
        .bind(&[n(now()),s(key)]) { let _ = stmt.run().await; }
}

/// Bounded eviction; maintenance resumes until no victim remains. Protected
/// user uploads count toward the budget but can never be selected as victims.
pub async fn trim(db: &D1Database, bucket: &Bucket, days: u32, max_bytes: u64) -> worker::Result<bool> {
    let cutoff = now().saturating_sub(u64::from(days)*86_400_000);
    for _ in 0..10 {
        let Some(old) = db.prepare(CLAIM).bind(&[n(cutoff), n(max_bytes)])?.first::<Entry>(None).await? else { return Ok(true); };
        if !(old.key.starts_with("audio/") || old.key.starts_with("covers/")) {
            return Err(worker::Error::RustError("invalid cache key".into()));
        }
        if let Err(e) = bucket.delete(&old.key).await {
            db.prepare("UPDATE audio_cache SET state='ready' WHERE key=?1 AND generation=?2 AND state='deleting'")
                .bind(&[s(&old.key), s(&old.generation)])?.run().await?;
            return Err(e);
        }
        db.prepare("DELETE FROM audio_cache WHERE key=?1 AND generation=?2 AND state='deleting'")
            .bind(&[s(&old.key), s(&old.generation)])?.run().await?;
    }
    Ok(false)
}

pub async fn store(db: &D1Database, bucket: &Bucket, key: &str, bytes: Vec<u8>, content_type: String, days: u32, cap: u64) -> worker::Result<bool> {
    super::storage::guarded(db, 64*1024, store_inner(db,bucket,key,bytes,content_type,days,cap)).await
}
async fn store_inner(db: &D1Database, bucket: &Bucket, key: &str, bytes: Vec<u8>, content_type: String, days: u32, cap: u64) -> worker::Result<bool> {
    if !inventory(db, bucket).await? { return Ok(false); }
    let size = bytes.len() as u64;
    if size == 0 || size > 32*1024*1024 || size > target(cap) { return Ok(false); }
    trim(db, bucket, days, target(cap).saturating_sub(size)).await?;
    // Blob keys are immutable hashes/track IDs. Never race an existing writer.
    #[derive(serde::Deserialize)] struct Existing { state: String }
    if let Some(row) = db.prepare("SELECT state FROM audio_cache WHERE key=?1").bind(&[s(key)])?.first::<Existing>(None).await? {
        return Ok(row.state == "ready");
    }
    let mut random = [0u8;16];
    getrandom::getrandom(&mut random).map_err(|_| worker::Error::RustError("cache entropy unavailable".into()))?;
    let generation: String = random.iter().map(|b| format!("{b:02x}")).collect();
    #[derive(serde::Deserialize)] struct Reserved { key: String }
    let reserved = db.prepare(RESERVE).bind(&[s(key),n(size),n(now()),s(&generation),n(cap)])?
        .first::<Reserved>(None).await?;
    if !reserved.is_some_and(|r| r.key == key) { return Ok(false); }
    let meta = worker::HttpMetadata { content_type: Some(content_type), ..Default::default() };
    bucket.put(key, bytes).http_metadata(meta).custom_metadata(std::collections::HashMap::from([("generation".to_string(),generation.clone())])).execute().await?;
    db.prepare("UPDATE audio_cache SET state='ready',seen=(SELECT scan_id FROM audio_cache_inventory WHERE id=1) WHERE key=?1 AND generation=?2 AND state='pending'")
        .bind(&[s(key),s(&generation)])?.run().await?;
    Ok(true)
}
