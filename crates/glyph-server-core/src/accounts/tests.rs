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

#[test]
fn account_playback_requires_target_ack_and_retains_unacknowledged_delivery() {
    let relay = Arc::new(RecPush::default());
    let t = setup_with(Some(relay.clone()));
    let (owner, _, _) = sign_up(&t, "ack_owner");
    let (outsider, _, _) = sign_up(&t, "ack_other");
    let login = start(&t, "Phone");
    call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code":login.user_code}), Some(&owner.access_token));
    let LoginPollResponse::Done { session: phone } = poll(&t, &login) else { panic!() };
    let state = serde_json::json!({"track":null,"playing":false,"position_ms":0,"duration_ms":null,"volume":0.5,"available":true,"acknowledgements":true});
    let poll_path = "/v1/devices/playback/poll";
    call(&t, Method::Post, poll_path, state.clone(), Some(&phone.access_token));
    let command = serde_json::json!({"target":phone.device_id,"command":{"action":"pause"}});
    let (status, response) = call(&t, Method::Post, "/v1/devices/playback/command?confirm=1", command, Some(&owner.access_token));
    assert_eq!(status, 200, "{response}");
    assert_eq!(relay.take()[0].1, jam::To::Member(phone.device_id.clone()));
    let id = response["id"].as_str().unwrap();
    let receipt = serde_json::json!({"id":id,"done":false,"error":null});
    for _ in 0..2 {
        let (_, response) = call(&t, Method::Post, poll_path, state.clone(), Some(&phone.access_token));
        assert_eq!(response["deliveries"].as_array().unwrap().len(), 1);
    }
    assert_eq!(call(&t, Method::Post, "/v1/devices/playback/ack", receipt.clone(), Some(&owner.access_token)).0, 404);
    assert_eq!(call(&t, Method::Post, "/v1/devices/playback/receipt", receipt.clone(), Some(&outsider.access_token)).0, 404);
    let (_, pending) = call(&t, Method::Post, "/v1/devices/playback/receipt", receipt.clone(), Some(&owner.access_token));
    assert_eq!(pending["done"], false);
    assert_eq!(call(&t, Method::Post, "/v1/devices/playback/ack", receipt.clone(), Some(&phone.access_token)).0, 200);
    assert_eq!(call(&t, Method::Post, "/v1/devices/playback/receipt", receipt, Some(&owner.access_token)).1["done"], true);
    assert!(call(&t, Method::Post, poll_path, state, Some(&phone.access_token)).1["deliveries"].as_array().unwrap().is_empty());
}

#[test]
fn stale_playlist_writes_cannot_overwrite_a_newer_membership_version() {
    let t = setup();
    let (owner, _, _) = sign_up(&t, "playlist_cas");
    let push = |rev, at, name: &str| call(&t, Method::Post, "/v1/sync/push", serde_json::json!({"items":[{"kind":"playlist","key":"shared","value":{"name":name,"tracks":[]},"deleted":false,"updated_at":at,"rev":rev}]}), Some(&owner.access_token));
    assert_eq!(push(0, 1, "Original").1["accepted"], 1);
    let stale = push(0, 2, "Stale");
    assert_eq!(stale.1["accepted"], 0);
    let rev = stale.1["stale"][0]["rev"].as_u64().unwrap();
    assert_eq!(push(rev, 3, "Merged").1["accepted"], 1);
}

#[test]
fn account_playback_is_private_expires_and_delivers_controls_once() {
    let t = setup();
    let (owner, _, _) = sign_up(&t, "playback_owner");
    let (other, _, _) = sign_up(&t, "playback_other");
    let login = start(&t, "Phone");
    assert_eq!(call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code":login.user_code}), Some(&owner.access_token)).0, 200);
    let LoginPollResponse::Done { session: phone } = poll(&t, &login) else { panic!("phone login failed") };
    let state = serde_json::json!({"track":null,"playing":false,"position_ms":0,"duration_ms":null,"volume":0.5,"available":true});
    let poll = "/v1/devices/playback/poll";
    let send = "/v1/devices/playback/command";
    assert_eq!(call(&t, Method::Post, poll, state.clone(), None).0, 401);
    let (status, own) = call(&t, Method::Post, poll, state.clone(), Some(&owner.access_token));
    assert_eq!(status, 200, "{own}");
    assert_eq!(own["devices"].as_array().unwrap().len(), 2);
    assert!(own["devices"].as_array().unwrap().iter().any(|d| d["device"]["id"] == phone.device_id && d["online"] == false));
    let (_, outsiders) = call(&t, Method::Post, poll, state.clone(), Some(&other.access_token));
    assert!(outsiders["devices"].as_array().unwrap().iter().all(|d| d["device"]["id"] != owner.device_id));
    assert_eq!(call(&t, Method::Post, poll, state.clone(), Some(&phone.access_token)).0, 200);
    let command = serde_json::json!({"target":phone.device_id,"command":{"action":"pause"}});
    assert_eq!(call(&t, Method::Post, send, command.clone(), Some(&other.access_token)).0, 404);
    assert_eq!(call(&t, Method::Post, send, command.clone(), Some(&owner.access_token)).0, 200);
    let (_, own_commands) = call(&t, Method::Post, poll, state.clone(), Some(&owner.access_token));
    assert!(own_commands["commands"].as_array().unwrap().is_empty());
    let (_, received) = call(&t, Method::Post, poll, state.clone(), Some(&phone.access_token));
    assert_eq!(received["commands"].as_array().unwrap().len(), 1);
    let (_, again) = call(&t, Method::Post, poll, state.clone(), Some(&phone.access_token));
    assert!(again["commands"].as_array().unwrap().is_empty());
    let bad = serde_json::json!({"target":phone.device_id,"command":{"action":"volume","volume":2.0}});
    assert_eq!(call(&t, Method::Post, send, bad, Some(&owner.access_token)).0, 400);
    assert_eq!(call(&t, Method::Post, send, command.clone(), Some(&owner.access_token)).0, 200);
    t.h.rt.advance(31_000);
    let (_, expired) = call(&t, Method::Post, poll, state, Some(&phone.access_token));
    assert!(expired["commands"].as_array().unwrap().is_empty());
    t.h.rt.advance(46_000);
    assert_eq!(call(&t, Method::Post, send, command, Some(&owner.access_token)).0, 409);
}

#[test]
fn optional_account_gate_covers_api_and_media_and_tracks_session_lifecycle() {
    let t = setup();
    t.s.update_config(|c| { c.require_account = true; c.rate_limit_per_min = 1000; });
    let (status, health) = call(&t,Method::Get,"/v1/health",serde_json::Value::Null,None);
    assert_eq!(status,200);
    assert_eq!(health["account_required"],true);
    // Sign-up and login must work even when guests cannot call the API.
    let (session,_,_) = sign_up(&t,"members_only");
    let req = |path: &str, token: Option<&str>| Request::new(Method::Get,path,
        token.map(|v| vec![("authorization".into(),format!("Bearer {v}"))]).unwrap_or_default(),vec![],Some("gate-test".into()));
    block_on(async {
        for path in ["/v1/search?q=test","/v1/resolve","/v1/lyrics","/v1/audio/youtube/abcdefghijk",
            "/v1/download/youtube/abcdefghijk","/v1/upstream?url=https://example.com","/v1/access","/v1/profiles/someone"] {
            for token in [None,Some("forged"),Some(session.refresh_token.as_str())] {
                let error = t.s.admit(&req(path,token),path.contains("audio")).await.unwrap_err();
                assert_eq!(error.status,401,"{path}");
                assert_eq!(error.body.code,"account_required");
            }
            assert!(t.s.admit(&req(path,Some(&session.access_token)),false).await.unwrap());
        }
        let mut duplicate = req("/v1/access",Some(&session.access_token));
        duplicate.headers.push(("authorization".into(),"Bearer forged".into()));
        assert_eq!(t.s.admit(&duplicate,false).await.unwrap_err().status,401);
        assert_eq!(t.s.handle(req("/v1/access",None)).await.status,401);
        assert_eq!(t.s.handle(req("/v1/access",Some(&session.access_token))).await.status,200);
    });
    t.h.rt.advance(ACCESS_TTL);
    assert_eq!(block_on(t.s.admit(&req("/v1/access",Some(&session.access_token)),false)).unwrap_err().status,401);
    let (status,value) = call(&t,Method::Post,"/v1/account/token/refresh",serde_json::json!({"refresh_token":session.refresh_token}),None);
    assert_eq!(status,200,"{value}");
    let fresh: Session = serde_json::from_value(value).unwrap();
    block_on(async {
        let request = req("/v1/access",Some(&fresh.access_token));
        assert!(t.s.admit(&request,false).await.unwrap());
        let accounts = t.s.accounts.as_ref().unwrap();
        let (user,device) = accounts.abuse_identity(&t.s.services(),&request).await.unwrap();
        accounts.store.delete_device(&user,&device.id).await.unwrap();
        assert_eq!(t.s.admit(&request,false).await.unwrap_err().status,401);
        t.s.update_config(|c| c.require_account = false);
        assert_eq!(t.s.handle(req("/v1/access",None)).await.status,200);
    });
}

#[test]
fn youtube_account_switch_keeps_youtube_media_for_accounts() {
    let t = setup();
    t.s.update_config(|c| { c.youtube_require_account = true; c.rate_limit_per_min = 1000; c.proxy_audio = true; });
    let (_, health) = call(&t,Method::Get,"/v1/health",serde_json::Value::Null,None);
    let caps: Vec<&str> = health["capabilities"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert!(caps.contains(&"accounts") && caps.contains(&"jam") && caps.contains(&"youtube_hints"), "{caps:?}");
    let (session,_,_) = sign_up(&t,"yt_member");
    let req = |path: &str, token: Option<&str>| Request::new(Method::Get,path,
        token.map(|v| vec![("authorization".into(),format!("Bearer {v}"))]).unwrap_or_default(),vec![],Some("yt-gate".into()));
    block_on(async {
        for path in ["/v1/audio/youtube/abcdefghijk","/v1/download/youtube/abcdefghijk"] {
            for token in [None,Some("forged")] {
                let error = t.s.admit(&req(path,token),true).await.unwrap_err();
                assert_eq!((error.status,error.body.code.as_str()),(401,"account_required"),"{path}");
            }
            assert!(t.s.admit(&req(path,Some(&session.access_token)),true).await.unwrap());
        }
        // Other sources and the metadata API stay open to guests.
        for path in ["/v1/audio/audius/abc123","/v1/search?q=x","/v1/resolve"] {
            assert!(!t.s.admit(&req(path,None),path.contains("audio")).await.unwrap(),"{path}");
        }
    });
    t.s.update_config(|c| c.youtube_require_account = false);
    assert!(!block_on(t.s.admit(&req("/v1/audio/youtube/abcdefghijk",None),true)).unwrap());
}

#[test]
fn account_gate_fails_closed_without_account_backend() {
    let h = harness(Default::default());
    let server = Server::from_arc(h.svc.clone());
    server.update_config(|c| c.require_account = true);
    for path in ["/v1/access","/v1/audio/youtube/abcdefghijk"] {
        let request = Request::new(Method::Get,path,vec![],vec![],None);
        assert_eq!(block_on(server.admit(&request,true)).unwrap_err().status,503);
    }
    assert_eq!(block_on(server.handle(Request::new(Method::Get,"/v1/health",vec![],vec![],None))).status,200);
}

#[test]
fn abuse_privileges_require_live_sessions_and_account_quotas_survive_ip_changes() {
    let t = setup();
    let (session, _, _) = sign_up(&t, "abuse_test");
    t.s.update_config(|c| c.rate_limit_per_min = 2);
    t.h.rt.advance(61_000);
    let req = |token: &str, ip: &str| Request::new(Method::Get, "/v1/lyrics",
        vec![("authorization".into(), format!("Bearer {token}")), (glyph_protocol::INSTALL_ID_HEADER.into(), "fake-device".into())],
        vec![], Some(ip.into()));
    block_on(async {
        // Unverified bearer and device headers grant no privileges.
        assert!(!t.s.admit(&req("forged", "guest"), false).await.unwrap());
        assert!(!t.s.admit(&req("forged", "guest"), false).await.unwrap());
        assert_eq!(t.s.admit(&req("forged", "guest"), false).await.unwrap_err().status, 429);
        for i in 0..8 {
            assert!(t.s.admit(&req(&session.access_token, &format!("member-{i}")), false).await.unwrap());
        }
        assert_eq!(t.s.admit(&req(&session.access_token, "another-ip"), false).await.unwrap_err().status, 429);
        let accounts = t.s.accounts.as_ref().unwrap();
        assert!(accounts.abuse_identity(&t.s.services(), &req(&session.access_token, "ip")).await.is_some());
        t.h.rt.advance(ACCESS_TTL);
        assert!(accounts.abuse_identity(&t.s.services(), &req(&session.access_token, "ip")).await.is_none());
    });
}

#[test]
fn revoked_device_immediately_loses_abuse_privileges() {
    let t = setup();
    let (session, _, _) = sign_up(&t, "revoked_test");
    let req = Request::new(Method::Get, "/v1/lyrics", vec![("authorization".into(), format!("Bearer {}", session.access_token))], vec![], None);
    block_on(async {
        let accounts = t.s.accounts.as_ref().unwrap();
        let (user, device) = accounts.abuse_identity(&t.s.services(), &req).await.unwrap();
        accounts.store.delete_device(&user, &device.id).await.unwrap();
        assert!(accounts.abuse_identity(&t.s.services(), &req).await.is_none());
    });
}

#[test]
fn bans_block_accounts_their_addresses_and_hardware() {
    use crate::bans::{hardware_hash, ip_hash, Ban, BanKind, BanList};
    let t = setup();
    let (session, _, _) = sign_up(&t, "ban_test");
    let req = |ip: &str, hw: Option<&str>| {
        let mut h = vec![("authorization".to_string(), format!("Bearer {}", session.access_token))];
        if let Some(hw) = hw {
            h.push((glyph_protocol::HARDWARE_ID_HEADER.to_string(), hw.to_string()));
        }
        Request::new(Method::Get, "/v1/lyrics", h, vec![], Some(ip.into()))
    };
    let guest = |ip: &str| Request::new(Method::Get, "/v1/lyrics", vec![], vec![], Some(ip.into()));
    let ban = |kind, value: String| Ban { id: "b".into(), kind, value, label: None, reason: None, created_at: 0, expires_at: None, origin: None };
    let set = |bans: Vec<Ban>| t.s.update_config(|c| c.bans = Arc::new(BanList { salt: "s".into(), bans }));
    block_on(async {
        set(vec![]);
        // The device remembers hashes of what it used, never the address.
        assert!(t.s.admit(&req("198.51.100.1", Some("hw-0123456789")), false).await.is_ok());
        let accounts = t.s.accounts.as_ref().unwrap();
        let (user, device) = accounts.abuse_identity(&t.s.services(), &req("198.51.100.1", None)).await.unwrap();
        assert_eq!(device.ip_hash, ip_hash("s", "198.51.100.1"));
        assert_eq!(device.hw_hash, hardware_hash("hw-0123456789"));

        set(vec![ban(BanKind::Account, user.clone())]);
        let e = t.s.admit(&req("203.0.113.9", None), false).await.unwrap_err();
        assert_eq!((e.status, e.body.code.as_str()), (403, "banned"));
        assert!(t.s.admit(&guest("203.0.113.9"), false).await.is_ok(), "only the account is banned");

        // Its address and hardware, without the account.
        set(vec![ban(BanKind::IpHash, device.ip_hash.clone().unwrap()), ban(BanKind::Hardware, device.hw_hash.clone().unwrap())]);
        assert_eq!(t.s.admit(&guest("198.51.100.1"), false).await.unwrap_err().status, 403);
        assert!(t.s.admit(&guest("198.51.100.2"), false).await.is_ok());
        let reinstalled = Request::new(Method::Get, "/v1/lyrics", vec![(glyph_protocol::HARDWARE_ID_HEADER.into(), "hw-0123456789".into())], vec![], Some("192.0.2.5".into()));
        assert_eq!(t.s.admit(&reinstalled, false).await.unwrap_err().status, 403);

        set(vec![ban(BanKind::Ip, "192.0.2.0/24".into())]);
        assert_eq!(t.s.admit(&guest("192.0.2.77"), false).await.unwrap_err().status, 403);
        assert!(t.s.handle(guest("192.0.2.77")).await.status == 403, "site pages too");
    });
}

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
    setup_with(None)
}

fn setup_with(push: Option<Arc<dyn jam::JamPush>>) -> T {
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
        push,
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

    let (s, _) = push(&a, vec![item("playlist", "oversized", Some(serde_json::json!({"tracks": vec![serde_json::json!({}); 5001]})), now)]);
    assert_eq!(s, 400);
    let (s, _) = push(&a, vec![item("playlist", "invalid", Some(serde_json::json!({"tracks": "untrusted"})), now)]);
    assert_eq!(s, 400);

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
        push: None,
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
    // Link cards, the embed player and oEmbed.
    let get = |p: &str| block_on(t.s.handle(Request::new(Method::Get, p, vec![], vec![], None)));
    assert_eq!(get(&format!("/p/{id}/cover")).status, 302);
    assert_eq!(get(&format!("/p/{id}/cover/0")).status, 302);
    assert_eq!(get(&format!("/p/{id}/listen/999")).status, 404);
    assert_eq!(get(&format!("/p/{id}/nope")).status, 404);
    assert_eq!(get(&format!("/embed/p/{id}")).status, 200);
    let o = get(&format!("/oembed?format=json&url={ORIGIN}/p/{id}"));
    assert_eq!(o.status, 200);
    let o: serde_json::Value = serde_json::from_slice(o.bytes().unwrap()).unwrap();
    assert_eq!((o["type"].as_str(), o["author_name"].as_str()), (Some("rich"), Some("@gisela")));
    assert_eq!(get("/oembed?url=https://example.com/x").status, 404);

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

#[test]
fn profiles_are_private_until_opened_and_show_only_what_was_picked() {
    let t = setup();
    let (owner, _, _) = sign_up(&t, "owner");
    let (viewer, _, _) = sign_up(&t, "viewer");
    let now = t.h.rt.now_ms();
    let bearer = |s: &Session| vec![("authorization".to_string(), format!("Bearer {}", s.access_token))];
    let cover = b"\x89PNG\r\n\x1a\ncover".to_vec();
    let cover_sha = hex(&Sha256::digest(&cover));
    let r = block_on(t.s.handle(Request::new(Method::Put, &format!("/v1/sync/covers/{cover_sha}"), bearer(&owner), cover.clone(), None)));
    assert_eq!(r.status, 200);
    let track = serde_json::json!({"title": "Song", "artist": "Band"});
    let items = vec![
        item("playlist", "p1", Some(serde_json::json!({"name": "Shown", "tracks": [track, track], "cover": cover_sha})), now),
        item("playlist", "p2", Some(serde_json::json!({"name": "Hidden", "tracks": [track]})), now),
    ];
    let (s, _) = call(&t, Method::Post, "/v1/sync/push", serde_json::json!({"items": items}), Some(&owner.access_token));
    assert_eq!(s, 200);

    // Signed-in only.
    let (s, _) = call(&t, Method::Get, "/v1/profiles/owner", serde_json::Value::Null, None);
    assert_eq!(s, 401);
    // Private by default: handle only.
    let (s, v) = call(&t, Method::Get, "/v1/profiles/@Owner", serde_json::Value::Null, Some(&viewer.access_token));
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["private"].as_bool(), v["own"].as_bool(), v["playlists"].as_array().map(|a| a.len())), (Some(true), Some(false), Some(0)));
    let (s, _) = call(&t, Method::Get, "/v1/profiles/nobody", serde_json::Value::Null, Some(&viewer.access_token));
    assert_eq!(s, 404);

    // Picture: stored, served by its hash, the old one removed when replaced.
    let make_pic = |color| {
        let image = ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_pixel(8, 8, ::image::Rgba(color)));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, ::image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    };
    let pic = make_pic([10,20,30,255]);
    let r = block_on(t.s.handle(Request::new(Method::Put, "/v1/profile/avatar", bearer(&owner), pic.clone(), None)));
    assert_eq!(r.status, 200);
    let settings: serde_json::Value = serde_json::from_slice(r.bytes().unwrap()).unwrap();
    let pic_sha = settings["avatar"].as_str().unwrap().to_string();
    let r = block_on(t.s.handle(Request::new(Method::Put, "/v1/profile/avatar", bearer(&owner), b"text".to_vec(), None)));
    assert_eq!(r.status, 400);
    let r = block_on(t.s.handle(Request::new(Method::Get, &format!("/v1/profiles/owner/images/{pic_sha}"), bearer(&viewer), vec![], None)));
    assert_eq!(r.status, 200);
    assert_eq!(&r.bytes().unwrap()[8..12], b"WEBP");
    assert_eq!(::image::load_from_memory(r.bytes().unwrap()).unwrap().to_rgba8().get_pixel(0,0).0, [10,20,30,255]);
    let pic2 = make_pic([30,20,10,255]);
    block_on(t.s.handle(Request::new(Method::Put, "/v1/profile/avatar", bearer(&owner), pic2, None)));
    assert!(!t.blobs.objects.lock().unwrap().contains_key(&format!("users/{}/profile/{pic_sha}", owner.account.id)));

    // Open the profile, show one playlist and the recent songs.
    let (s, v) = call(&t, Method::Patch, "/v1/profile", serde_json::json!({"public": true, "show_recent": true, "playlists": ["p1", "p1", "gone"]}), Some(&owner.access_token));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["playlists"], serde_json::json!(["p1", "gone"]));
    let (s, v) = call(&t, Method::Put, "/v1/profile/recent", serde_json::json!({"tracks": [track]}), Some(&owner.access_token));
    assert_eq!((s, v["stored"].as_bool()), (200, Some(true)));
    let (_, v) = call(&t, Method::Get, "/v1/profiles/owner", serde_json::Value::Null, Some(&viewer.access_token));
    assert_eq!(v["private"], false);
    let pl = &v["playlists"][0];
    assert_eq!(v["playlists"].as_array().map(|a| a.len()), Some(1));
    assert_eq!((&pl["id"], &pl["name"], &pl["songs"], &pl["cover"]), (&serde_json::json!("p1"), &serde_json::json!("Shown"), &serde_json::json!(2), &serde_json::json!(cover_sha)));
    // The first songs ride along for the app's cover mosaic.
    assert_eq!(pl["preview"].as_array().map(|a| a.len()), Some(2));
    assert_eq!(v["recent"], serde_json::json!([track]));
    let (s, v) = call(&t, Method::Get, "/v1/profiles/owner/playlists/p1", serde_json::Value::Null, Some(&viewer.access_token));
    assert_eq!((s, v["tracks"].as_array().map(|a| a.len())), (200, Some(2)));
    let (s, _) = call(&t, Method::Get, "/v1/profiles/owner/playlists/p2", serde_json::Value::Null, Some(&viewer.access_token));
    assert_eq!(s, 404);
    let r = block_on(t.s.handle(Request::new(Method::Get, &format!("/v1/profiles/owner/images/{cover_sha}"), bearer(&viewer), vec![], None)));
    assert_eq!(r.status, 200);

    // Recent songs off: cleared. Private again: nothing, except for the owner.
    call(&t, Method::Patch, "/v1/profile", serde_json::json!({"show_recent": false, "public": false}), Some(&owner.access_token));
    let (_, v) = call(&t, Method::Get, "/v1/profiles/owner", serde_json::Value::Null, Some(&owner.access_token));
    assert_eq!((v["own"].as_bool(), v["recent"].as_array().map(|a| a.len()), v["playlists"].as_array().map(|a| a.len())), (Some(true), Some(0), Some(1)));
    let (s, _) = call(&t, Method::Get, "/v1/profiles/owner/playlists/p1", serde_json::Value::Null, Some(&viewer.access_token));
    assert_eq!(s, 404);
    let r = block_on(t.s.handle(Request::new(Method::Get, &format!("/v1/profiles/owner/images/{cover_sha}"), bearer(&viewer), vec![], None)));
    assert_eq!(r.status, 404);

    // Deleting the account removes the profile and its pictures.
    let (s, _) = call(&t, Method::Delete, "/v1/account", serde_json::json!({"confirm_handle": "owner"}), Some(&owner.access_token));
    assert_eq!(s, 200);
    assert!(!t.blobs.objects.lock().unwrap().keys().any(|k| k.starts_with(&format!("users/{}/", owner.account.id))));
    let (s, _) = call(&t, Method::Get, "/v1/profiles/owner", serde_json::Value::Null, Some(&viewer.access_token));
    assert_eq!(s, 404);
}

#[test]
fn device_location_is_host_derived_and_logout_revokes_both_tokens() {
    let t=setup();
    let (alice,_,_)=sign_up(&t,"alice_location");
    let (bob,_,_)=sign_up(&t,"bob_location");
    let headers=vec![("authorization".into(),format!("Bearer {}",alice.access_token)),("x-device-location".into(),"Forged city".into()),("cf-region".into(),"Forged region".into())];
    let request=Request::new(Method::Get,"/v1/devices",headers.clone(),vec![],None);
    let response=block_on(t.s.handle(request));
    let devices:DevicesResponse=serde_json::from_slice(response.bytes().unwrap()).unwrap();
    assert_eq!(devices.devices[0].approximate_location,None,"client headers cannot supply location");
    let mut request=Request::new(Method::Get,"/v1/devices",headers,vec![],None);
    request.approximate_location=Some("Berlin, DE".into());
    let response=block_on(t.s.handle(request));
    let devices:DevicesResponse=serde_json::from_slice(response.bytes().unwrap()).unwrap();
    assert_eq!(devices.devices[0].approximate_location.as_deref(),Some("Berlin, DE"));
    assert_eq!(call(&t,Method::Delete,&format!("/v1/devices/{}",bob.device_id),serde_json::Value::Null,Some(&alice.access_token)).0,404);
    assert_eq!(call(&t,Method::Get,"/v1/devices",serde_json::Value::Null,Some(&bob.access_token)).0,200);
    assert_eq!(call(&t,Method::Delete,&format!("/v1/devices/{}",alice.device_id),serde_json::Value::Null,Some(&alice.access_token)).0,200);
    assert_eq!(call(&t,Method::Get,"/v1/devices",serde_json::Value::Null,Some(&alice.access_token)).0,401);
    assert_eq!(call(&t,Method::Post,"/v1/account/token/refresh",serde_json::json!({"refresh_token":alice.refresh_token}),None).0,401);
}

#[test]
fn jam_relays_between_host_and_members() {
    let t = setup();
    t.s.update_config(|c| c.rate_limit_per_min = 1000);
    let none = serde_json::Value::Null;
    let song = serde_json::json!({"id": "7", "source": "audius", "title": "Song", "artist": "Band", "duration_ms": 200000});
    assert_eq!(call(&t, Method::Post, "/v1/jam", none.clone(), None).0, 401);
    let (host, _, _) = sign_up(&t, "jam_host");
    let (ana, _, _) = sign_up(&t, "jam_ana");
    let (ben, _, _) = sign_up(&t, "jam_ben");

    let (s, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    assert_eq!(s, 200, "{v}");
    let code = v["code"].as_str().unwrap().to_string();
    assert!(glyph_protocol::jam::normalize_code(&code).is_some());
    assert_eq!((v["rev"].as_u64(), v["host"].as_str(), v["host_online"].as_bool()), (Some(1), Some("jam_host"), Some(true)));
    assert_eq!(v["link"].as_str().map(String::from), Some(format!("{ORIGIN}/jam/{code}")));
    // The invite page needs no sign-in and opens the app.
    let page = block_on(t.s.handle(Request::new(Method::Get, &format!("/jam/{}", code.to_lowercase()), vec![], vec![], None)));
    assert_eq!(page.status, 200);
    let html = String::from_utf8_lossy(page.bytes().unwrap()).to_string();
    assert!(html.contains(&format!("glyph://jam/{code}")) && html.contains("@jam_host"));
    assert_eq!(block_on(t.s.handle(Request::new(Method::Get, "/jam/AAAAAA", vec![], vec![], None))).status, 404);
    let at = |p: &str| format!("/v1/jam/{code}{p}");

    // Clock sync.
    let (s, v) = call(&t, Method::Get, &at("/time"), none.clone(), Some(&ana.access_token));
    assert_eq!((s, v["server_ms"].as_u64()), (200, Some(t.h.rt.now_ms())));

    // Only members read the state; joining tells the host.
    assert_eq!(call(&t, Method::Get, &at("?rev=0"), none.clone(), Some(&ana.access_token)).0, 403);
    assert_eq!(call(&t, Method::Post, "/v1/jam/AAAAAA/join", none.clone(), Some(&ana.access_token)).0, 404);
    let (s, v) = call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token));
    assert_eq!((s, v["state"]["code"].as_str(), v["waiting"].as_bool()), (200, Some(code.as_str()), None), "{v}");
    assert_eq!(call(&t, Method::Get, &at("?rev=0"), none.clone(), Some(&ana.access_token)).1["rev"], 1);
    // A guest asks for a song; only the host picks requests up and publishes.
    let (s, _) = call(&t, Method::Post, &at("/op"), serde_json::json!({"op": "add", "track": song}), Some(&ana.access_token));
    assert_eq!(s, 200);
    assert_eq!(call(&t, Method::Post, &at("/op"), serde_json::json!({"op": "react", "reaction": "fire"}), Some(&ana.access_token)).0, 200);
    for fake in ["join", "knock", "leave"] {
        assert_eq!(call(&t, Method::Post, &at("/op"), serde_json::json!({"op": fake}), Some(&ana.access_token)).0, 400);
    }
    assert_eq!(call(&t, Method::Post, &at("/op"), serde_json::json!({"op": "vote", "item": "x", "up": true}), Some(&ben.access_token)).0, 403);
    assert_eq!(call(&t, Method::Get, &at("/ops"), none.clone(), Some(&ana.access_token)).0, 403);
    let (s, v) = call(&t, Method::Get, &at("/ops"), none.clone(), Some(&host.access_token));
    assert_eq!(s, 200, "{v}");
    let ops = v["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 3);
    assert_eq!((ops[0]["from"].as_str(), ops[0]["op"]["op"].as_str()), (Some("jam_ana"), Some("join")));
    assert_eq!(ops[1]["op"]["track"]["id"], "7");
    assert_eq!(ops[2]["op"]["reaction"], "fire");

    let now_ms = t.h.rt.now_ms();
    let state = serde_json::json!({
        "code": "IGNORED", "rev": 99, "host": "someone_else", "host_online": false,
        "members": [{"handle": "jam_ana", "host": true}],
        "now": {"track": song, "position_ms": 5000, "playing": true, "at_ms": now_ms - 300},
        "queue": [{"id": "audius:7", "track": song, "by": "jam_ana"}],
        "feed": [{"at_ms": 1, "who": "jam_ana", "what": {"kind": "joined"}}],
    });
    assert_eq!(call(&t, Method::Post, &at("/state"), state.clone(), Some(&ana.access_token)).0, 403);
    let (s, v) = call(&t, Method::Post, &at("/state"), state.clone(), Some(&host.access_token));
    assert_eq!((s, v["rev"].as_u64()), (200, Some(2)), "{v}");
    let (_, v) = call(&t, Method::Get, &at("?rev=1"), none.clone(), Some(&ana.access_token));
    assert_eq!((v["rev"].as_u64(), v["code"].as_str(), v["host"].as_str()), (Some(2), Some(code.as_str()), Some("jam_host")));
    assert_eq!(v["members"][0], serde_json::json!({"handle": "jam_host", "host": true}));
    assert_eq!(v["members"][1], serde_json::json!({"handle": "jam_ana"}));
    // The host's clock estimate stays when it is close to the server's.
    assert_eq!(v["now"]["at_ms"].as_u64(), Some(now_ms - 300));
    assert_eq!((v["host_online"].as_bool(), v["feed"][0]["what"]["kind"].as_str()), (Some(true), Some("joined")));
    // A far-off estimate is replaced.
    let mut skewed = state.clone();
    skewed["now"]["at_ms"] = serde_json::json!(1);
    call(&t, Method::Post, &at("/state"), skewed, Some(&host.access_token));
    assert_eq!(call(&t, Method::Get, &at("?rev=2"), none.clone(), Some(&ana.access_token)).1["now"]["at_ms"].as_u64(), Some(now_ms));

    // Removed members cannot come back, not even under another handle.
    assert_eq!(call(&t, Method::Post, &at("/kick"), serde_json::json!({"handle": "@jam_ana"}), Some(&ana.access_token)).0, 403);
    assert_eq!(call(&t, Method::Post, &at("/kick"), serde_json::json!({"handle": "@jam_ana"}), Some(&host.access_token)).0, 200);
    assert_eq!(call(&t, Method::Get, &at("?rev=0"), none.clone(), Some(&ana.access_token)).0, 403);
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token)).0, 403);
    let (s, _) = call(&t, Method::Patch, "/v1/account", serde_json::json!({"handle": "jam_ana2"}), Some(&ana.access_token));
    assert_eq!(s, 200);
    let (s, v) = call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token));
    assert_eq!((s, v["code"].as_str()), (403, Some("jam_removed")));
    // Leaving tells the host too.
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ben.access_token)).0, 200);
    assert_eq!(call(&t, Method::Post, &at("/leave"), none.clone(), Some(&ben.access_token)).0, 200);
    let (_, v) = call(&t, Method::Get, &at("/ops"), none.clone(), Some(&host.access_token));
    let kinds: Vec<&str> = v["ops"].as_array().unwrap().iter().filter_map(|o| o["op"]["op"].as_str()).collect();
    assert_eq!(kinds, ["join", "leave"]);

    // A new jam ends the old one; ending it is for the host only.
    let (_, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    let code2 = v["code"].as_str().unwrap().to_string();
    assert_ne!(code2, code);
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ben.access_token)).0, 404);
    assert_eq!(call(&t, Method::Post, &format!("/v1/jam/{code2}/join"), none.clone(), Some(&ben.access_token)).0, 200);
    assert_eq!(call(&t, Method::Delete, &format!("/v1/jam/{code2}"), none.clone(), Some(&ben.access_token)).0, 403);
    assert_eq!(call(&t, Method::Delete, &format!("/v1/jam/{code2}"), none.clone(), Some(&host.access_token)).0, 200);
    let (s, v) = call(&t, Method::Get, &format!("/v1/jam/{code2}?rev=0"), none.clone(), Some(&ben.access_token));
    assert_eq!((s, v["code"].as_str()), (404, Some("jam_ended")));

    // Jams end by themselves.
    let (_, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    let code3 = v["code"].as_str().unwrap().to_string();
    t.h.rt.advance(glyph_protocol::jam::MAX_AGE_MS + 1);
    let fresh = call(&t, Method::Post, "/v1/account/token/refresh", serde_json::json!({"refresh_token": host.refresh_token}), None).1;
    let tok = fresh["access_token"].as_str().unwrap().to_string();
    assert_eq!(call(&t, Method::Get, &format!("/v1/jam/{code3}?rev=0"), none, Some(&tok)).0, 404);
}

#[test]
fn jam_host_lets_people_in_and_hands_over_when_gone() {
    let t = setup();
    t.s.update_config(|c| c.rate_limit_per_min = 1000);
    let none = serde_json::Value::Null;
    let (host, _, _) = sign_up(&t, "jam_h2");
    let (ana, _, _) = sign_up(&t, "jam_a2");
    let (ben, _, _) = sign_up(&t, "jam_b2");
    let (_, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    let code = v["code"].as_str().unwrap().to_string();
    let at = |p: &str| format!("/v1/jam/{code}{p}");
    let publish = |extra: serde_json::Value| {
        let mut s = serde_json::json!({"code": code, "host": "jam_h2", "members": [{"handle": "jam_a2"}]});
        if let (Some(s), Some(e)) = (s.as_object_mut(), extra.as_object()) {
            s.extend(e.clone());
        }
        call(&t, Method::Post, &at("/state"), s, Some(&host.access_token))
    };
    assert_eq!(publish(serde_json::json!({"approve_joins": true})).0, 200);

    // Knocking waits for the host; asking again does not knock twice.
    let (s, v) = call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token));
    assert_eq!((s, v["waiting"].as_bool()), (202, Some(true)), "{v}");
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token)).0, 202);
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ben.access_token)).0, 202);
    assert_eq!(call(&t, Method::Get, &at("?rev=0"), none.clone(), Some(&ana.access_token)).0, 403);
    let (_, v) = call(&t, Method::Get, &at("/ops"), none.clone(), Some(&host.access_token));
    let knocks: Vec<&str> = v["ops"].as_array().unwrap().iter().filter(|o| o["op"]["op"] == "knock").filter_map(|o| o["from"].as_str()).collect();
    assert_eq!(knocks, ["jam_a2", "jam_b2"]);
    assert_eq!(call(&t, Method::Post, &at("/admit"), serde_json::json!({"handle": "jam_a2", "allow": true}), Some(&ana.access_token)).0, 403);
    assert_eq!(call(&t, Method::Post, &at("/admit"), serde_json::json!({"handle": "jam_a2", "allow": true}), Some(&host.access_token)).0, 200);
    assert_eq!(call(&t, Method::Post, &at("/admit"), serde_json::json!({"handle": "jam_b2", "allow": false}), Some(&host.access_token)).0, 200);
    assert_eq!(call(&t, Method::Post, &at("/admit"), serde_json::json!({"handle": "nobody", "allow": true}), Some(&host.access_token)).0, 404);
    let (s, v) = call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token));
    assert_eq!((s, v["state"]["code"].as_str()), (200, Some(code.as_str())));
    let (s, v) = call(&t, Method::Post, &at("/join"), none.clone(), Some(&ben.access_token));
    assert_eq!((s, v["code"].as_str()), (403, Some("jam_denied")));
    let (_, v) = call(&t, Method::Get, &at("/ops"), none.clone(), Some(&host.access_token));
    assert_eq!(v["ops"][0]["op"]["op"], "join");

    // The host is around: no takeover.
    assert_eq!(call(&t, Method::Post, &at("/claim"), none.clone(), Some(&ana.access_token)).1["code"], "host_online");
    // The host's app is gone for a while.
    t.h.rt.advance(glyph_protocol::jam::HOST_TIMEOUT_MS + 1);
    assert_eq!(call(&t, Method::Get, &at("?rev=0"), none.clone(), Some(&ana.access_token)).1["host_online"], false);
    assert_eq!(call(&t, Method::Post, &at("/claim"), none.clone(), Some(&ben.access_token)).0, 403);
    t.h.rt.advance(glyph_protocol::jam::HANDOVER_MS);
    let (s, v) = call(&t, Method::Post, &at("/claim"), none.clone(), Some(&ana.access_token));
    assert_eq!((s, v["host"].as_str(), v["host_online"].as_bool()), (200, Some("jam_a2"), Some(true)), "{v}");
    assert_eq!(v["members"][0], serde_json::json!({"handle": "jam_a2", "host": true}));
    // The old host is a guest now; the new one runs the jam.
    let (s, v) = call(&t, Method::Get, &at("/ops"), none.clone(), Some(&host.access_token));
    assert_eq!((s, v["code"].as_str()), (403, Some("not_jam_host")));
    assert_eq!(call(&t, Method::Get, &at("?rev=0"), none.clone(), Some(&host.access_token)).0, 200);
    assert_eq!(call(&t, Method::Post, &at("/state"), serde_json::json!({"code": code, "host": "x"}), Some(&ana.access_token)).0, 200);
}

/// Records what the server pushes.
#[derive(Default)]
struct RecPush(std::sync::Mutex<Vec<(String, jam::To, Option<String>, Option<u16>)>>);

#[async_trait::async_trait]
impl jam::JamPush for RecPush {
    async fn send(&self, code: &str, to: jam::To, msg: Option<&str>, close: Option<u16>) {
        self.0.lock().unwrap().push((code.to_string(), to, msg.map(String::from), close));
    }
}

impl RecPush {
    fn take(&self) -> Vec<(String, jam::To, Option<String>, Option<u16>)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// A push room that also keeps the devices' playback states.
#[derive(Default)]
struct StatePush(std::sync::Mutex<std::collections::HashMap<String, glyph_protocol::playback::State>>);

#[async_trait::async_trait]
impl jam::JamPush for StatePush {
    async fn send(&self, _code: &str, _to: jam::To, _msg: Option<&str>, _close: Option<u16>) {}

    async fn playback_states(&self, _scope: &str, device: &str, report: Option<&glyph_protocol::playback::State>) -> Option<Vec<(String, glyph_protocol::playback::State)>> {
        let mut states = self.0.lock().unwrap();
        if let Some(state) = report {
            states.insert(device.to_string(), state.clone());
        }
        Some(states.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }
}

#[test]
fn account_playback_states_live_in_the_push_room() {
    let room = Arc::new(StatePush::default());
    let t = setup_with(Some(room.clone()));
    let (owner, _, _) = sign_up(&t, "room_owner");
    let login = start(&t, "Phone");
    call(&t, Method::Post, "/v1/account/login/approve", serde_json::json!({"user_code":login.user_code}), Some(&owner.access_token));
    let LoginPollResponse::Done { session: phone } = poll(&t, &login) else { panic!() };
    let state = serde_json::json!({"track":null,"playing":true,"position_ms":1000,"duration_ms":null,"volume":0.5,"available":true});
    let poll_path = "/v1/devices/playback/poll";
    assert_eq!(call(&t, Method::Post, poll_path, state.clone(), Some(&phone.access_token)).0, 200);
    assert!(room.0.lock().unwrap().contains_key(&phone.device_id));
    // The room answers for presence: the phone is online without a store entry.
    let (_, own) = call(&t, Method::Post, poll_path, state, Some(&owner.access_token));
    let phone_row = own["devices"].as_array().unwrap().iter().find(|d| d["device"]["id"] == phone.device_id).unwrap().clone();
    assert_eq!((phone_row["online"].as_bool(), phone_row["state"]["playing"].as_bool()), (Some(true), Some(true)));
    let command = serde_json::json!({"target":phone.device_id,"command":{"action":"pause"}});
    assert_eq!(call(&t, Method::Post, "/v1/devices/playback/command", command, Some(&owner.access_token)).0, 200);
}

/// `GET /live` as a WebSocket upgrade: (status, relay header, error code).
fn live(t: &T, code: &str, token: &str, upgrade: bool) -> (u16, Option<String>, Option<String>) {
    let mut headers = vec![("authorization".to_string(), format!("Bearer {token}"))];
    if upgrade {
        headers.push(("upgrade".to_string(), "websocket".to_string()));
    }
    let r = block_on(t.s.handle(Request::new(Method::Get, &format!("/v1/jam/{code}/live"), headers, vec![], Some("1.2.3.4".into()))));
    let relay = r.headers.iter().find(|(k, _)| k == jam::LIVE_HEADER).map(|(_, v)| v.clone());
    let v: serde_json::Value = serde_json::from_slice(r.bytes().unwrap_or_default()).unwrap_or_default();
    (r.status, relay, v["code"].as_str().map(String::from))
}

#[test]
fn jam_pushes_once_someone_listens() {
    use glyph_protocol::jam::{Live, LIVE_CLOSE_AGAIN, LIVE_CLOSE_ENDED, LIVE_CLOSE_REMOVED};
    let none = serde_json::Value::Null;
    // Without a relay there is no /live: the apps long poll.
    let t = setup();
    t.s.update_config(|c| c.rate_limit_per_min = 1000);
    let (host, _, _) = sign_up(&t, "live_h0");
    let (_, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    let code = v["code"].as_str().unwrap().to_string();
    assert_eq!(live(&t, &code, &host.access_token, true), (404, None, Some("jam_push_off".into())));

    let push = Arc::new(RecPush::default());
    let t = setup_with(Some(push.clone()));
    t.s.update_config(|c| c.rate_limit_per_min = 1000);
    let (host, _, _) = sign_up(&t, "live_h");
    let (ana, _, _) = sign_up(&t, "live_a");
    let (ben, _, _) = sign_up(&t, "live_b");
    let (_, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    let code = v["code"].as_str().unwrap().to_string();
    let at = |p: &str| format!("/v1/jam/{code}{p}");
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token)).0, 200);
    // Nobody connected yet: no push spent.
    assert!(push.take().is_empty());

    // Only members, and only as a WebSocket.
    assert_eq!(live(&t, &code, &ben.access_token, true).0, 403);
    assert_eq!(live(&t, &code, &ana.access_token, false).0, 426);
    assert_eq!(live(&t, &code, &ana.access_token, true), (200, Some("guest live_a".into()), None));
    assert_eq!(live(&t, &code, &host.access_token, true), (200, Some("host live_h".into()), None));

    // A publish reaches the guests, as GET would answer it.
    let state = serde_json::json!({"code": code, "host": "live_h", "members": [{"handle": "live_a"}]});
    assert_eq!(call(&t, Method::Post, &at("/state"), state, Some(&host.access_token)).0, 200);
    let sent = push.take();
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].0.as_str(), &sent[0].1, sent[0].3), (code.as_str(), &jam::To::Guests, None));
    match serde_json::from_str::<Live>(sent[0].2.as_deref().unwrap()).unwrap() {
        Live::State { state } => {
            assert_eq!((state.rev, state.host.as_str(), state.host_online), (2, "live_h", true));
            assert_eq!(state.link, Some(format!("{ORIGIN}/jam/{code}")));
            assert_eq!(state.server_ms, t.h.rt.now_ms());
        }
        other => panic!("{other:?}"),
    }

    // Requests ring the host, who picks them up without waiting.
    assert_eq!(call(&t, Method::Post, &at("/op"), serde_json::json!({"op": "react", "reaction": "fire"}), Some(&ana.access_token)).0, 200);
    let sent = push.take();
    assert_eq!((&sent[0].1, sent[0].2.as_deref()), (&jam::To::Host, Some(r#"{"t":"ops"}"#)));
    // Ana's join from the start is still waiting too.
    let (s, v) = call(&t, Method::Get, &at("/ops?wait=0"), none.clone(), Some(&host.access_token));
    assert_eq!((s, v["ops"].as_array().map(Vec::len)), (200, Some(2)));
    let (_, v) = call(&t, Method::Get, &at("/ops?wait=0"), none.clone(), Some(&host.access_token));
    assert_eq!(v["ops"].as_array().map(Vec::len), Some(0));

    // Removed members are cut off.
    assert_eq!(call(&t, Method::Post, &at("/kick"), serde_json::json!({"handle": "live_a"}), Some(&host.access_token)).0, 200);
    let sent = push.take();
    assert_eq!((&sent[0].1, sent[0].3), (&jam::To::Member("live_a".into()), Some(LIVE_CLOSE_REMOVED)));

    // A takeover sends everyone to connect again.
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ben.access_token)).0, 200);
    push.take();
    t.h.rt.advance(glyph_protocol::jam::HANDOVER_MS + 1);
    let fresh = call(&t, Method::Post, "/v1/account/token/refresh", serde_json::json!({"refresh_token": ben.refresh_token}), None).1;
    let ben_tok = fresh["access_token"].as_str().unwrap().to_string();
    assert_eq!(call(&t, Method::Post, &at("/claim"), none.clone(), Some(&ben_tok)).0, 200);
    let sent = push.take();
    assert_eq!((&sent[0].1, sent[0].3), (&jam::To::All, Some(LIVE_CLOSE_AGAIN)));
    assert_eq!(live(&t, &code, &ben_tok, true).1, Some("host live_b".into()));

    // Ending closes every socket.
    assert_eq!(call(&t, Method::Delete, &at(""), none, Some(&ben_tok)).0, 200);
    let sent = push.take();
    assert_eq!((&sent[0].1, sent[0].3), (&jam::To::All, Some(LIVE_CLOSE_ENDED)));
}

#[test]
fn jam_push_budget_ends_in_one_fallback() {
    use crate::traits::RateLimiter;
    use glyph_protocol::jam::Live;
    let none = serde_json::Value::Null;
    let push = Arc::new(RecPush::default());
    let t = setup_with(Some(push.clone()));
    t.s.update_config(|c| c.rate_limit_per_min = 1000);
    let (host, _, _) = sign_up(&t, "budget_h");
    let (_, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    let code = v["code"].as_str().unwrap().to_string();
    assert_eq!(live(&t, &code, &host.access_token, true).0, 200);
    // Use up the rest of today's budget.
    let key = format!("jamdo:{}", t.h.rt.now_ms() / (24 * 3600 * 1000));
    block_on(async {
        for _ in 1..jam::PUSH_BUDGET {
            t.h.svc.limiter.check(&key, jam::PUSH_BUDGET, 86_400).await;
        }
    });
    assert_eq!(live(&t, &code, &host.access_token, true).2, Some("jam_push_busy".into()));
    let state = serde_json::json!({"code": code, "host": "budget_h"});
    for _ in 0..3 {
        assert_eq!(call(&t, Method::Post, &format!("/v1/jam/{code}/state"), state.clone(), Some(&host.access_token)).0, 200);
    }
    // Publishing still works; the sockets got one goodbye, then nothing.
    let sent = push.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(serde_json::from_str::<Live>(sent[0].2.as_deref().unwrap()).unwrap(), Live::Fallback);
}

#[test]
fn jam_position_updates_are_small() {
    use glyph_protocol::jam::Live;
    let none = serde_json::Value::Null;
    let push = Arc::new(RecPush::default());
    let t = setup_with(Some(push.clone()));
    t.s.update_config(|c| c.rate_limit_per_min = 1000);
    let (host, _, _) = sign_up(&t, "now_h");
    let (ana, _, _) = sign_up(&t, "now_a");
    let (_, v) = call(&t, Method::Post, "/v1/jam", none.clone(), Some(&host.access_token));
    let code = v["code"].as_str().unwrap().to_string();
    let at = |p: &str| format!("/v1/jam/{code}{p}");
    assert_eq!(call(&t, Method::Post, &at("/join"), none.clone(), Some(&ana.access_token)).0, 200);
    assert_eq!(live(&t, &code, &ana.access_token, true).0, 200);
    let song = serde_json::json!({"id": "7", "source": "audius", "title": "Song", "artist": "Band", "duration_ms": 200000});
    let state = serde_json::json!({"code": code, "host": "now_h", "members": [{"handle": "now_a"}], "queue": [{"id": "audius:7", "track": song, "by": "now_a"}],
        "now": {"track": song, "position_ms": 1000, "playing": true, "at_ms": t.h.rt.now_ms()}});
    assert_eq!(call(&t, Method::Post, &at("/state"), state.clone(), Some(&host.access_token)).1["rev"], 2);
    push.take();

    // Only the host moves the position; it raises the revision and keeps the rest.
    let moved = serde_json::json!({"track": song, "position_ms": 90000, "playing": true, "at_ms": t.h.rt.now_ms()});
    assert_eq!(call(&t, Method::Post, &at("/now"), moved.clone(), Some(&ana.access_token)).0, 403);
    let (s, v) = call(&t, Method::Post, &at("/now"), moved, Some(&host.access_token));
    assert_eq!((s, v["rev"].as_u64()), (200, Some(3)), "{v}");
    let sent = push.take();
    assert_eq!(&sent[0].1, &jam::To::Guests);
    match serde_json::from_str::<Live>(sent[0].2.as_deref().unwrap()).unwrap() {
        Live::Now { rev, now, .. } => assert_eq!((rev, now.position_ms), (3, 90000)),
        other => panic!("{other:?}"),
    }
    let (_, v) = call(&t, Method::Get, &at("?rev=2"), none.clone(), Some(&ana.access_token));
    assert_eq!((v["rev"].as_u64(), v["now"]["position_ms"].as_u64(), v["queue"][0]["id"].as_str()), (Some(3), Some(90000), Some("audius:7")));

    // A full publish after it wins again.
    assert_eq!(call(&t, Method::Post, &at("/state"), state, Some(&host.access_token)).1["rev"], 4);
    let (_, v) = call(&t, Method::Get, &at("?rev=3"), none, Some(&ana.access_token));
    assert_eq!((v["rev"].as_u64(), v["now"]["position_ms"].as_u64()), (Some(4), Some(1000)));
}
