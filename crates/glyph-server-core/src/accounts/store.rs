//! Storage seam for accounts. The Worker implements it with D1 (and R2 for
//! blobs); tests use [`MemStore`]. Rules that must hold atomically (handle
//! uniqueness, last-writer-wins, revision order) are part of the contract, so
//! every implementation enforces them itself.

use crate::traits::MaybeSendSync;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: String,
    pub handle: String,
    pub created_at: u64,
    /// Effective limit: `device_limit_set`, or the server default when that is 0.
    /// Stores fill it on read; writes ignore it.
    pub device_limit: u8,
    /// Limit an admin set for this account; 0 = server default.
    pub device_limit_set: u8,
    /// Newest item revision.
    pub rev: u64,
    /// Tombstones up to this revision were purged.
    pub purged_rev: u64,
    pub recovery_hash: String,
    pub handle_changed_at: u64,
    /// base64url salt the client uses for its Argon2id password key.
    pub pw_salt: Option<String>,
    /// SHA-256 (hex) of that key.
    pub pw_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Passkey {
    /// base64url credential id.
    pub cred_id: String,
    pub user_id: String,
    /// COSE algorithm (-7 ES256, -257 RS256).
    pub alg: i64,
    /// base64url COSE key as sent by the authenticator.
    pub public_key: String,
    pub sign_count: u32,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub platform: String,
    pub created_at: u64,
    pub last_seen: u64,
    pub access_hash: String,
    pub access_exp: u64,
    pub refresh_hash: String,
    pub refresh_exp: u64,
}

/// A sign-in in progress (10 minutes).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Login {
    pub id: String,
    pub poll_hash: String,
    pub user_code: String,
    pub device_name: String,
    pub platform: String,
    /// Current WebAuthn challenge (base64url) and what it is for.
    pub challenge: Option<String>,
    /// Handle and user id picked while registering.
    pub pending_handle: Option<String>,
    pub pending_user_id: Option<String>,
    /// Adds a passkey to `pending_user_id` instead of signing in.
    pub link: bool,
    /// Set once a passkey, recovery code or another device approved the login.
    pub user_id: Option<String>,
    /// Lets the login page manage devices after it signed in.
    pub page_hash: Option<String>,
    pub expires: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub kind: String,
    pub key: String,
    /// JSON text; `None` for tombstones.
    pub value: Option<String>,
    pub updated_at: u64,
    pub device_id: String,
    pub deleted: bool,
    pub rev: u64,
}

/// Profile settings of an account (see `accounts::profile`). Absent = default:
/// private, no songs, nothing shown.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Profile {
    pub public: bool,
    pub show_recent: bool,
    /// Ids of the playlists shown, in order.
    pub playlists: Vec<String>,
    /// sha256 of the picture / banner (blob `users/{id}/profile/{sha}`).
    pub avatar: Option<String>,
    pub banner: Option<String>,
    /// JSON array of the songs played last.
    pub recent: Option<String>,
    pub updated_at: u64,
}

/// Last writer wins: newer timestamp, ties broken by the larger device id.
pub fn wins(new: &Item, old: &Item) -> bool {
    (new.updated_at, new.device_id.as_str()) > (old.updated_at, old.device_id.as_str())
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait AccountStore: MaybeSendSync {
    async fn user(&self, id: &str) -> StoreResult<Option<User>>;
    async fn user_by_handle(&self, handle: &str) -> StoreResult<Option<User>>;
    /// Not used by an account and not held after a rename/deletion.
    async fn handle_free(&self, handle: &str, now: u64) -> StoreResult<bool>;
    /// Insert the user (with its first passkey, if any). `false`: handle taken or held.
    async fn create_user(&self, user: &User, passkey: Option<&Passkey>, now: u64) -> StoreResult<bool>;
    async fn set_password(&self, user_id: &str, salt: &str, hash: &str) -> StoreResult<()>;
    /// Rename; the old handle is held until `hold_until`. `false`: taken or held.
    async fn change_handle(&self, user_id: &str, new: &str, now: u64, hold_until: u64) -> StoreResult<bool>;
    /// Admin only: per-account limit, 0 = server default.
    async fn set_device_limit(&self, user_id: &str, limit: u8) -> StoreResult<()>;
    /// Server-wide default for accounts without their own limit.
    async fn default_device_limit(&self) -> StoreResult<u8>;
    async fn set_default_device_limit(&self, limit: u8) -> StoreResult<()>;
    async fn set_recovery(&self, user_id: &str, hash: &str) -> StoreResult<()>;
    /// Removes the user and everything it owns; holds the handle until `hold_until`.
    async fn delete_user(&self, user_id: &str, hold_until: u64) -> StoreResult<()>;

    async fn passkey(&self, cred_id: &str) -> StoreResult<Option<Passkey>>;
    async fn add_passkey(&self, passkey: &Passkey) -> StoreResult<()>;
    async fn set_sign_count(&self, cred_id: &str, count: u32) -> StoreResult<()>;

    async fn put_login(&self, login: &Login) -> StoreResult<()>;
    async fn login(&self, id: &str) -> StoreResult<Option<Login>>;
    async fn login_by_code(&self, user_code: &str) -> StoreResult<Option<Login>>;
    async fn delete_login(&self, id: &str) -> StoreResult<()>;

    async fn devices(&self, user_id: &str) -> StoreResult<Vec<Device>>;
    async fn device_by_access(&self, hash: &str) -> StoreResult<Option<Device>>;
    async fn device_by_refresh(&self, hash: &str) -> StoreResult<Option<Device>>;
    /// Insert or replace by id.
    async fn put_device(&self, device: &Device) -> StoreResult<()>;
    /// Insert a new device only while the user has fewer than `limit`
    /// (atomic, so parallel sign-ins cannot pass the limit). `false`: full.
    async fn add_device(&self, device: &Device, limit: u8) -> StoreResult<bool>;
    async fn touch_device(&self, id: &str, last_seen: u64) -> StoreResult<()>;
    /// `false` when no such device belongs to the user.
    async fn delete_device(&self, user_id: &str, id: &str) -> StoreResult<bool>;

    /// Apply items with [`wins`] against the stored copies, give every accepted
    /// item the next revision of the user (in order, atomically with respect to
    /// other pushes) and return which were accepted plus the user's newest rev.
    async fn push_items(&self, user_id: &str, items: &[Item]) -> StoreResult<(Vec<bool>, u64)>;
    async fn items(&self, user_id: &str, keys: &[(String, String)]) -> StoreResult<Vec<Item>>;
    /// Items with `rev > since`, ordered by rev.
    async fn items_since(&self, user_id: &str, since: u64, limit: u32) -> StoreResult<Vec<Item>>;
    /// Drop tombstones older than `before` (ms) and raise `purged_rev`.
    async fn purge_tombstones(&self, user_id: &str, before: u64) -> StoreResult<()>;

    async fn profile(&self, user_id: &str) -> StoreResult<Option<Profile>>;
    /// Insert or replace (`delete_user` removes it).
    async fn put_profile(&self, user_id: &str, profile: &Profile) -> StoreResult<()>;
}

/// Binary objects (cover images).
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait BlobStore: MaybeSendSync {
    async fn get(&self, key: &str) -> StoreResult<Option<(Vec<u8>, String)>>;
    async fn put(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> StoreResult<()>;
    async fn delete_prefix(&self, prefix: &str) -> StoreResult<()>;
}

// ------------------------------------------------------------------ memory

#[derive(Default)]
struct Mem {
    users: BTreeMap<String, User>,
    holds: BTreeMap<String, u64>,
    passkeys: BTreeMap<String, Passkey>,
    logins: BTreeMap<String, Login>,
    devices: BTreeMap<String, Device>,
    items: BTreeMap<(String, String, String), Item>,
    profiles: BTreeMap<String, Profile>,
    default_limit: Option<u8>,
}

/// Device limit of accounts nobody configured.
pub const DEFAULT_DEVICE_LIMIT: u8 = 5;

/// In-process store for tests (and a reference for other implementations).
#[derive(Default)]
pub struct MemStore {
    m: Mutex<Mem>,
}

impl Mem {
    fn free(&self, handle: &str, now: u64) -> bool {
        !self.users.values().any(|u| u.handle == handle) && self.holds.get(handle).is_none_or(|&t| t <= now)
    }
    fn read(&self, u: &User) -> User {
        let default = self.default_limit.unwrap_or(DEFAULT_DEVICE_LIMIT);
        User { device_limit: if u.device_limit_set > 0 { u.device_limit_set } else { default }, ..u.clone() }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl AccountStore for MemStore {
    async fn user(&self, id: &str) -> StoreResult<Option<User>> {
        let m = self.m.lock().unwrap();
        Ok(m.users.get(id).map(|u| m.read(u)))
    }
    async fn user_by_handle(&self, handle: &str) -> StoreResult<Option<User>> {
        let m = self.m.lock().unwrap();
        Ok(m.users.values().find(|u| u.handle == handle).map(|u| m.read(u)))
    }
    async fn handle_free(&self, handle: &str, now: u64) -> StoreResult<bool> {
        Ok(self.m.lock().unwrap().free(handle, now))
    }
    async fn create_user(&self, user: &User, passkey: Option<&Passkey>, now: u64) -> StoreResult<bool> {
        let mut m = self.m.lock().unwrap();
        if !m.free(&user.handle, now) {
            return Ok(false);
        }
        m.users.insert(user.id.clone(), user.clone());
        if let Some(p) = passkey {
            m.passkeys.insert(p.cred_id.clone(), p.clone());
        }
        Ok(true)
    }
    async fn set_password(&self, user_id: &str, salt: &str, hash: &str) -> StoreResult<()> {
        if let Some(u) = self.m.lock().unwrap().users.get_mut(user_id) {
            u.pw_salt = Some(salt.to_string());
            u.pw_hash = Some(hash.to_string());
        }
        Ok(())
    }
    async fn change_handle(&self, user_id: &str, new: &str, now: u64, hold_until: u64) -> StoreResult<bool> {
        let mut m = self.m.lock().unwrap();
        if !m.free(new, now) {
            return Ok(false);
        }
        let Some(u) = m.users.get_mut(user_id) else { return Ok(false) };
        let old = std::mem::replace(&mut u.handle, new.to_string());
        u.handle_changed_at = now;
        m.holds.insert(old, hold_until);
        Ok(true)
    }
    async fn set_device_limit(&self, user_id: &str, limit: u8) -> StoreResult<()> {
        if let Some(u) = self.m.lock().unwrap().users.get_mut(user_id) {
            u.device_limit_set = limit;
        }
        Ok(())
    }
    async fn default_device_limit(&self) -> StoreResult<u8> {
        Ok(self.m.lock().unwrap().default_limit.unwrap_or(DEFAULT_DEVICE_LIMIT))
    }
    async fn set_default_device_limit(&self, limit: u8) -> StoreResult<()> {
        self.m.lock().unwrap().default_limit = Some(limit);
        Ok(())
    }
    async fn set_recovery(&self, user_id: &str, hash: &str) -> StoreResult<()> {
        if let Some(u) = self.m.lock().unwrap().users.get_mut(user_id) {
            u.recovery_hash = hash.to_string();
        }
        Ok(())
    }
    async fn delete_user(&self, user_id: &str, hold_until: u64) -> StoreResult<()> {
        let mut m = self.m.lock().unwrap();
        if let Some(u) = m.users.remove(user_id) {
            m.holds.insert(u.handle, hold_until);
        }
        m.passkeys.retain(|_, p| p.user_id != user_id);
        m.devices.retain(|_, d| d.user_id != user_id);
        m.items.retain(|(u, _, _), _| u != user_id);
        m.logins.retain(|_, l| l.user_id.as_deref() != Some(user_id));
        m.profiles.remove(user_id);
        Ok(())
    }
    async fn passkey(&self, cred_id: &str) -> StoreResult<Option<Passkey>> {
        Ok(self.m.lock().unwrap().passkeys.get(cred_id).cloned())
    }
    async fn add_passkey(&self, passkey: &Passkey) -> StoreResult<()> {
        self.m.lock().unwrap().passkeys.insert(passkey.cred_id.clone(), passkey.clone());
        Ok(())
    }
    async fn set_sign_count(&self, cred_id: &str, count: u32) -> StoreResult<()> {
        if let Some(p) = self.m.lock().unwrap().passkeys.get_mut(cred_id) {
            p.sign_count = count;
        }
        Ok(())
    }
    async fn put_login(&self, login: &Login) -> StoreResult<()> {
        self.m.lock().unwrap().logins.insert(login.id.clone(), login.clone());
        Ok(())
    }
    async fn login(&self, id: &str) -> StoreResult<Option<Login>> {
        Ok(self.m.lock().unwrap().logins.get(id).cloned())
    }
    async fn login_by_code(&self, user_code: &str) -> StoreResult<Option<Login>> {
        Ok(self.m.lock().unwrap().logins.values().find(|l| l.user_code == user_code).cloned())
    }
    async fn delete_login(&self, id: &str) -> StoreResult<()> {
        self.m.lock().unwrap().logins.remove(id);
        Ok(())
    }
    async fn devices(&self, user_id: &str) -> StoreResult<Vec<Device>> {
        let m = self.m.lock().unwrap();
        let mut v: Vec<Device> = m.devices.values().filter(|d| d.user_id == user_id).cloned().collect();
        v.sort_by_key(|d| d.created_at);
        Ok(v)
    }
    async fn device_by_access(&self, hash: &str) -> StoreResult<Option<Device>> {
        Ok(self.m.lock().unwrap().devices.values().find(|d| d.access_hash == hash).cloned())
    }
    async fn device_by_refresh(&self, hash: &str) -> StoreResult<Option<Device>> {
        Ok(self.m.lock().unwrap().devices.values().find(|d| d.refresh_hash == hash).cloned())
    }
    async fn put_device(&self, device: &Device) -> StoreResult<()> {
        self.m.lock().unwrap().devices.insert(device.id.clone(), device.clone());
        Ok(())
    }
    async fn add_device(&self, device: &Device, limit: u8) -> StoreResult<bool> {
        let mut m = self.m.lock().unwrap();
        if m.devices.values().filter(|d| d.user_id == device.user_id).count() >= limit as usize {
            return Ok(false);
        }
        m.devices.insert(device.id.clone(), device.clone());
        Ok(true)
    }
    async fn touch_device(&self, id: &str, last_seen: u64) -> StoreResult<()> {
        if let Some(d) = self.m.lock().unwrap().devices.get_mut(id) {
            d.last_seen = last_seen;
        }
        Ok(())
    }
    async fn delete_device(&self, user_id: &str, id: &str) -> StoreResult<bool> {
        let mut m = self.m.lock().unwrap();
        if m.devices.get(id).is_some_and(|d| d.user_id == user_id) {
            m.devices.remove(id);
            return Ok(true);
        }
        Ok(false)
    }
    async fn push_items(&self, user_id: &str, items: &[Item]) -> StoreResult<(Vec<bool>, u64)> {
        let mut m = self.m.lock().unwrap();
        let Some(mut rev) = m.users.get(user_id).map(|u| u.rev) else {
            return Err(StoreError("no such user".into()));
        };
        let mut accepted = Vec::with_capacity(items.len());
        for it in items {
            let k = (user_id.to_string(), it.kind.clone(), it.key.clone());
            let ok = m.items.get(&k).is_none_or(|old| wins(it, old));
            if ok {
                rev += 1;
                m.items.insert(k, Item { rev, ..it.clone() });
            }
            accepted.push(ok);
        }
        m.users.get_mut(user_id).unwrap().rev = rev;
        Ok((accepted, rev))
    }
    async fn items(&self, user_id: &str, keys: &[(String, String)]) -> StoreResult<Vec<Item>> {
        let m = self.m.lock().unwrap();
        Ok(keys
            .iter()
            .filter_map(|(kind, key)| m.items.get(&(user_id.to_string(), kind.clone(), key.clone())).cloned())
            .collect())
    }
    async fn items_since(&self, user_id: &str, since: u64, limit: u32) -> StoreResult<Vec<Item>> {
        let m = self.m.lock().unwrap();
        let mut v: Vec<Item> = m.items.iter().filter(|((u, _, _), i)| u == user_id && i.rev > since).map(|(_, i)| i.clone()).collect();
        v.sort_by_key(|i| i.rev);
        v.truncate(limit as usize);
        Ok(v)
    }
    async fn purge_tombstones(&self, user_id: &str, before: u64) -> StoreResult<()> {
        let mut m = self.m.lock().unwrap();
        let mut max = 0;
        m.items.retain(|(u, _, _), i| {
            let drop = u == user_id && i.deleted && i.updated_at < before;
            if drop {
                max = max.max(i.rev);
            }
            !drop
        });
        if let Some(u) = m.users.get_mut(user_id) {
            u.purged_rev = u.purged_rev.max(max);
        }
        Ok(())
    }
    async fn profile(&self, user_id: &str) -> StoreResult<Option<Profile>> {
        Ok(self.m.lock().unwrap().profiles.get(user_id).cloned())
    }
    async fn put_profile(&self, user_id: &str, profile: &Profile) -> StoreResult<()> {
        let mut m = self.m.lock().unwrap();
        if m.users.contains_key(user_id) {
            m.profiles.insert(user_id.to_string(), profile.clone());
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct MemBlobs {
    pub objects: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl BlobStore for MemBlobs {
    async fn get(&self, key: &str) -> StoreResult<Option<(Vec<u8>, String)>> {
        Ok(self.objects.lock().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> StoreResult<()> {
        self.objects.lock().unwrap().insert(key.to_string(), (bytes, content_type.to_string()));
        Ok(())
    }
    async fn delete_prefix(&self, prefix: &str) -> StoreResult<()> {
        self.objects.lock().unwrap().retain(|k, _| !k.starts_with(prefix));
        Ok(())
    }
}
