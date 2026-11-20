//! What the admin dashboard shows over time: how much space the server uses
//! now, and one snapshot a day (D1 `server_settings`, key 'stats_history').

use glyph_server_core::admin::types::{day_of, push_day, DayStats, StorageUsage};
use worker::wasm_bindgen::JsValue;
use worker::D1Database;

/// Bytes in use right now. Caps are filled in by the admin API.
pub async fn usage(db: &D1Database) -> StorageUsage {
    #[derive(serde::Deserialize)]
    struct Audio {
        bytes: f64,
        files: f64,
    }
    #[derive(serde::Deserialize)]
    struct Sync {
        bytes: Option<f64>,
    }
    let audio = match db.prepare("SELECT COALESCE(SUM(size),0) AS bytes, COUNT(*) AS files FROM audio_cache").first::<Audio>(None).await {
        Ok(Some(a)) => Some(a),
        _ => None,
    };
    let sync = match db
        .prepare("SELECT SUM(LENGTH(CAST(value AS BLOB))) AS bytes FROM sync_items WHERE deleted = 0")
        .first::<Sync>(None)
        .await
    {
        Ok(Some(s)) => Some(s.bytes.unwrap_or(0.0) as u64),
        _ => None,
    };
    StorageUsage {
        audio_bytes: audio.as_ref().map(|a| a.bytes as u64),
        audio_files: audio.as_ref().map(|a| a.files as u64),
        audio_cap_bytes: 0,
        db_bytes: crate::storage::size(db).await.ok(),
        db_cap_bytes: 0,
        sync_bytes: sync,
    }
}

/// The saved days, oldest first.
pub async fn read(db: &D1Database) -> Vec<DayStats> {
    #[derive(serde::Deserialize)]
    struct Row {
        value: String,
    }
    match db.prepare("SELECT value FROM server_settings WHERE key = 'stats_history'").first::<Row>(None).await {
        Ok(Some(r)) => serde_json::from_str(&r.value).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// From the minute job: once an hour, adds today's snapshot if it is not
/// there yet. Cheap on every other tick.
pub async fn record(db: &D1Database, now: u64) {
    if now % 3_600_000 >= 60_000 {
        return;
    }
    let mut days = read(db).await;
    let today = day_of(now);
    if days.last().is_some_and(|d| d.day >= today) {
        return;
    }
    #[derive(serde::Deserialize)]
    struct Counts {
        accounts: f64,
        devices: f64,
        active_1d: f64,
    }
    let counts = db
        .prepare(
            "SELECT (SELECT COUNT(*) FROM users) AS accounts, (SELECT COUNT(*) FROM devices) AS devices,
             (SELECT COUNT(DISTINCT user_id) FROM devices WHERE last_seen >= ?1) AS active_1d",
        )
        .bind(&[JsValue::from_f64(now.saturating_sub(86_400_000) as f64)]);
    let counts = match counts {
        Ok(stmt) => stmt.first::<Counts>(None).await.ok().flatten(),
        Err(_) => None,
    };
    let use_ = usage(db).await;
    let snap = DayStats {
        day: today,
        accounts: counts.as_ref().map_or(0, |c| c.accounts as u64),
        devices: counts.as_ref().map_or(0, |c| c.devices as u64),
        active_1d: counts.as_ref().map_or(0, |c| c.active_1d as u64),
        audio_bytes: use_.audio_bytes.unwrap_or(0),
        db_bytes: use_.db_bytes.unwrap_or(0),
        sync_bytes: use_.sync_bytes.unwrap_or(0),
    };
    if !push_day(&mut days, snap) {
        return;
    }
    let Ok(value) = serde_json::to_string(&days) else { return };
    let stmt = db
        .prepare("INSERT INTO server_settings (key, value) VALUES ('stats_history', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(&[value.into()]);
    if let Ok(stmt) = stmt {
        if stmt.run().await.is_err() {
            worker::console_error!("could not save the daily stats");
        }
    }
}
