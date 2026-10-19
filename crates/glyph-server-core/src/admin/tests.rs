use super::auth::cheap_hash;
use super::host::{AdminHost, HostError};
use super::*;
use crate::cache::MemoryCache;
use crate::testutil::*;
use futures::executor::block_on;
use std::sync::Mutex;

const PW: &str = "correct horse battery";

#[derive(Default)]
struct TestHost {
    keys: Mutex<Vec<BTreeMap<String, Option<String>>>>,
    fail: bool,
}

#[async_trait::async_trait]
impl AdminHost for TestHost {
    fn platform(&self) -> &'static str {
        "native"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities { set_keys: true, set_limits: true, logs: true, ..Default::default() }
    }
    async fn persist_keys(&self, v: &BTreeMap<String, Option<String>>) -> Result<Vec<String>, HostError> {
        if self.fail {
            return Err(HostError::Unsupported("worker: use wrangler secret put".into()));
        }
        self.keys.lock().unwrap().push(v.clone());
        Ok(vec!["environment variable overrides the file".into()])
    }
    async fn persist_limits(&self, _: &Limits) -> Result<(), HostError> {
        Ok(())
    }
    async fn companion_status(&self) -> Result<CompanionStatus,HostError> {
        let tool=ToolStatus{available:true,version:Some("0.1.1".into()),configured:true};
        Ok(CompanionStatus{version:"0.3.0".into(),target:"test".into(),enabled:true,ready:true,r_dlp:tool.clone(),yt_dlp:tool.clone(),ffmpeg:tool})
    }
    async fn companion_update(&self, rdlp:bool, apply:bool) -> Result<MaintenanceResponse,HostError> {
        Ok(MaintenanceResponse{info:UpdateInfo{current:"0.1.0".into(),latest:Some("0.1.1".into()),update_available:true,message:"test".into()},
            applied:apply,restart_required:false,message:if rdlp{"r-dlp"}else{"companion"}.into()})
    }
    async fn logs(&self, _: usize, after: Option<u64>) -> Result<LogsResponse, HostError> {
        Ok(LogsResponse { lines: vec!["line".into()], next: after.unwrap_or(0) + 1 })
    }
}

#[test]
fn companion_management_routes_require_admin_auth_and_fixed_methods() {
    let t=setup(true,TestHost::default());
    let pair=login(&t);let token=Some(pair.access_token.as_str());
    let (status,value,_)=call(&t,req(Method::Get,"/admin/v1/companion/status","",token));
    assert_eq!(status,200);assert_eq!(value["r_dlp"]["version"],"0.1.1");
    for (path,rdlp,apply) in [("companion/update/check",false,false),("companion/update/apply",false,true),
        ("companion/r-dlp/check",true,false),("companion/r-dlp/apply",true,true)] {
        let path=format!("/admin/v1/{path}");
        assert_eq!(call(&t,req(Method::Post,&path,"{}",None)).0,401);
        assert_eq!(call(&t,req(Method::Get,&path,"",token)).0,405);
        let (status,value,_)=call(&t,req(Method::Post,&path,"{}",token));
        assert_eq!(status,200);assert_eq!(value["applied"],apply);
        assert_eq!(value["message"],if rdlp{"r-dlp"}else{"companion"});
    }
}

struct T {
    h: Harness,
    server: Server,
}

fn setup(with_admin: bool, host: TestHost) -> T {
    let mut h = harness(all_secrets());
    // Mutate config before wrapping (Arc is unique here).
    Arc::get_mut(&mut h.svc).unwrap().config.admin_password_hash = with_admin.then(|| cheap_hash(PW));
    Arc::get_mut(&mut h.svc).unwrap().config.rate_limit_per_min = 1000;
    let server = Server::from_arc(h.svc.clone()).with_host(Arc::new(host));
    T { h, server }
}

fn req(method: Method, path: &str, body: &str, token: Option<&str>) -> Request {
    let mut headers = vec![];
    if let Some(t) = token {
        headers.push(("authorization".to_string(), format!("Bearer {t}")));
    }
    Request::new(method, path, headers, body.as_bytes().to_vec(), Some("1.2.3.4".into())).with_transport(true, false)
}

fn call(t: &T, r: Request) -> (u16, serde_json::Value, String) {
    let resp = block_on(t.server.handle(r));
    let text = String::from_utf8_lossy(resp.bytes().unwrap_or(b"")).to_string();
    (resp.status, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null), text)
}

fn login(t: &T) -> TokenPair {
    let (s, v, _) = call(t, req(Method::Post, "/admin/v1/login", &format!(r#"{{"password":"{PW}"}}"#), None));
    assert_eq!(s, 200, "{v}");
    serde_json::from_value(v).unwrap()
}

#[test]
fn disabled_without_password() {
    let t = setup(false, TestHost::default());
    let (s, v, _) = call(&t, req(Method::Post, "/admin/v1/login", "{}", None));
    assert_eq!((s, v["code"].as_str()), (404, Some("admin_disabled")));
    let (s, _, _) = call(&t, req(Method::Get, "/admin/v1/status", "", None));
    assert_eq!(s, 404);
}

#[test]
fn requires_https_unless_local() {
    let t = setup(true, TestHost::default());
    let body = format!(r#"{{"password":"{PW}"}}"#);
    let plain = Request::new(Method::Post, "/admin/v1/login", vec![], body.clone().into_bytes(), Some("8.8.8.8".into()));
    let (s, v, _) = call(&t, plain.clone());
    assert_eq!((s, v["code"].as_str()), (403, Some("https_required")));
    let (s, _, _) = call(&t, plain.clone().with_transport(false, true));
    assert_eq!(s, 200);
    let (s, _, _) = call(&t, plain.with_transport(true, false));
    assert_eq!(s, 200);
}

#[test]
fn login_wrong_password_then_lockout_then_recovery() {
    let t = setup(true, TestHost::default());
    let bad = || req(Method::Post, "/admin/v1/login", r#"{"password":"nope nope nope"}"#, None);
    for _ in 0..(MAX_LOGIN_FAILURES - 1) {
        let (s, v, _) = call(&t, bad());
        assert_eq!((s, v["code"].as_str()), (401, Some("invalid_credentials")));
    }
    // 5th failure triggers the lockout; from now on even the right password is refused.
    assert_eq!(call(&t, bad()).0, 401);
    let (s, v, _) = call(&t, req(Method::Post, "/admin/v1/login", &format!(r#"{{"password":"{PW}"}}"#), None));
    assert_eq!((s, v["code"].as_str()), (429, Some("locked_out")));
    assert!(v["retry_after_s"].as_u64().unwrap() > 800);
    // another client is not affected
    let other = Request::new(
        Method::Post,
        "/admin/v1/login",
        vec![],
        format!(r#"{{"password":"{PW}"}}"#).into_bytes(),
        Some("5.5.5.5".into()),
    )
    .with_transport(true, false);
    assert_eq!(call(&t, other).0, 200);
    // after the lockout period the right password works again
    t.h.rt.advance((LOCKOUT_S + 1) * 1000);
    let (s, _, _) = call(&t, req(Method::Post, "/admin/v1/login", &format!(r#"{{"password":"{PW}"}}"#), None));
    assert_eq!(s, 200);
}

#[test]
fn successful_login_resets_failure_counter() {
    let t = setup(true, TestHost::default());
    for _ in 0..3 {
        call(&t, req(Method::Post, "/admin/v1/login", r#"{"password":"wrong wrong"}"#, None));
    }
    login(&t);
    for _ in 0..3 {
        assert_eq!(call(&t, req(Method::Post, "/admin/v1/login", r#"{"password":"wrong wrong"}"#, None)).0, 401);
    }
    login(&t);
}

#[test]
fn login_is_rate_limited_per_ip() {
    let t = setup(true, TestHost::default());
    // 10 attempts per minute max; use a correct password so lockout does not interfere.
    for _ in 0..LOGIN_PER_MIN {
        login(&t);
    }
    let (s, v, _) = call(&t, req(Method::Post, "/admin/v1/login", &format!(r#"{{"password":"{PW}"}}"#), None));
    assert_eq!((s, v["code"].as_str()), (429, Some("rate_limited")));
}

#[test]
fn tokens_gate_everything_and_expire() {
    let t = setup(true, TestHost::default());
    assert_eq!(call(&t, req(Method::Get, "/admin/v1/status", "", None)).0, 401);
    assert_eq!(call(&t, req(Method::Get, "/admin/v1/status", "", Some("garbage"))).0, 401);
    let p = login(&t);
    assert_eq!(call(&t, req(Method::Get, "/admin/v1/status", "", Some(&p.access_token))).0, 200);
    // refresh tokens are not access tokens
    assert_eq!(call(&t, req(Method::Get, "/admin/v1/status", "", Some(&p.refresh_token))).0, 401);
    // after 16 minutes the access token is dead, the refresh token still works
    t.h.rt.advance(16 * 60 * 1000);
    let (s, v, _) = call(&t, req(Method::Get, "/admin/v1/status", "", Some(&p.access_token)));
    assert_eq!((s, v["message"].as_str()), (401, Some("access token expired")));
    let (s, v, _) = call(
        &t,
        req(Method::Post, "/admin/v1/refresh", &format!(r#"{{"refresh_token":"{}"}}"#, p.refresh_token), None),
    );
    assert_eq!(s, 200);
    let p2: TokenPair = serde_json::from_value(v).unwrap();
    assert_eq!(call(&t, req(Method::Get, "/admin/v1/status", "", Some(&p2.access_token))).0, 200);
    // access token cannot refresh
    let (s, _, _) = call(
        &t,
        req(Method::Post, "/admin/v1/refresh", &format!(r#"{{"refresh_token":"{}"}}"#, p2.access_token), None),
    );
    assert_eq!(s, 401);
}

#[test]
fn logout_revokes_the_session() {
    let t = setup(true, TestHost::default());
    let p = login(&t);
    assert_eq!(call(&t, req(Method::Post, "/admin/v1/logout", "", Some(&p.access_token))).0, 200);
    assert_eq!(call(&t, req(Method::Get, "/admin/v1/status", "", Some(&p.access_token))).0, 401);
    let (s, _, _) = call(
        &t,
        req(Method::Post, "/admin/v1/refresh", &format!(r#"{{"refresh_token":"{}"}}"#, p.refresh_token), None),
    );
    assert_eq!(s, 401);
}

#[test]
fn keys_list_is_masked_and_nothing_leaks() {
    let t = setup(true, TestHost::default());
    let p = login(&t);
    let tok = Some(p.access_token.as_str());
    let (s, v, text) = call(&t, req(Method::Get, "/admin/v1/keys", "", tok));
    assert_eq!(s, 200);
    let keys: KeysResponse = serde_json::from_value(v).unwrap();
    let yt = keys.keys.iter().find(|k| k.name == "youtube_api_key").unwrap();
    assert!(yt.set);
    assert_eq!(yt.masked.as_deref(), Some("****_KEY"), "only the last 4 chars");
    // Every admin response, including status/limits/test, must be free of secrets.
    let mut all = text;
    for (m, path, body) in [
        (Method::Get, "/admin/v1/status", ""),
        (Method::Get, "/admin/v1/limits", ""),
        (Method::Post, "/admin/v1/keys/test", r#"{"provider":"youtube"}"#),
        (Method::Get, "/admin/v1/cache/stats", ""),
    ] {
        all.push_str(&call(&t, req(m, path, body, tok)).2);
    }
    for secret in ["YT_SECRET_KEY", "JAM_SECRET", "SC_SECRET", "SP_SECRET", "LF_SECRET", "SC_ID"] {
        assert!(!all.contains(secret), "{secret} leaked");
    }
}

#[test]
fn set_keys_persists_applies_and_validates() {
    let t = setup(true, TestHost::default());
    let p = login(&t);
    let tok = Some(p.access_token.as_str());
    let long = "AIzaSyD-0123456789abcdefWXYZ";
    let (s, v, _) = call(&t, req(Method::Put, "/admin/v1/keys", &format!(r#"{{"values":{{"youtube_api_key":"{long}"}}}}"#), tok));
    assert_eq!(s, 200, "{v}");
    let r: SetKeysResponse = serde_json::from_value(v).unwrap();
    assert_eq!(r.updated, vec!["youtube_api_key"]);
    assert_eq!(r.warnings.len(), 1);
    // applied to the running config
    assert_eq!(t.server.services().config.secrets.youtube_api_key.as_deref(), Some(long));
    let (_, v, text) = call(&t, req(Method::Get, "/admin/v1/keys", "", tok));
    let keys: KeysResponse = serde_json::from_value(v).unwrap();
    assert_eq!(keys.keys[0].masked.as_deref(), Some("****WXYZ"));
    assert!(!text.contains(long));
    // removing a key
    let (s, _, _) = call(&t, req(Method::Put, "/admin/v1/keys", r#"{"values":{"youtube_api_key":""}}"#, tok));
    assert_eq!(s, 200);
    assert!(t.server.services().config.secrets.youtube_api_key.is_none());
    assert!(!t.server.services().config.provider_enabled(ProviderId::YouTube));
    // validation
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/keys", r#"{"values":{"nope":"x"}}"#, tok)).0, 400);
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/keys", r#"{"values":{"lastfm_api_key":"a b"}}"#, tok)).0, 400);
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/keys", r#"{"values":{}}"#, tok)).0, 400);
    assert_eq!(t.server.host.platform(), "native");
}

#[test]
fn unsupported_host_returns_501_and_changes_nothing() {
    let t = setup(true, TestHost { fail: true, ..Default::default() });
    let p = login(&t);
    let (s, v, _) = call(
        &t,
        req(Method::Put, "/admin/v1/keys", r#"{"values":{"lastfm_api_key":"abcdefghijklmnop"}}"#, Some(&p.access_token)),
    );
    assert_eq!((s, v["code"].as_str()), (501, Some("not_supported")));
    assert_eq!(t.server.services().config.secrets.lastfm_api_key.as_deref(), Some("LF_SECRET"));
    // default host methods
    let t = {
        let mut h = harness(all_secrets());
        Arc::get_mut(&mut h.svc).unwrap().config.admin_password_hash = Some(cheap_hash(PW));
        let server = Server::from_arc(h.svc.clone());
        T { h, server }
    };
    let p = login(&t);
    for path in ["/admin/v1/logs"] {
        assert_eq!(call(&t, req(Method::Get, path, "", Some(&p.access_token))).0, 501);
    }
    assert_eq!(call(&t, req(Method::Post, "/admin/v1/update/check", "", Some(&p.access_token))).0, 501);
}

#[test]
fn key_test_with_candidate_values_does_not_save() {
    let t = setup(true, TestHost::default());
    t.h.http.on("googleapis.com/youtube/v3/videos", 400, "{}");
    let p = login(&t);
    let (s, v, _) = call(
        &t,
        req(
            Method::Post,
            "/admin/v1/keys/test",
            r#"{"provider":"youtube","values":{"youtube_api_key":"candidate-key-123"}}"#,
            Some(&p.access_token),
        ),
    );
    assert_eq!(s, 200);
    let r: TestResult = serde_json::from_value(v).unwrap();
    assert!(!r.ok);
    assert!(!r.message.contains("candidate-key-123"));
    assert_eq!(t.server.services().config.secrets.youtube_api_key.as_deref(), Some("YT_SECRET_KEY"));
    assert!(t.h.http.calls.lock().unwrap().iter().any(|u| u.contains("candidate-key-123")));
}

#[test]
fn limits_get_set_and_validate() {
    let t = setup(true, TestHost::default());
    let p = login(&t);
    let tok = Some(p.access_token.as_str());
    let (s, v, _) = call(&t, req(Method::Get, "/admin/v1/limits", "", tok));
    assert_eq!(s, 200);
    assert_eq!(v["rate_limit_per_min"], 1000);
    let ok = r#"{"rate_limit_per_min":30,"youtube_daily_budget_units":5000,"import_max_pages":3}"#;
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/limits", ok, tok)).0, 200);
    assert_eq!(t.server.services().config.rate_limit_per_min, 30);
    let bad = r#"{"rate_limit_per_min":0,"youtube_daily_budget_units":5000,"import_max_pages":3}"#;
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/limits", bad, tok)).0, 400);
    assert_eq!(t.server.services().config.rate_limit_per_min, 30);
    // Old clients omit the companion switch: it stays on.
    assert!(t.server.services().config.companion_enabled);
    let off = r#"{"rate_limit_per_min":30,"youtube_daily_budget_units":5000,"import_max_pages":3,"companion_enabled":false}"#;
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/limits", off, tok)).0, 200);
    assert!(!t.server.services().config.companion_enabled);
    let (_, v, _) = call(&t, req(Method::Get, "/admin/v1/limits", "", tok));
    assert_eq!(v["companion_enabled"], false);
    let separate = r#"{"rate_limit_per_min":30,"youtube_daily_budget_units":5000,"import_max_pages":3,"audio_cache_max_gb":20,"d1_max_mb":250}"#;
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/limits", separate, tok)).0, 200);
    assert_eq!(t.server.services().config.audio_cache_max_gb, 20);
    assert_eq!(t.server.services().config.d1_max_mb, 250);
    let invalid = separate.replace("\"d1_max_mb\":250", "\"d1_max_mb\":0");
    assert_eq!(call(&t, req(Method::Put, "/admin/v1/limits", &invalid, tok)).0, 400);
    assert_eq!(t.server.services().config.d1_max_mb, 250);
}

#[test]
fn account_gate_requires_a_backend_and_old_admin_clients_preserve_it() {
    let mut t = setup(true,TestHost::default());
    let pair = login(&t);
    let token = Some(pair.access_token.as_str());
    let required = r#"{"require_account":true,"rate_limit_per_min":30,"youtube_daily_budget_units":5000,"import_max_pages":3}"#;
    assert_eq!(call(&t,req(Method::Put,"/admin/v1/limits",required,token)).0,400);
    struct Entropy;
    impl crate::accounts::Entropy for Entropy { fn fill(&self, bytes: &mut [u8]) { bytes.fill(1); } }
    t.server = t.server.with_accounts(crate::accounts::Accounts {
        store:Arc::new(crate::accounts::store::MemStore::default()),blobs:None,entropy:Arc::new(Entropy),
        origin:"https://glyph.example".into(),extra_origins:vec![],rp_id:None,
    });
    assert_eq!(call(&t,req(Method::Put,"/admin/v1/limits",required,token)).0,200);
    assert!(t.server.services().config.require_account);
    let legacy = r#"{"rate_limit_per_min":40,"youtube_daily_budget_units":5000,"import_max_pages":3}"#;
    assert_eq!(call(&t,req(Method::Put,"/admin/v1/limits",legacy,token)).0,200);
    assert!(t.server.services().config.require_account,"old clients must not reopen the API");
    let (_,value,_) = call(&t,req(Method::Get,"/admin/v1/limits","",token));
    assert_eq!(value["require_account"],true);
    let disabled = required.replace("true","false");
    assert_eq!(call(&t,req(Method::Put,"/admin/v1/limits",&disabled,token)).0,200);
    assert!(!t.server.services().config.require_account);
}

#[test]
fn status_logs_and_routing() {
    let t = setup(true, TestHost::default());
    let p = login(&t);
    let tok = Some(p.access_token.as_str());
    let (_, v, _) = call(&t, req(Method::Get, "/admin/v1/status", "", tok));
    let st: StatusResponse = serde_json::from_value(v).unwrap();
    assert_eq!(st.platform, "native");
    // YouTube has a key (for playlist import) but search needs yt-dlp, so it is off.
    assert!(st.providers.iter().any(|p| p.id == ProviderId::YouTube && p.needs_key && !p.enabled));
    assert!(st.providers.iter().any(|p| p.id == ProviderId::Jamendo && p.enabled));
    assert!(st.providers.iter().any(|p| p.id == ProviderId::Audius && !p.needs_key));
    let (s, v, _) = call(&t, req(Method::Get, "/admin/v1/logs?n=5&after=41", "", tok));
    assert_eq!((s, v["next"].as_u64()), (200, Some(42)));
    assert_eq!(call(&t, req(Method::Get, "/admin/v1/nothing", "", tok)).0, 404);
    assert_eq!(call(&t, req(Method::Post, "/admin/v1/status", "", tok)).0, 405);
    // cache without stats support
    let (_, v, _) = call(&t, req(Method::Get, "/admin/v1/cache/stats", "", tok));
    assert_eq!(v["supported"], false);
    let _ = MemoryCache::new(t.h.rt.clone(), 1);
}
