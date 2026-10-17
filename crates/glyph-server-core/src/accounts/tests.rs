use super::store::{MemBlobs, MemStore};
use super::webauthn::testkit::Authenticator;
use super::*;
use crate::server::Server;
use crate::testutil::*;
use crate::traits::Runtime;
use futures::executor::block_on;
use glyph_protocol::HealthResponse;
use std::sync::atomic::{AtomicU64, Ordering};

const ORIGIN: &str = "https://glyph.example";
const RP: &str = "glyph.example";

/// Deterministic "randomness" for tests.
#[derive(Default)]
struct Counter(AtomicU64);

impl Entropy for Counter {
    fn fill(&self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            *b = (n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8;
        }
    }
}

struct T {
    h: Harness,
    s: Server,
    blobs: Arc<MemBlobs>,
}

fn setup() -> T {
    let h = harness(Default::default());
    h.rt.advance(1_760_000_000_000);
    let blobs = Arc::new(MemBlobs::default());
    let s = Server::from_arc(h.svc.clone()).with_accounts(Accounts {
        store: Arc::new(MemStore::default()),
        blobs: Some(blobs.clone()),
        entropy: Arc::new(Counter::default()),
        origin: ORIGIN.into(),
        extra_origins: vec![],
        rp_id: None,
    });
    T { h, s, blobs }
}

fn call(t: &T, m: Method, path: &str, body: serde_json::Value, token: Option<&str>) -> (u16, serde_json::Value) {
    let mut headers = vec![];
    if let Some(tok) = token {
        headers.push(("authorization".to_string(), format!("Bearer {tok}")));
    }
    let body = if body.is_null() { vec![] } else { body.to_string().into_bytes() };
    let r = block_on(t.s.handle(Request::new(m, path, headers, body, Some("1.2.3.4".into()))));
    let v = serde_json::from_slice(r.bytes().unwrap_or_default()).unwrap_or(serde_json::Value::Null);
    (r.status, v)
}

fn start(t: &T, name: &str) -> LoginStartResponse {
    let (s, v) = call(t, Method::Post, "/v1/account/login/start", serde_json::json!({"device_name": name, "platform": "windows"}), None);
    assert_eq!(s, 200, "{v}");
    serde_json::from_value(v).unwrap()
}

fn poll(t: &T, l: &LoginStartResponse) -> LoginPollResponse {
    let (s, v) = call(t, Method::Post, "/v1/account/login/poll", serde_json::json!({"login_id": l.login_id, "poll_secret": l.poll_secret}), None);
    assert_eq!(s, 200, "{v}");
    serde_json::from_value(v).unwrap()
}

/// Full sign-up through the passkey page; returns the session and the authenticator.
fn sign_up(t: &T, handle: &str) -> (Session, Authenticator, String) {
    let l = start(t, "Desk");
    let (s, opts) = call(t, Method::Post, "/v1/account/passkey/register/options", serde_json::json!({"login_id": l.login_id, "handle": handle}), None);
    assert_eq!(s, 200, "{opts}");
    assert_eq!(opts["publicKey"]["rp"]["id"], RP);
    let ch = opts["publicKey"]["challenge"].as_str().unwrap().to_string();
    let a = Authenticator::new(handle.bytes().fold(7u8, |x, y| x.wrapping_mul(31).wrapping_add(y)), RP, ORIGIN);
    let (cd, att) = a.create(&ch);
    let (s, v) = call(t, Method::Post, "/v1/account/passkey/register/verify", serde_json::json!({"login_id": l.login_id, "client_data_json": b64(&cd), "attestation_object": b64(&att)}), None);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["user_code"], l.user_code);
    let rc = v["recovery_code"].as_str().unwrap().to_string();
    assert!(rc.starts_with("glyph-rc-"));
    let LoginPollResponse::Done { session } = poll(t, &l) else { panic!("not done") };
    assert_eq!(session.account.handle, handle.trim_start_matches('@').to_lowercase());
    (session, a, rc)
}

#[test]
fn health_reports_accounts_only_when_enabled() {
    let t = setup();
    let (_, v) = call(&t, Method::Get, "/v1/health", serde_json::Value::Null, None);
    let h: HealthResponse = serde_json::from_value(v).unwrap();
    assert!(h.accounts && h.covers);
    let plain = Server::from_arc(t.h.svc.clone());
    let r = block_on(plain.handle(Request::new(Method::Get, "/v1/health", vec![], vec![], None)));
    let h: HealthResponse = serde_json::from_slice(r.bytes().unwrap()).unwrap();
    assert!(!h.accounts);
    let r = block_on(plain.handle(Request::new(Method::Post, "/v1/account/login/start", vec![], b"{}".to_vec(), None)));
    assert_eq!(r.status, 404);
}

#[test]
fn signup_signin_and_handles() {
    let t = setup();
    let (s1, mut a, _) = sign_up(&t, "@Kasto");
    assert_eq!(s1.account.device_limit, 5);

    // Taken, reserved and invalid handles.
    let (_, v) = call(&t, Method::Get, "/v1/account/handle/kasto", serde_json::Value::Null, None);
    assert_eq!(v["available"], false);
    assert_eq!(v["reason"], "taken");
    let (_, v) = call(&t, Method::Get, "/v1/account/handle/admin", serde_json::Value::Null, None);
    assert_eq!(v["reason"], "reserved");
    let (_, v) = call(&t, Method::Get, "/v1/account/handle/a-b", serde_json::Value::Null, None);
    assert_eq!(v["reason"], "invalid");
    let l = start(&t, "x");
    let (s, v) = call(&t, Method::Post, "/v1/account/passkey/register/options", serde_json::json!({"login_id": l.login_id, "handle": "kasto"}), None);
    assert_eq!((s, v["code"].as_str()), (409, Some("handle_taken")));

    // Sign in on a second device with the same passkey.
    let l = start(&t, "Laptop");
    assert_eq!(poll(&t, &l), LoginPollResponse::Pending);
    let (_, opts) = call(&t, Method::Post, "/v1/account/passkey/login/options", serde_json::json!({"login_id": l.login_id}), None);
    let ch = opts["publicKey"]["challenge"].as_str().unwrap().to_string();
    let (cd, ad, sig) = a.get(&ch);
    let body = serde_json::json!({"login_id": l.login_id, "id": b64(&a.cred_id), "client_data_json": b64(&cd), "authenticator_data": b64(&ad), "signature": b64(&sig), "user_handle": b64(s1.account.id.as_bytes())});
    let (s, v) = call(&t, Method::Post, "/v1/account/passkey/login/verify", body.clone(), None);
    assert_eq!(s, 200, "{v}");
    assert!(v.get("recovery_code").is_none());
    // The challenge is single use.
    let (s, _) = call(&t, Method::Post, "/v1/account/passkey/login/verify", body, None);
    assert_eq!(s, 400);
    let LoginPollResponse::Done { session: s2 } = poll(&t, &l) else { panic!() };
    assert_eq!(s2.account.id, s1.account.id);
    // A finished login is gone.
    assert_eq!(poll(&t, &l), LoginPollResponse::Expired);

    let (_, v) = call(&t, Method::Get, "/v1/devices", serde_json::Value::Null, Some(&s2.access_token));
    let d: DevicesResponse = serde_json::from_value(v).unwrap();
    assert_eq!(d.devices.len(), 2);
    assert!(d.devices.iter().any(|d| d.current && d.name == "Laptop"));
}

#[test]
fn poll_needs_secret_and_logins_expire() {
    let t = setup();
    let l = start(&t, "x");
    let (s, _) = call(&t, Method::Post, "/v1/account/login/poll", serde_json::json!({"login_id": l.login_id, "poll_secret": "nope"}), None);
    assert_eq!(s, 401);
    t.h.rt.advance(LOGIN_TTL + 1);
    assert_eq!(poll(&t, &l), LoginPollResponse::Expired);
    let (s, v) = call(&t, Method::Post, "/v1/account/passkey/login/options", serde_json::json!({"login_id": l.login_id}), None);
    assert_eq!((s, v["code"].as_str()), (404, Some("login_expired")));
}

#[test]
fn pairing_code_and_device_limit() {
    let t = setup();
    let (s1, _, _) = sign_up(&t, "pair");
    // Fill up to the limit of 5 via pairing codes.
    for i in 0..4 {
        let l = start(&t, &format!("Dev {i}"));
        let (s, v) = call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code": l.user_code.to_lowercase().replace('-', " ")}), Some(&s1.access_token));
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["device_limit_reached"], false);
        assert!(matches!(poll(&t, &l), LoginPollResponse::Done { .. }));
    }
    let l = start(&t, "Sixth");
    let (_, v) = call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code": l.user_code}), Some(&s1.access_token));
    assert_eq!(v["device_limit_reached"], true);
    assert_eq!(poll(&t, &l), LoginPollResponse::DeviceLimit);
    // The client cannot raise its own limit.
    let (s, v) = call(&t, Method::Patch, "/v1/account", serde_json::json!({"device_limit": 10}), Some(&s1.access_token));
    assert_eq!((s, v["code"].as_str()), (403, Some("server_setting")));
    // Remove one device, then the waiting login completes.
    let (_, v) = call(&t, Method::Get, "/v1/devices", serde_json::Value::Null, Some(&s1.access_token));
    let d: DevicesResponse = serde_json::from_value(v).unwrap();
    let victim = d.devices.iter().find(|d| !d.current).unwrap().id.clone();
    let (s, _) = call(&t, Method::Delete, &format!("/v1/devices/{victim}"), serde_json::Value::Null, Some(&s1.access_token));
    assert_eq!(s, 200);
    let LoginPollResponse::Done { session: s6 } = poll(&t, &l) else { panic!() };
    // A removed device's token stops working at once.
    let (s, _) = call(&t, Method::Delete, &format!("/v1/devices/{}", s6.device_id), serde_json::Value::Null, Some(&s1.access_token));
    assert_eq!(s, 200);
    let (s, _) = call(&t, Method::Get, "/v1/account", serde_json::Value::Null, Some(&s6.access_token));
    assert_eq!(s, 401);
    // Unknown / used codes.
    let (s, _) = call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code": l.user_code}), Some(&s1.access_token));
    assert_eq!(s, 404);
}

#[test]
fn page_can_remove_devices_when_full() {
    let t = setup();
    let (s1, mut a, _) = sign_up(&t, "fullpage");
    for i in 0..4 {
        let l = start(&t, &format!("D{i}"));
        call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code": l.user_code}), Some(&s1.access_token));
        poll(&t, &l);
    }
    let l = start(&t, "New");
    let (_, opts) = call(&t, Method::Post, "/v1/account/passkey/login/options", serde_json::json!({"login_id": l.login_id}), None);
    let (cd, ad, sig) = a.get(opts["publicKey"]["challenge"].as_str().unwrap());
    let (_, v) = call(&t, Method::Post, "/v1/account/passkey/login/verify", serde_json::json!({"login_id": l.login_id, "id": b64(&a.cred_id), "client_data_json": b64(&cd), "authenticator_data": b64(&ad), "signature": b64(&sig)}), None);
    assert_eq!(v["devices"].as_array().unwrap().len(), 5);
    let secret = v["page_secret"].as_str().unwrap().to_string();
    let victim = v["devices"][1]["id"].as_str().unwrap().to_string();
    let (s, _) = call(&t, Method::Post, "/v1/account/login/remove_device", serde_json::json!({"login_id": l.login_id, "page_secret": "wrong", "device_id": victim}), None);
    assert_eq!(s, 401);
    let (s, v) = call(&t, Method::Post, "/v1/account/login/remove_device", serde_json::json!({"login_id": l.login_id, "page_secret": secret, "device_id": victim}), None);
    assert_eq!(s, 200);
    assert_eq!(v["devices"].as_array().unwrap().len(), 4);
    assert!(matches!(poll(&t, &l), LoginPollResponse::Done { .. }));
}

#[test]
fn refresh_rotates_and_access_expires() {
    let t = setup();
    let (s1, _, _) = sign_up(&t, "fresh");
    t.h.rt.advance(ACCESS_TTL + 1);
    let (s, v) = call(&t, Method::Get, "/v1/account", serde_json::Value::Null, Some(&s1.access_token));
    assert_eq!((s, v["code"].as_str()), (401, Some("token_expired")));
    let (s, v) = call(&t, Method::Post, "/v1/account/token/refresh", serde_json::json!({"refresh_token": s1.refresh_token}), None);
    assert_eq!(s, 200);
    let s2: Session = serde_json::from_value(v).unwrap();
    assert_eq!(s2.device_id, s1.device_id);
    // The old refresh token is used up.
    let (s, _) = call(&t, Method::Post, "/v1/account/token/refresh", serde_json::json!({"refresh_token": s1.refresh_token}), None);
    assert_eq!(s, 401);
    let (s, _) = call(&t, Method::Get, "/v1/account", serde_json::Value::Null, Some(&s2.access_token));
    assert_eq!(s, 200);
}

#[test]
fn recovery_code_signs_in_once() {
    let t = setup();
    let (_, _, rc) = sign_up(&t, "lost");
    let l = start(&t, "Phone");
    let (s, _) = call(&t, Method::Post, "/v1/account/recover", serde_json::json!({"login_id": l.login_id, "handle": "lost", "recovery_code": "glyph-rc-wrong"}), None);
    assert_eq!(s, 401);
    let (s, v) = call(&t, Method::Post, "/v1/account/recover", serde_json::json!({"login_id": l.login_id, "handle": "@LOST", "recovery_code": rc}), None);
    assert_eq!(s, 200, "{v}");
    let fresh = v["recovery_code"].as_str().unwrap();
    assert_ne!(fresh, rc);
    assert!(matches!(poll(&t, &l), LoginPollResponse::Done { .. }));
    let l = start(&t, "Again");
    let (s, _) = call(&t, Method::Post, "/v1/account/recover", serde_json::json!({"login_id": l.login_id, "handle": "lost", "recovery_code": rc}), None);
    assert_eq!(s, 401);
}

#[test]
fn rename_cooldown_hold_and_delete() {
    let t = setup();
    let (s1, _, _) = sign_up(&t, "oldname");
    let (s, v) = call(&t, Method::Patch, "/v1/account", serde_json::json!({"handle": "newname"}), Some(&s1.access_token));
    assert_eq!((s, v["handle"].as_str()), (200, Some("newname")));
    let (s, v) = call(&t, Method::Patch, "/v1/account", serde_json::json!({"handle": "third"}), Some(&s1.access_token));
    assert_eq!((s, v["code"].as_str()), (409, Some("handle_cooldown")));
    // The old handle is held for 30 days.
    let (_, v) = call(&t, Method::Get, "/v1/account/handle/oldname", serde_json::Value::Null, None);
    assert_eq!(v["available"], false);

    let (s, _) = call(&t, Method::Delete, "/v1/account", serde_json::json!({"confirm_handle": "oldname"}), Some(&s1.access_token));
    assert_eq!(s, 400);
    t.blobs.objects.lock().unwrap().insert(format!("users/{}/covers/x", s1.account.id), (vec![1], "image/png".into()));
    let (s, _) = call(&t, Method::Delete, "/v1/account", serde_json::json!({"confirm_handle": "@newname"}), Some(&s1.access_token));
    assert_eq!(s, 200);
    assert!(t.blobs.objects.lock().unwrap().is_empty());
    let (s, _) = call(&t, Method::Get, "/v1/account", serde_json::Value::Null, Some(&s1.access_token));
    assert_eq!(s, 401);
    let (_, v) = call(&t, Method::Get, "/v1/account/handle/newname", serde_json::Value::Null, None);
    assert_eq!(v["available"], false);
    t.h.rt.advance(HANDLE_HOLD + 1);
    let (_, v) = call(&t, Method::Get, "/v1/account/handle/newname", serde_json::Value::Null, None);
    assert_eq!(v["available"], true);
}

fn item(kind: &str, key: &str, value: Option<serde_json::Value>, at: u64) -> serde_json::Value {
    let mut v = serde_json::json!({"kind": kind, "key": key, "updated_at": at});
    match value {
        Some(x) => v["value"] = x,
        None => v["deleted"] = true.into(),
    }
    v
}

#[test]
fn sync_last_writer_wins_with_tombstones() {
    let t = setup();
    let (a, _, _) = sign_up(&t, "syncer");
    let l = start(&t, "B");
    call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code": l.user_code}), Some(&a.access_token));
    let LoginPollResponse::Done { session: b } = poll(&t, &l) else { panic!() };
    let now = t.h.rt.now_ms();

    let push = |s: &Session, items: Vec<serde_json::Value>| call(&t, Method::Post, "/v1/sync/push", serde_json::json!({"items": items}), Some(&s.access_token));
    let pull = |s: &Session, since: u64| -> SyncPullResponse {
        let (st, v) = call(&t, Method::Get, &format!("/v1/sync/pull?since={since}&limit=2"), serde_json::Value::Null, Some(&s.access_token));
        assert_eq!(st, 200, "{v}");
        serde_json::from_value(v).unwrap()
    };

    let (s, v) = push(&a, vec![
        item("liked", "audius:1", Some(serde_json::json!({"title": "One"})), now),
        item("liked", "audius:2", Some(serde_json::json!({"title": "Two"})), now),
        item("followed", "daft punk", Some(serde_json::json!({"name": "Daft Punk"})), now),
    ]);
    assert_eq!((s, v["accepted"].as_u64(), v["rev"].as_u64()), (200, Some(3), Some(3)));

    // B pulls in pages of 2.
    let p1 = pull(&b, 0);
    assert_eq!((p1.items.len(), p1.more, p1.rev), (2, true, 2));
    let p2 = pull(&b, p1.rev);
    assert_eq!((p2.items.len(), p2.more, p2.rev), (1, false, 3));

    // B unlikes song 1 later; A edits it with an older clock -> A loses.
    let (_, v) = push(&b, vec![item("liked", "audius:1", None, now + 10)]);
    assert_eq!(v["accepted"], 1);
    let (_, v) = push(&a, vec![item("liked", "audius:1", Some(serde_json::json!({"title": "One!"})), now + 5)]);
    assert_eq!(v["accepted"], 0);
    assert_eq!(v["stale"][0]["deleted"], true);
    let p = pull(&a, 3);
    assert_eq!(p.items.len(), 1);
    assert!(p.items[0].deleted && p.items[0].value.is_none());

    // Clocks far in the future are clamped, so they cannot pin an item forever.
    let (_, _) = push(&a, vec![item("liked", "audius:9", Some(serde_json::json!({})), now + 365 * DAY)]);
    let (_, v) = push(&b, vec![item("liked", "audius:9", None, now + 10 * MIN)]);
    assert_eq!(v["accepted"], 1);

    // Validation.
    let (s, _) = push(&a, vec![item("Bad Kind", "k", Some(serde_json::json!(1)), now)]);
    assert_eq!(s, 400);
    let (s, _) = push(&a, vec![serde_json::json!({"kind": "liked", "key": "k", "updated_at": now})]);
    assert_eq!(s, 400);

    // Old tombstones get purged; devices that slept through it must reset.
    let renew = |s: &Session| -> Session {
        let (_, v) = call(&t, Method::Post, "/v1/account/token/refresh", serde_json::json!({"refresh_token": s.refresh_token}), None);
        serde_json::from_value(v).unwrap()
    };
    let (mut a, mut b) = (a, b);
    // Devices that stay in use keep their tokens alive across the horizon.
    for _ in 0..3 {
        t.h.rt.advance(61 * DAY);
        (a, b) = (renew(&a), renew(&b));
    }
    let (_, v) = push(&a, vec![item("hidden", "x", Some(serde_json::json!({"name": "X"})), t.h.rt.now_ms())]);
    assert_eq!(v["accepted"], 1);
    let p = pull(&b, 1);
    assert!(p.reset);
    // Export has only live items.
    let (_, v) = call(&t, Method::Get, "/v1/sync/export", serde_json::Value::Null, Some(&a.access_token));
    let kinds: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["key"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["audius:2", "daft punk", "x"]);
}

#[test]
fn sync_needs_auth_and_users_are_isolated() {
    let t = setup();
    let (a, _, _) = sign_up(&t, "alice");
    let (b, _, _) = sign_up(&t, "bobby");
    let (s, _) = call(&t, Method::Get, "/v1/sync/pull", serde_json::Value::Null, None);
    assert_eq!(s, 401);
    let (s, _) = call(&t, Method::Get, "/v1/sync/pull", serde_json::Value::Null, Some("ga_forged"));
    assert_eq!(s, 401);
    call(&t, Method::Post, "/v1/sync/push", serde_json::json!({"items": [item("liked", "a", Some(serde_json::json!(1)), 1)]}), Some(&a.access_token));
    let (_, v) = call(&t, Method::Get, "/v1/sync/pull", serde_json::Value::Null, Some(&b.access_token));
    assert!(v["items"].as_array().unwrap().is_empty());
    // Bob cannot remove Alice's device.
    let (s, _) = call(&t, Method::Delete, &format!("/v1/devices/{}", a.device_id), serde_json::Value::Null, Some(&b.access_token));
    assert_eq!(s, 404);
}

#[test]
fn covers_cache_and_own_uploads() {
    let t = setup();
    let gif = "GIF89a-fake-image-bytes";
    t.h.http.on("i.ytimg.com", 200, gif);
    let url = "https://i.ytimg.com/vi/abc/hq.jpg";
    let name = hash(url);
    let path = format!("/v1/covers/{name}?u={}", url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>());
    for _ in 0..2 {
        let r = block_on(t.s.handle(Request::new(Method::Get, &path, vec![], vec![], None)));
        assert_eq!(r.status, 200);
        assert_eq!(r.bytes().unwrap(), gif.as_bytes());
        assert!(r.headers.iter().any(|(k, v)| k == "content-type" && v == "image/gif"));
    }
    assert_eq!(t.h.http.call_count("i.ytimg.com"), 1);
    // Hash must match, host must be allowed, body must be an image.
    let (s, _) = call(&t, Method::Get, &format!("/v1/covers/{}?u=https://evil.example/x.jpg", hash("https://evil.example/x.jpg")), serde_json::Value::Null, None);
    assert_eq!(s, 400);
    let (s, _) = call(&t, Method::Get, &format!("/v1/covers/{name}?u=https://i.ytimg.com/other.jpg"), serde_json::Value::Null, None);
    assert_eq!(s, 400);
    t.h.http.on("archive.org", 200, "<html>not an image</html>");
    let u2 = "https://archive.org/services/img/x";
    let (s, _) = call(&t, Method::Get, &format!("/v1/covers/{}?u={u2}", hash(u2)), serde_json::Value::Null, None);
    assert_eq!(s, 502);

    let (s1, _, _) = sign_up(&t, "covers");
    let img = b"\x89PNG\r\n\x1a\nrest".to_vec();
    let name = hex(&Sha256::digest(&img));
    let put = |body: Vec<u8>, n: &str| {
        let r = block_on(t.s.handle(Request::new(Method::Put, &format!("/v1/sync/covers/{n}"), vec![("authorization".into(), format!("Bearer {}", s1.access_token))], body, None)));
        r.status
    };
    assert_eq!(put(img.clone(), &hash("x")), 400);
    assert_eq!(put(b"plain text".to_vec(), &hex(&Sha256::digest(b"plain text"))), 400);
    assert_eq!(put(img.clone(), &name), 200);
    let r = block_on(t.s.handle(Request::new(Method::Get, &format!("/v1/sync/covers/{name}"), vec![("authorization".into(), format!("Bearer {}", s1.access_token))], vec![], None)));
    assert_eq!((r.status, r.bytes().unwrap()), (200, img.as_slice()));
    let r = block_on(t.s.handle(Request::new(Method::Get, &format!("/v1/sync/covers/{name}"), vec![], vec![], None)));
    assert_eq!(r.status, 401);
}

#[test]
fn login_page_is_served_with_strict_headers() {
    let t = setup();
    let r = block_on(t.s.handle(Request::new(Method::Get, "/account/login/abc", vec![], vec![], None)));
    assert_eq!(r.status, 200);
    assert!(std::str::from_utf8(r.bytes().unwrap()).unwrap().contains("navigator.credentials"));
    assert!(r.headers.iter().any(|(k, v)| k == "content-security-policy" && v.contains("frame-ancestors 'none'")));
}

#[test]
fn helpers() {
    assert_eq!(normalize_handle(" @Ab_1 "), Ok("ab_1".into()));
    assert_eq!(normalize_handle("ab"), Err("invalid"));
    assert_eq!(normalize_handle("___"), Err("invalid"));
    assert_eq!(normalize_handle("ünï"), Err("invalid"));
    assert_eq!(normalize_user_code("k7px 2qmd"), Some("K7PX-2QMD".into()));
    assert_eq!(normalize_user_code("K7PX-2QM0"), None);
    assert!(owns("/v1/account") && owns("/v1/sync/pull") && owns("/account/login/x") && owns("/v1/covers/ab"));
    assert!(!owns("/v1/accounts") && !owns("/v1/search") && !owns("/v1/cover"));
    assert_eq!(crate::server::max_body_bytes("/v1/sync/push"), MAX_PUSH_BYTES);
    assert_eq!(crate::server::max_body_bytes("/v1/resolve"), crate::server::MAX_BODY_BYTES);
}

fn pw_register(t: &T, handle: &str, key: &[u8; 32]) -> (u16, serde_json::Value) {
    call(t, Method::Post, "/v1/account/password/register", serde_json::json!({"handle": handle, "salt": b64(&[9u8; 16]), "key": b64(key), "device_name": "PC", "platform": "windows"}), None)
}

fn pw_login(t: &T, handle: &str, key: &[u8; 32]) -> (u16, serde_json::Value) {
    call(t, Method::Post, "/v1/account/password/login", serde_json::json!({"handle": handle, "key": b64(key), "device_name": "Laptop", "platform": "linux"}), None)
}

#[test]
fn password_signup_and_signin() {
    let t = setup();
    let (s, v) = pw_register(&t, "@Pw_User", &[1; 32]);
    assert_eq!(s, 200, "{v}");
    let su: glyph_protocol::SignupResponse = serde_json::from_value(v).unwrap();
    assert_eq!(su.session.account.handle, "pw_user");
    assert!(su.session.account.has_password);
    assert!(su.recovery_code.starts_with("glyph-rc-"));
    // Same handle again.
    assert_eq!(pw_register(&t, "pw_user", &[2; 32]).0, 409);
    // Salt for the client's key derivation.
    let (s, v) = call(&t, Method::Get, "/v1/account/password/salt/PW_USER", serde_json::Value::Null, None);
    assert_eq!((s, v["salt"].as_str()), (200, Some(b64(&[9u8; 16]).as_str())));
    let (s, _) = call(&t, Method::Get, "/v1/account/password/salt/nobody_here", serde_json::Value::Null, None);
    assert_eq!(s, 404);
    // Wrong and right key.
    let (s, v) = pw_login(&t, "pw_user", &[7; 32]);
    assert_eq!((s, v["code"].as_str()), (401, Some("login_failed")));
    let (s, v) = pw_login(&t, "pw_user", &[1; 32]);
    assert_eq!(s, 200, "{v}");
    let s2: Session = serde_json::from_value(v).unwrap();
    assert_ne!(s2.device_id, su.session.device_id);
    // Malformed keys are rejected before any lookup.
    let (s, _) = call(&t, Method::Post, "/v1/account/password/login", serde_json::json!({"handle": "pw_user", "key": "short", "device_name": "x", "platform": "x"}), None);
    assert_eq!(s, 400);
    // The server never stores the key itself.
    let (s, v) = call(&t, Method::Get, "/v1/sync/export", serde_json::Value::Null, Some(&s2.access_token));
    assert_eq!(s, 200);
    assert!(!v.to_string().contains(&b64(&[1; 32])));
}

#[test]
fn password_device_limit_and_change() {
    let t = setup();
    let (_, v) = pw_register(&t, "limited", &[3; 32]);
    let su: glyph_protocol::SignupResponse = serde_json::from_value(v).unwrap();
    for _ in 0..4 {
        assert_eq!(pw_login(&t, "limited", &[3; 32]).0, 200);
    }
    let (s, v) = pw_login(&t, "limited", &[3; 32]);
    assert_eq!((s, v["code"].as_str()), (409, Some("device_limit")));
    // Change the password while signed in: the old key stops working.
    let (s, v) = call(&t, Method::Put, "/v1/account/password", serde_json::json!({"salt": b64(&[5u8; 16]), "key": b64(&[4u8; 32])}), Some(&su.session.access_token));
    assert_eq!((s, v["has_password"].as_bool()), (200, Some(true)));
    let (s, _) = call(&t, Method::Delete, &format!("/v1/devices/{}", su.session.device_id), serde_json::Value::Null, Some(&su.session.access_token));
    assert_eq!(s, 200);
    assert_eq!(pw_login(&t, "limited", &[3; 32]).0, 401);
    assert_eq!(pw_login(&t, "limited", &[4; 32]).0, 200);
}

#[test]
fn passkey_users_add_a_password_and_password_users_add_a_passkey() {
    let t = setup();
    // Passkey account -> add password.
    let (s1, _, _) = sign_up(&t, "keyfirst");
    assert!(!s1.account.has_password);
    let (_, v) = call(&t, Method::Get, "/v1/account/password/salt/keyfirst", serde_json::Value::Null, None);
    assert_eq!(v["code"], "no_password");
    call(&t, Method::Put, "/v1/account/password", serde_json::json!({"salt": b64(&[1u8; 16]), "key": b64(&[8u8; 32])}), Some(&s1.access_token));
    assert_eq!(pw_login(&t, "keyfirst", &[8; 32]).0, 200);

    // Password account -> link a passkey through the page, then sign in with it.
    let (_, v) = pw_register(&t, "pwfirst", &[6; 32]);
    let su: glyph_protocol::SignupResponse = serde_json::from_value(v).unwrap();
    let (s, v) = call(&t, Method::Post, "/v1/account/passkey/link", serde_json::Value::Null, Some(&su.session.access_token));
    assert_eq!(s, 200, "{v}");
    let login_id = v["url"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let (_, info) = call(&t, Method::Post, "/v1/account/login/info", serde_json::json!({"login_id": login_id}), None);
    assert_eq!((info["link"].as_bool(), info["handle"].as_str()), (Some(true), Some("pwfirst")));
    let (_, opts) = call(&t, Method::Post, "/v1/account/passkey/register/options", serde_json::json!({"login_id": login_id}), None);
    assert_eq!(opts["publicKey"]["user"]["id"], b64(su.session.account.id.as_bytes()));
    let mut a = Authenticator::new(201, RP, ORIGIN);
    let (cd, att) = a.create(opts["publicKey"]["challenge"].as_str().unwrap());
    let (s, v) = call(&t, Method::Post, "/v1/account/passkey/register/verify", serde_json::json!({"login_id": login_id, "client_data_json": b64(&cd), "attestation_object": b64(&att)}), None);
    assert_eq!((s, v["linked"].as_bool()), (200, Some(true)), "{v}");
    // Linking created no device.
    let (_, d) = call(&t, Method::Get, "/v1/devices", serde_json::Value::Null, Some(&su.session.access_token));
    assert_eq!(d["devices"].as_array().unwrap().len(), 1);
    let l = start(&t, "Phone");
    let (_, opts) = call(&t, Method::Post, "/v1/account/passkey/login/options", serde_json::json!({"login_id": l.login_id}), None);
    let (cd, ad, sig) = a.get(opts["publicKey"]["challenge"].as_str().unwrap());
    let (s, v) = call(&t, Method::Post, "/v1/account/passkey/login/verify", serde_json::json!({"login_id": l.login_id, "id": b64(&a.cred_id), "client_data_json": b64(&cd), "authenticator_data": b64(&ad), "signature": b64(&sig)}), None);
    assert_eq!((s, v["handle"].as_str()), (200, Some("pwfirst")));
    let LoginPollResponse::Done { session } = poll(&t, &l) else { panic!() };
    assert_eq!(session.account.id, su.session.account.id);
}

#[test]
fn login_page_password_flow_completes_the_app_login() {
    let t = setup();
    // Sign up on the page for a login the app started.
    let l = start(&t, "Desk");
    let (s, v) = call(&t, Method::Post, "/v1/account/login/password_register", serde_json::json!({"login_id": l.login_id, "handle": "webpw", "salt": b64(&[2u8; 16]), "key": b64(&[3u8; 32])}), None);
    assert_eq!(s, 200, "{v}");
    assert!(v["recovery_code"].as_str().unwrap().starts_with("glyph-rc-"));
    let LoginPollResponse::Done { session } = poll(&t, &l) else { panic!() };
    assert!(session.account.has_password);
    // Sign in on the page for a second device; wrong key first.
    let l = start(&t, "Laptop");
    let (s, _) = call(&t, Method::Post, "/v1/account/login/password", serde_json::json!({"login_id": l.login_id, "handle": "webpw", "key": b64(&[9u8; 32])}), None);
    assert_eq!(s, 401);
    let (s, v) = call(&t, Method::Post, "/v1/account/login/password", serde_json::json!({"login_id": l.login_id, "handle": "@WebPw", "key": b64(&[3u8; 32])}), None);
    assert_eq!((s, v["handle"].as_str()), (200, Some("webpw")));
    assert!(matches!(poll(&t, &l), LoginPollResponse::Done { .. }));
}

#[test]
fn passkey_domain_must_contain_the_origin() {
    let make = |origin: &str, rp: Option<&str>| Accounts {
        store: Arc::new(MemStore::default()),
        blobs: None,
        entropy: Arc::new(Counter::default()),
        origin: origin.into(),
        extra_origins: vec![],
        rp_id: rp.map(Into::into),
    };
    assert_eq!(make("https://api.glyph.my", Some("glyph.my")).rp_id(), "glyph.my");
    assert_eq!(make("https://api.glyph.my", None).rp_id(), "api.glyph.my");
    assert_eq!(make("https://api.glyph.my", Some("evil.com")).rp_id(), "api.glyph.my");
    assert_eq!(make("https://api.glyph.my", Some("ph.my")).rp_id(), "api.glyph.my");
}

#[test]
fn operator_sets_default_and_per_account_device_limits() {
    let t = setup();
    let acc = t.s.accounts.clone().unwrap();
    let (s1, _, _) = sign_up(&t, "limited");
    block_on(acc.admin_set_defaults(&crate::admin::types::AccountDefaults { device_limit: 2 })).unwrap();
    assert!(block_on(acc.admin_set_defaults(&crate::admin::types::AccountDefaults { device_limit: 0 })).is_err());
    let (_, v) = call(&t, Method::Get, "/v1/account", serde_json::Value::Null, Some(&s1.access_token));
    assert_eq!(v["device_limit"], 2);
    // Second device fits, third does not.
    let pair = |name: &str| {
        let l = start(&t, name);
        call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code": l.user_code}), Some(&s1.access_token));
        poll(&t, &l)
    };
    assert!(matches!(pair("two"), LoginPollResponse::Done { .. }));
    assert_eq!(pair("three"), LoginPollResponse::DeviceLimit);
    // Own limit beats the default; None goes back to it.
    let a = block_on(acc.admin_set_limit("@Limited", Some(3))).unwrap();
    assert_eq!((a.device_limit, a.device_limit_set, a.devices.len()), (3, Some(3), 2));
    assert!(matches!(pair("three"), LoginPollResponse::Done { .. }));
    let a = block_on(acc.admin_set_limit("limited", None)).unwrap();
    assert_eq!((a.device_limit, a.device_limit_set), (2, None));
    assert!(block_on(acc.admin_set_limit("nobody", Some(3))).is_err());
}

#[test]
fn share_links_need_an_account_and_die_with_it() {
    let t = setup();
    let code = "glyph:playlist:q1YqU7Iy1FHKU7JSCshJrMzJLC5RMFTSUSpRsoquBpFKESHOQH4i";
    let body = serde_json::json!({"code": code, "name": "Road <trip>", "songs": 2});
    let (s, _) = call(&t, Method::Post, "/v1/share", body.clone(), None);
    assert_eq!(s, 401);
    let (s1, _, _) = sign_up(&t, "gisela");
    let (s, v) = call(&t, Method::Post, "/v1/share", body.clone(), Some(&s1.access_token));
    assert_eq!(s, 200, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["url"], format!("{ORIGIN}/p/{id}"));
    // Sharing the same playlist again keeps the link.
    assert_eq!(call(&t, Method::Post, "/v1/share", body, Some(&s1.access_token)).1["id"], id.as_str());

    let (s, v) = call(&t, Method::Get, &format!("/v1/share/{id}"), serde_json::Value::Null, None);
    assert_eq!((s, v["code"].as_str(), v["author"].as_str()), (200, Some(code), Some("gisela")));
    let page = block_on(t.s.handle(Request::new(Method::Get, &format!("/p/{id}"), vec![], vec![], None)));
    assert_eq!(page.status, 200);
    assert!(String::from_utf8_lossy(page.bytes().unwrap()).contains("@gisela"));

    // Albums carry no author.
    let (_, v) = call(&t, Method::Post, "/v1/share", serde_json::json!({"code": "glyph:playlist:abc", "kind": "album"}), Some(&s1.access_token));
    let album = v["id"].as_str().unwrap().to_string();
    assert!(call(&t, Method::Get, &format!("/v1/share/{album}"), serde_json::Value::Null, None).1["author"].is_null());

    let (s, _) = call(&t, Method::Delete, "/v1/account", serde_json::json!({"confirm_handle": "gisela"}), Some(&s1.access_token));
    assert_eq!(s, 200);
    for gone in [&id, &album] {
        assert_eq!(call(&t, Method::Get, &format!("/v1/share/{gone}"), serde_json::Value::Null, None).0, 404);
    }
    let page = block_on(t.s.handle(Request::new(Method::Get, &format!("/p/{id}"), vec![], vec![], None)));
    assert_eq!(page.status, 404);
}
