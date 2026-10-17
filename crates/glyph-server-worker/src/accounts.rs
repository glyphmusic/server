//! D1 / R2 implementations of the account storage seams.
//!
//! Every rule that must be atomic (handle uniqueness, last-writer-wins,
//! revision order) runs inside one D1 batch, which D1 executes as a single
//! transaction. Pushes send all items as one JSON parameter (`json_each`), so a
//! push costs a handful of queries no matter how many items it carries.

use async_trait::async_trait;
use glyph_server_core::accounts::store::{
    AccountStore, BlobStore, Device, Item, Login, Passkey, Profile, Share, StoreError, StoreResult, User,
};
use glyph_server_core::accounts::Entropy;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

fn err(_: worker::Error) -> StoreError {
    // Never forward D1 error texts (they can contain SQL / values).
    StoreError("d1 failed".into())
}

fn s(v: &str) -> JsValue {
    JsValue::from_str(v)
}
fn n(v: u64) -> JsValue {
    JsValue::from_f64(v as f64)
}
fn opt(v: &Option<String>) -> JsValue {
    v.as_deref().map(JsValue::from_str).unwrap_or(JsValue::NULL)
}

pub struct D1Store {
    pub db: D1Database,
}

impl D1Store {
    fn stmt(&self, sql: &str, args: &[JsValue]) -> StoreResult<D1PreparedStatement> {
        self.db.prepare(sql).bind(args).map_err(err)
    }

    async fn first<T: DeserializeOwned>(&self, sql: &str, args: &[JsValue]) -> StoreResult<Option<T>> {
        self.stmt(sql, args)?.first::<T>(None).await.map_err(err)
    }

    async fn all<T: DeserializeOwned>(&self, sql: &str, args: &[JsValue]) -> StoreResult<Vec<T>> {
        self.stmt(sql, args)?.all().await.map_err(err)?.results::<T>().map_err(err)
    }

    async fn run(&self, sql: &str, args: &[JsValue]) -> StoreResult<usize> {
        let r = self.stmt(sql, args)?.run().await.map_err(err)?;
        Ok(r.meta().map_err(err)?.and_then(|m| m.changes).unwrap_or(0))
    }

    async fn batch(&self, stmts: Vec<D1PreparedStatement>) -> StoreResult<Vec<worker::D1Result>> {
        self.db.batch(stmts).await.map_err(err)
    }
}

fn changes(r: &worker::D1Result) -> usize {
    r.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0)
}

#[derive(Deserialize)]
struct UserRow {
    id: String,
    handle: String,
    created_at: u64,
    device_limit: u8,
    device_limit_set: u8,
    rev: u64,
    purged_rev: u64,
    recovery_hash: String,
    handle_changed_at: u64,
    pw_salt: Option<String>,
    pw_hash: Option<String>,
}
impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            handle: r.handle,
            created_at: r.created_at,
            device_limit: r.device_limit,
            device_limit_set: r.device_limit_set,
            rev: r.rev,
            purged_rev: r.purged_rev,
            recovery_hash: r.recovery_hash,
            handle_changed_at: r.handle_changed_at,
            pw_salt: r.pw_salt,
            pw_hash: r.pw_hash,
        }
    }
}

#[derive(Deserialize)]
struct PasskeyRow {
    cred_id: String,
    user_id: String,
    alg: i64,
    public_key: String,
    sign_count: u32,
    created_at: u64,
}

#[derive(Deserialize)]
struct DeviceRow {
    id: String,
    user_id: String,
    name: String,
    platform: String,
    created_at: u64,
    last_seen: u64,
    access_hash: String,
    access_exp: u64,
    refresh_hash: String,
    refresh_exp: u64,
}
impl From<DeviceRow> for Device {
    fn from(r: DeviceRow) -> Self {
        Device {
            id: r.id,
            user_id: r.user_id,
            name: r.name,
            platform: r.platform,
            created_at: r.created_at,
            last_seen: r.last_seen,
            access_hash: r.access_hash,
            access_exp: r.access_exp,
            refresh_hash: r.refresh_hash,
            refresh_exp: r.refresh_exp,
        }
    }
}

#[derive(Deserialize)]
struct LoginRow {
    id: String,
    poll_hash: String,
    user_code: String,
    device_name: String,
    platform: String,
    challenge: Option<String>,
    pending_handle: Option<String>,
    pending_user_id: Option<String>,
    user_id: Option<String>,
    page_hash: Option<String>,
    expires: u64,
    link: u8,
}

#[derive(Deserialize)]
struct ItemRow {
    kind: String,
    key: String,
    value: Option<String>,
    updated_at: u64,
    device_id: String,
    deleted: u8,
    rev: u64,
}
impl From<ItemRow> for Item {
    fn from(r: ItemRow) -> Self {
        Item { kind: r.kind, key: r.key, value: r.value, updated_at: r.updated_at, device_id: r.device_id, deleted: r.deleted != 0, rev: r.rev }
    }
}

#[derive(Deserialize)]
struct RevRow {
    rev: u64,
}

/// `device_limit` is the limit in force: the account's own one, else the
/// operator default from `settings`, else store::DEFAULT_DEVICE_LIMIT (5).
const USER_COLS: &str = "id, handle, created_at,
    COALESCE(NULLIF(device_limit, 0), (SELECT CAST(value AS INTEGER) FROM settings WHERE key = 'default_device_limit'), 5) AS device_limit,
    device_limit AS device_limit_set, rev, purged_rev, recovery_hash, handle_changed_at, pw_salt, pw_hash";
const DEVICE_COLS: &str = "id, user_id, name, platform, created_at, last_seen, access_hash, access_exp, refresh_hash, refresh_exp";
const ITEM_COLS: &str = "kind, key, value, updated_at, device_id, deleted, rev";

#[async_trait(?Send)]
impl AccountStore for D1Store {
    async fn user(&self, id: &str) -> StoreResult<Option<User>> {
        Ok(self.first::<UserRow>(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), &[s(id)]).await?.map(Into::into))
    }

    async fn user_by_handle(&self, handle: &str) -> StoreResult<Option<User>> {
        Ok(self.first::<UserRow>(&format!("SELECT {USER_COLS} FROM users WHERE handle = ?1"), &[s(handle)]).await?.map(Into::into))
    }

    async fn handle_free(&self, handle: &str, now: u64) -> StoreResult<bool> {
        #[derive(Deserialize)]
        struct C {
            c: u64,
        }
        let c = self
            .first::<C>(
                "SELECT (SELECT count(*) FROM users WHERE handle = ?1) + (SELECT count(*) FROM handle_holds WHERE handle = ?1 AND until > ?2) AS c",
                &[s(handle), n(now)],
            )
            .await?;
        Ok(c.map_or(0, |c| c.c) == 0)
    }

    async fn create_user(&self, u: &User, p: Option<&Passkey>, now: u64) -> StoreResult<bool> {
        let mut stmts = vec![
            self.stmt("DELETE FROM handle_holds WHERE handle = ?1 AND until <= ?2", &[s(&u.handle), n(now)])?,
            self.stmt(
                "INSERT OR IGNORE INTO users (id, handle, created_at, device_limit, rev, purged_rev, recovery_hash, handle_changed_at, pw_salt, pw_hash)
                 SELECT ?1, ?2, ?3, ?4, 0, 0, ?5, 0, ?6, ?7 WHERE NOT EXISTS (SELECT 1 FROM handle_holds WHERE handle = ?2)",
                &[s(&u.id), s(&u.handle), n(u.created_at), n(u.device_limit_set as u64), s(&u.recovery_hash), opt(&u.pw_salt), opt(&u.pw_hash)],
            )?,
        ];
        if let Some(p) = p {
            stmts.push(self.stmt(
                "INSERT INTO passkeys (cred_id, user_id, alg, public_key, sign_count, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6 WHERE EXISTS (SELECT 1 FROM users WHERE id = ?2 AND handle = ?7)",
                &[s(&p.cred_id), s(&p.user_id), JsValue::from_f64(p.alg as f64), s(&p.public_key), n(p.sign_count as u64), n(p.created_at), s(&u.handle)],
            )?);
        }
        let r = self.batch(stmts).await?;
        Ok(r.get(1).map(changes).unwrap_or(0) == 1)
    }

    async fn set_password(&self, user_id: &str, salt: &str, hash: &str) -> StoreResult<()> {
        self.run("UPDATE users SET pw_salt = ?1, pw_hash = ?2 WHERE id = ?3", &[s(salt), s(hash), s(user_id)]).await.map(|_| ())
    }

    async fn change_handle(&self, user_id: &str, new: &str, now: u64, hold_until: u64) -> StoreResult<bool> {
        let Some(old) = self.user(user_id).await? else { return Ok(false) };
        let r = self
            .batch(vec![
                self.stmt("DELETE FROM handle_holds WHERE handle = ?1 AND until <= ?2", &[s(new), n(now)])?,
                self.stmt(
                    "UPDATE OR IGNORE users SET handle = ?1, handle_changed_at = ?2
                     WHERE id = ?3 AND NOT EXISTS (SELECT 1 FROM handle_holds WHERE handle = ?1)",
                    &[s(new), n(now), s(user_id)],
                )?,
                self.stmt(
                    "INSERT OR REPLACE INTO handle_holds (handle, until)
                     SELECT ?1, ?2 WHERE EXISTS (SELECT 1 FROM users WHERE id = ?3 AND handle = ?4)",
                    &[s(&old.handle), n(hold_until), s(user_id), s(new)],
                )?,
            ])
            .await?;
        Ok(r.get(1).map(changes).unwrap_or(0) == 1)
    }

    async fn set_device_limit(&self, user_id: &str, limit: u8) -> StoreResult<()> {
        self.run("UPDATE users SET device_limit = ?1 WHERE id = ?2", &[n(limit as u64), s(user_id)]).await.map(|_| ())
    }

    async fn default_device_limit(&self) -> StoreResult<u8> {
        #[derive(Deserialize)]
        struct V {
            v: u8,
        }
        let row = self.first::<V>("SELECT CAST(value AS INTEGER) AS v FROM settings WHERE key = 'default_device_limit'", &[]).await?;
        Ok(row.map_or(glyph_server_core::accounts::store::DEFAULT_DEVICE_LIMIT, |r| r.v))
    }

    async fn set_default_device_limit(&self, limit: u8) -> StoreResult<()> {
        self.run(
            "INSERT INTO settings (key, value) VALUES ('default_device_limit', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            &[s(&limit.to_string())],
        )
        .await
        .map(|_| ())
    }

    async fn set_recovery(&self, user_id: &str, hash: &str) -> StoreResult<()> {
        self.run("UPDATE users SET recovery_hash = ?1 WHERE id = ?2", &[s(hash), s(user_id)]).await.map(|_| ())
    }

    async fn delete_user(&self, user_id: &str, hold_until: u64) -> StoreResult<()> {
        let id = s(user_id);
        self.batch(vec![
            self.stmt("INSERT OR REPLACE INTO handle_holds (handle, until) SELECT handle, ?2 FROM users WHERE id = ?1", &[id.clone(), n(hold_until)])?,
            self.stmt("DELETE FROM passkeys WHERE user_id = ?1", &[id.clone()])?,
            self.stmt("DELETE FROM devices WHERE user_id = ?1", &[id.clone()])?,
            self.stmt("DELETE FROM sync_items WHERE user_id = ?1", &[id.clone()])?,
            self.stmt("DELETE FROM logins WHERE user_id = ?1", &[id.clone()])?,
            self.stmt("DELETE FROM shares WHERE owner_id = ?1", &[id.clone()])?,
            self.stmt("DELETE FROM profiles WHERE user_id = ?1", &[id.clone()])?,
            self.stmt("DELETE FROM users WHERE id = ?1", &[id])?,
        ])
        .await
        .map(|_| ())
    }

    async fn passkey(&self, cred_id: &str) -> StoreResult<Option<Passkey>> {
        let r = self
            .first::<PasskeyRow>("SELECT cred_id, user_id, alg, public_key, sign_count, created_at FROM passkeys WHERE cred_id = ?1", &[s(cred_id)])
            .await?;
        Ok(r.map(|r| Passkey { cred_id: r.cred_id, user_id: r.user_id, alg: r.alg, public_key: r.public_key, sign_count: r.sign_count, created_at: r.created_at }))
    }

    async fn add_passkey(&self, p: &Passkey) -> StoreResult<()> {
        self.run(
            "INSERT INTO passkeys (cred_id, user_id, alg, public_key, sign_count, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            &[s(&p.cred_id), s(&p.user_id), JsValue::from_f64(p.alg as f64), s(&p.public_key), n(p.sign_count as u64), n(p.created_at)],
        )
        .await
        .map(|_| ())
    }

    async fn set_sign_count(&self, cred_id: &str, count: u32) -> StoreResult<()> {
        self.run("UPDATE passkeys SET sign_count = ?1 WHERE cred_id = ?2", &[n(count as u64), s(cred_id)]).await.map(|_| ())
    }

    async fn put_login(&self, l: &Login) -> StoreResult<()> {
        self.batch(vec![
            // Expired sign-ins are cleaned up on the way.
            self.stmt("DELETE FROM logins WHERE expires < ?1", &[n(l.expires.saturating_sub(glyph_server_core::accounts::LOGIN_TTL))])?,
            self.stmt(
                "INSERT OR REPLACE INTO logins (id, poll_hash, user_code, device_name, platform, challenge, pending_handle, pending_user_id, user_id, page_hash, expires, link)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                &[
                    s(&l.id),
                    s(&l.poll_hash),
                    s(&l.user_code),
                    s(&l.device_name),
                    s(&l.platform),
                    opt(&l.challenge),
                    opt(&l.pending_handle),
                    opt(&l.pending_user_id),
                    opt(&l.user_id),
                    opt(&l.page_hash),
                    n(l.expires),
                    n(l.link as u64),
                ],
            )?,
        ])
        .await
        .map(|_| ())
    }

    async fn login(&self, id: &str) -> StoreResult<Option<Login>> {
        self.login_where("id", id).await
    }

    async fn login_by_code(&self, user_code: &str) -> StoreResult<Option<Login>> {
        self.login_where("user_code", user_code).await
    }

    async fn delete_login(&self, id: &str) -> StoreResult<()> {
        self.run("DELETE FROM logins WHERE id = ?1", &[s(id)]).await.map(|_| ())
    }

    async fn devices(&self, user_id: &str) -> StoreResult<Vec<Device>> {
        Ok(self
            .all::<DeviceRow>(&format!("SELECT {DEVICE_COLS} FROM devices WHERE user_id = ?1 ORDER BY created_at"), &[s(user_id)])
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn device_by_access(&self, hash: &str) -> StoreResult<Option<Device>> {
        Ok(self.first::<DeviceRow>(&format!("SELECT {DEVICE_COLS} FROM devices WHERE access_hash = ?1"), &[s(hash)]).await?.map(Into::into))
    }

    async fn device_by_refresh(&self, hash: &str) -> StoreResult<Option<Device>> {
        Ok(self.first::<DeviceRow>(&format!("SELECT {DEVICE_COLS} FROM devices WHERE refresh_hash = ?1"), &[s(hash)]).await?.map(Into::into))
    }

    async fn put_device(&self, d: &Device) -> StoreResult<()> {
        self.run(
            &format!("INSERT OR REPLACE INTO devices ({DEVICE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
            &[
                s(&d.id),
                s(&d.user_id),
                s(&d.name),
                s(&d.platform),
                n(d.created_at),
                n(d.last_seen),
                s(&d.access_hash),
                n(d.access_exp),
                s(&d.refresh_hash),
                n(d.refresh_exp),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn add_device(&self, d: &Device, limit: u8) -> StoreResult<bool> {
        // One statement: the count and the insert cannot interleave with another sign-in.
        self.run(
            &format!(
                "INSERT INTO devices ({DEVICE_COLS}) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
                 WHERE (SELECT COUNT(*) FROM devices WHERE user_id = ?2) < ?11"
            ),
            &[
                s(&d.id),
                s(&d.user_id),
                s(&d.name),
                s(&d.platform),
                n(d.created_at),
                n(d.last_seen),
                s(&d.access_hash),
                n(d.access_exp),
                s(&d.refresh_hash),
                n(d.refresh_exp),
                n(limit as u64),
            ],
        )
        .await
        .map(|c| c == 1)
    }

    async fn touch_device(&self, id: &str, last_seen: u64) -> StoreResult<()> {
        self.run("UPDATE devices SET last_seen = ?1 WHERE id = ?2", &[n(last_seen), s(id)]).await.map(|_| ())
    }

    async fn delete_device(&self, user_id: &str, id: &str) -> StoreResult<bool> {
        Ok(self.run("DELETE FROM devices WHERE id = ?1 AND user_id = ?2", &[s(id), s(user_id)]).await? > 0)
    }

    async fn push_items(&self, user_id: &str, items: &[Item]) -> StoreResult<(Vec<bool>, u64)> {
        let count = items.len() as u64;
        let payload: Vec<serde_json::Value> = items
            .iter()
            .map(|i| {
                serde_json::json!({
                    "kind": i.kind, "key": i.key, "value": i.value, "updated_at": i.updated_at,
                    "device_id": i.device_id, "deleted": if i.deleted { 1 } else { 0 },
                })
            })
            .collect();
        let json = serde_json::Value::Array(payload).to_string();
        let uid = s(user_id);
        let r = self
            .batch(vec![
                self.stmt("UPDATE users SET rev = rev + ?2 WHERE id = ?1", &[uid.clone(), n(count)])?,
                // Revision of item i = (rev after the reservation) - count + i + 1.
                self.stmt(
                    "INSERT INTO sync_items (user_id, kind, key, value, updated_at, device_id, deleted, rev)
                     SELECT ?1, j.value ->> 'kind', j.value ->> 'key', j.value ->> 'value', j.value ->> 'updated_at',
                            j.value ->> 'device_id', j.value ->> 'deleted',
                            (SELECT rev FROM users WHERE id = ?1) - ?2 + j.key + 1
                     FROM json_each(?3) AS j WHERE true
                     ON CONFLICT (user_id, kind, key) DO UPDATE SET
                       value = excluded.value, updated_at = excluded.updated_at, device_id = excluded.device_id,
                       deleted = excluded.deleted, rev = excluded.rev
                     WHERE excluded.updated_at > sync_items.updated_at
                        OR (excluded.updated_at = sync_items.updated_at AND excluded.device_id > sync_items.device_id)
                     RETURNING rev",
                    &[uid.clone(), n(count), s(&json)],
                )?,
                self.stmt("SELECT rev FROM users WHERE id = ?1", &[uid])?,
            ])
            .await?;
        let rev = r.get(2).and_then(|x| x.results::<RevRow>().ok()).and_then(|v| v.into_iter().next()).map(|r| r.rev).ok_or_else(|| StoreError("no such user".into()))?;
        let base = rev - count;
        let mut accepted = vec![false; items.len()];
        for row in r.get(1).map(|x| x.results::<RevRow>()).transpose().map_err(err)?.unwrap_or_default() {
            if let Some(slot) = row.rev.checked_sub(base + 1).and_then(|i| accepted.get_mut(i as usize)) {
                *slot = true;
            }
        }
        Ok((accepted, rev))
    }

    async fn items(&self, user_id: &str, keys: &[(String, String)]) -> StoreResult<Vec<Item>> {
        let json = serde_json::to_string(keys).unwrap_or_else(|_| "[]".into());
        Ok(self
            .all::<ItemRow>(
                &format!(
                    "SELECT {ITEM_COLS} FROM sync_items WHERE user_id = ?1
                     AND (kind, key) IN (SELECT j.value ->> 0, j.value ->> 1 FROM json_each(?2) AS j)"
                ),
                &[s(user_id), s(&json)],
            )
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn items_since(&self, user_id: &str, since: u64, limit: u32) -> StoreResult<Vec<Item>> {
        Ok(self
            .all::<ItemRow>(
                &format!("SELECT {ITEM_COLS} FROM sync_items WHERE user_id = ?1 AND rev > ?2 ORDER BY rev LIMIT ?3"),
                &[s(user_id), n(since), n(limit as u64)],
            )
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn purge_tombstones(&self, user_id: &str, before: u64) -> StoreResult<()> {
        let (uid, b) = (s(user_id), n(before));
        self.batch(vec![
            self.stmt(
                "UPDATE users SET purged_rev = max(purged_rev, coalesce(
                   (SELECT max(rev) FROM sync_items WHERE user_id = ?1 AND deleted = 1 AND updated_at < ?2), 0))
                 WHERE id = ?1",
                &[uid.clone(), b.clone()],
            )?,
            self.stmt("DELETE FROM sync_items WHERE user_id = ?1 AND deleted = 1 AND updated_at < ?2", &[uid, b])?,
        ])
        .await
        .map(|_| ())
    }

    async fn put_share(&self, sh: &Share) -> StoreResult<()> {
        self.run(
            "INSERT OR REPLACE INTO shares (id, owner_id, code, name, songs, kind, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[s(&sh.id), s(&sh.owner_id), s(&sh.code), s(&sh.name), n(sh.songs as u64), s(&sh.kind), n(sh.created_at)],
        )
        .await
        .map(|_| ())
    }

    async fn profile(&self, user_id: &str) -> StoreResult<Option<Profile>> {
        #[derive(Deserialize)]
        struct Row {
            public: u8,
            show_recent: u8,
            playlists: String,
            avatar: Option<String>,
            banner: Option<String>,
            recent: Option<String>,
            updated_at: u64,
        }
        let r = self
            .first::<Row>("SELECT public, show_recent, playlists, avatar, banner, recent, updated_at FROM profiles WHERE user_id = ?1", &[s(user_id)])
            .await?;
        Ok(r.map(|r| Profile {
            public: r.public != 0,
            show_recent: r.show_recent != 0,
            playlists: serde_json::from_str(&r.playlists).unwrap_or_default(),
            avatar: r.avatar,
            banner: r.banner,
            recent: r.recent,
            updated_at: r.updated_at,
        }))
    }

    async fn put_profile(&self, user_id: &str, p: &Profile) -> StoreResult<()> {
        let playlists = serde_json::to_string(&p.playlists).unwrap_or_else(|_| "[]".into());
        self.run(
            "INSERT OR REPLACE INTO profiles (user_id, public, show_recent, playlists, avatar, banner, recent, updated_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 WHERE EXISTS (SELECT 1 FROM users WHERE id = ?1)",
            &[s(user_id), n(p.public as u64), n(p.show_recent as u64), s(&playlists), opt(&p.avatar), opt(&p.banner), opt(&p.recent), n(p.updated_at)],
        )
        .await
        .map(|_| ())
    }

    async fn share(&self, id: &str) -> StoreResult<Option<Share>> {
        #[derive(Deserialize)]
        struct Row {
            id: String,
            owner_id: String,
            code: String,
            name: String,
            songs: u32,
            kind: String,
            created_at: u64,
        }
        let r = self.first::<Row>("SELECT id, owner_id, code, name, songs, kind, created_at FROM shares WHERE id = ?1", &[s(id)]).await?;
        Ok(r.map(|r| Share { id: r.id, owner_id: r.owner_id, code: r.code, name: r.name, songs: r.songs, kind: r.kind, created_at: r.created_at }))
    }

    async fn delete_share(&self, user_id: &str, id: &str) -> StoreResult<bool> {
        Ok(self.run("DELETE FROM shares WHERE id = ?1 AND owner_id = ?2", &[s(id), s(user_id)]).await? > 0)
    }
}

impl D1Store {
    async fn login_where(&self, col: &str, v: &str) -> StoreResult<Option<Login>> {
        let r = self
            .first::<LoginRow>(
                &format!(
                    "SELECT id, poll_hash, user_code, device_name, platform, challenge, pending_handle, pending_user_id, user_id, page_hash, expires, link
                     FROM logins WHERE {col} = ?1"
                ),
                &[s(v)],
            )
            .await?;
        Ok(r.map(|r| Login {
            id: r.id,
            poll_hash: r.poll_hash,
            user_code: r.user_code,
            device_name: r.device_name,
            platform: r.platform,
            challenge: r.challenge,
            pending_handle: r.pending_handle,
            pending_user_id: r.pending_user_id,
            user_id: r.user_id,
            page_hash: r.page_hash,
            expires: r.expires,
            link: r.link != 0,
        }))
    }
}

// ------------------------------------------------------------------ R2

pub struct R2Blobs {
    pub bucket: worker::Bucket,
}

#[async_trait(?Send)]
impl BlobStore for R2Blobs {
    async fn get(&self, key: &str) -> StoreResult<Option<(Vec<u8>, String)>> {
        let Some(obj) = self.bucket.get(key).execute().await.map_err(err)? else { return Ok(None) };
        let ct = obj.http_metadata().content_type.unwrap_or_else(|| "application/octet-stream".into());
        let Some(body) = obj.body() else { return Ok(None) };
        Ok(Some((body.bytes().await.map_err(err)?, ct)))
    }

    async fn put(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> StoreResult<()> {
        let meta = worker::HttpMetadata { content_type: Some(content_type.into()), ..Default::default() };
        self.bucket.put(key, bytes).http_metadata(meta).execute().await.map_err(err)?;
        Ok(())
    }

    async fn delete_prefix(&self, prefix: &str) -> StoreResult<()> {
        loop {
            let list = self.bucket.list().prefix(prefix).limit(1000).execute().await.map_err(err)?;
            let keys: Vec<String> = list.objects().iter().map(|o| o.key()).collect();
            if keys.is_empty() {
                return Ok(());
            }
            self.bucket.delete_multiple(keys).await.map_err(err)?;
            if !list.truncated() {
                return Ok(());
            }
        }
    }
}

// ------------------------------------------------------------------ entropy

/// `crypto.getRandomValues` through getrandom's JS backend.
pub struct WebCrypto;

impl Entropy for WebCrypto {
    fn fill(&self, buf: &mut [u8]) {
        getrandom::getrandom(buf).expect("crypto.getRandomValues unavailable");
    }
}
