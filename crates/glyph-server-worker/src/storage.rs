//! Independent D1 pressure guard. Use actual D1 size metadata, not R2 usage.
//! Conservative envelopes reserve room for SQLite pages/indexes before writes.
use std::future::Future;
use worker::{D1Database, wasm_bindgen::JsValue};
pub const RESERVE: &str = r#"WITH budget AS (
 SELECT COALESCE((SELECT json_extract(value,'$.d1_max_mb')*1000000 FROM server_settings WHERE key='limits'),500000000) AS cap
)
INSERT INTO d1_storage_reservations(token,bytes,created)
SELECT ?1,?2,?3 FROM budget WHERE
MAX(?4,(SELECT bytes FROM d1_storage_watermark WHERE id=1)) + COALESCE((SELECT SUM(bytes) FROM d1_storage_reservations),0) + ?2
<= cap-MIN(50000000,cap/10) RETURNING token"#;

pub async fn size(db: &D1Database) -> worker::Result<u64> {
    db.prepare("SELECT 1").run().await?.meta()?.and_then(|m| m.size_after)
        .map(|n| n as u64).ok_or_else(|| worker::Error::RustError("D1 size metadata unavailable".into()))
}

pub async fn guarded<T>(db: &D1Database, envelope: u64, work: impl Future<Output=worker::Result<T>>) -> worker::Result<T> {
    let bytes = size(db).await?;
    let mut entropy = [0u8;16];
    getrandom::getrandom(&mut entropy).map_err(|_| worker::Error::RustError("storage entropy unavailable".into()))?;
    let token: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
    #[derive(serde::Deserialize)] struct Reserved { token: String }
    let row = db.prepare(RESERVE).bind(&[JsValue::from_str(&token),JsValue::from_f64(envelope as f64),
        JsValue::from_f64(worker::Date::now().as_millis() as f64),JsValue::from_f64(bytes as f64)])?
        .first::<Reserved>(None).await?;
    if !row.is_some_and(|r| r.token == token) {
        return Err(worker::Error::RustError("D1 storage headroom exhausted".into()));
    }
    // On uncertain failure/cancellation retain capacity rather than risk an
    // overlapping write which may still complete at the database.
    let result = work.await;
    if result.is_err() && !matches!(&result, Err(worker::Error::D1(_))) {
        return result;
    }
    // Publish the committed size before releasing the envelope. Otherwise a
    // concurrent caller could reserve against an old pre-write measurement.
    let after = size(db).await?;
    db.batch(vec![
        db.prepare("UPDATE d1_storage_watermark SET bytes=MAX(bytes,?1),revision=revision+1 WHERE id=1").bind(&[JsValue::from_f64(after as f64)])?,
        db.prepare("DELETE FROM d1_storage_reservations WHERE token=?1").bind(&[JsValue::from_str(&token)])?,
    ]).await?;
    result
}
/// Only expired temporary records are eligible. Live accounts, credentials,
/// sync data and tombstones are never discarded to satisfy a space ceiling.
pub async fn maintain(db: &D1Database) -> worker::Result<()> {
    let now = JsValue::from_f64(worker::Date::now().as_millis() as f64);
    let queries = [
        "DELETE FROM d1_storage_reservations WHERE token IN (SELECT token FROM d1_storage_reservations WHERE created<=?1-300000 LIMIT 1000)",
        "DELETE FROM audio_variant_jobs WHERE track_id IN (SELECT track_id FROM audio_variant_jobs WHERE expires<=?1 LIMIT 1000)",
        "DELETE FROM abuse_limits WHERE key IN (SELECT key FROM abuse_limits WHERE expires<=?1 LIMIT 1000)",
        "DELETE FROM logins WHERE id IN (SELECT id FROM logins WHERE expires<=?1 LIMIT 1000)",
        "DELETE FROM handle_holds WHERE handle IN (SELECT handle FROM handle_holds WHERE until<=?1 LIMIT 1000)",
        "DELETE FROM audio_variants WHERE rowid IN (SELECT rowid FROM audio_variants WHERE created<=?1-COALESCE((SELECT json_extract(value,'$.audio_cache_days')*86400000 FROM server_settings WHERE key='limits'),1296000000) LIMIT 1000)",
    ];
    let stmts = queries.iter().map(|q| db.prepare(*q).bind(&[now.clone()])).collect::<worker::Result<Vec<_>>>()?;
    db.batch(stmts).await?;
    #[derive(serde::Deserialize)] struct Revision { revision: u64 }
    let measured = db.prepare("SELECT revision FROM d1_storage_watermark WHERE id=1").run().await?;
    if let (Some(bytes), Some(revision)) = (
        measured.meta()?.and_then(|m| m.size_after),
        measured.results::<Revision>()?.first().map(|r| r.revision),
    ) {
        db.prepare("UPDATE d1_storage_watermark SET bytes=?1,revision=revision+1 WHERE id=1 AND revision=?2 AND NOT EXISTS(SELECT 1 FROM d1_storage_reservations)")
            .bind(&[JsValue::from_f64(bytes as f64),JsValue::from_f64(revision as f64)])?.run().await?;
        worker::console_log!("D1 database bytes={}",bytes);
    }
    Ok(())
}
