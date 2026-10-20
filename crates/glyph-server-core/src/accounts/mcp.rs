//! MCP relay: AI assistants control a user's signed-in Glyph app through this
//! server (the app has no address of its own on the internet).
//!
//! * `POST /mcp`: the MCP endpoint (Streamable HTTP, JSON answers). A tool
//!   call is queued in the user's mailbox; the app fetches it with
//!   `GET /v1/mcp/pull` (long poll), runs it and answers with
//!   `POST /v1/mcp/result`, which this request waits for.
//! * Sign-in follows the MCP authorization spec: OAuth 2.1 with PKCE and
//!   Dynamic Client Registration. The assistant opens `/oauth/authorize`;
//!   the user enters their @handle there and confirms the request in the app
//!   (the page and the app show the same code). A token can also be made in
//!   the app (`POST /v1/mcp/token`) for clients without OAuth.
//!
//! The server owner switches all of it off with `remote mcp off`
//! (`Config::mcp_relay`).

use super::*;

const CODE_TTL: u64 = 5 * MIN;
const AUTH_TTL: u64 = 10 * MIN;
const ACCESS_TTL_MCP: u64 = 30 * DAY;
const REFRESH_TTL_MCP: u64 = 365 * DAY;
const CLIENT_TTL: u64 = 365 * DAY;
/// Calls nobody picked up are dropped after this.
const CALL_TTL: u64 = 2 * MIN;
const POLL_MS: u64 = 400;

const CLIENTS: &str = "_mcp_client";
const AUTHS: &str = "_mcp_auth";
const CODES: &str = "_mcp_code";
const TOKENS: &str = "_mcp_token";
const REFRESH: &str = "_mcp_refresh";

/// Paths this module serves.
pub fn owns(path: &str) -> bool {
    path == "/mcp"
        || path.starts_with("/v1/mcp/")
        || path.starts_with("/oauth/")
        || path.starts_with("/.well-known/oauth-protected-resource")
        || path == "/.well-known/oauth-authorization-server"
}

#[derive(Serialize, Deserialize)]
struct Client {
    name: String,
    redirect_uris: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Auth {
    client_id: String,
    client_name: String,
    redirect_uri: String,
    code_challenge: String,
    state: Option<String>,
    /// Set once a handle was entered.
    user_id: Option<String>,
    code: String,
    /// "pending" | "allowed" | "denied"
    status: String,
}

#[derive(Serialize, Deserialize)]
struct Grant {
    user_id: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
}

fn form(body: &[u8]) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn oauth_err(status: u16, code: &str, desc: &str) -> Response {
    let mut r = Response::json(status, &serde_json::json!({ "error": code, "error_description": desc }));
    r.headers.push(("access-control-allow-origin".into(), "*".into()));
    r
}

fn cors(mut r: Response) -> Response {
    r.headers.push(("access-control-allow-origin".into(), "*".into()));
    r.headers.push(("access-control-allow-headers".into(), "authorization, content-type, mcp-protocol-version".into()));
    r.headers.push(("access-control-allow-methods".into(), "GET, POST, OPTIONS".into()));
    r
}

/// A redirect URI an assistant may register: HTTPS, or loopback HTTP
/// (desktop clients), or a private app scheme; never with a fragment.
fn redirect_ok(u: &str) -> bool {
    if u.len() > 2000 || u.contains('#') {
        return false;
    }
    if let Some(rest) = u.strip_prefix("https://") {
        return !rest.is_empty();
    }
    if let Some(rest) = u.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        return matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    }
    // Custom schemes like `claude://` or `cursor://`.
    u.split_once("://").is_some_and(|(s, _)| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) && !matches!(s, "javascript" | "data" | "file"))
}

impl Accounts {
    fn random_hex(&self, n: usize) -> String {
        let mut b = vec![0u8; n];
        self.entropy.fill(&mut b);
        hex(&b)
    }

    /// `ABCD-1234` style confirmation code shown on the page and in the app.
    fn confirm_code(&self) -> String {
        let mut b = [0u8; 8];
        self.entropy.fill(&mut b);
        let c: String = b.iter().map(|x| USER_CODE_ALPHABET[*x as usize % USER_CODE_ALPHABET.len()] as char).collect();
        format!("{}-{}", &c[..4], &c[4..])
    }

    fn mcp_off(svc: &Services) -> Option<ApiErr> {
        (!svc.config.mcp_relay).then(|| ApiErr::new(404, "mcp_disabled", "the owner of this server turned AI assistant access (MCP) off"))
    }

    /// The user an MCP bearer token belongs to.
    async fn mcp_user(&self, svc: &Services, req: &Request) -> Option<String> {
        let token = req.header("authorization")?.strip_prefix("Bearer ")?.trim();
        if token.len() < 32 || token.len() > 200 {
            return None;
        }
        self.store.mcp_get(TOKENS, &hash(token), svc.rt.now_ms()).await.ok()?
    }

    fn unauth_mcp(&self) -> Response {
        let mut r = Response::json(401, &serde_json::json!({ "error": "invalid_token", "error_description": "sign in with the Glyph server" }));
        r.headers.push((
            "www-authenticate".into(),
            format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"", self.origin.trim_end_matches('/')),
        ));
        cors(r)
    }

    pub(crate) async fn mcp_route(&self, svc: &Arc<Services>, req: &Request, path: &str) -> Result<Response, ApiErr> {
        if let Some(e) = Self::mcp_off(svc) {
            return Err(e);
        }
        let origin = self.origin.trim_end_matches('/').to_string();
        let m = req.method;
        if m == Method::Options {
            return Ok(cors(Response { status: 204, headers: Vec::new(), body: Body::Bytes(Vec::new()) }));
        }
        let now = svc.rt.now_ms();
        let ip = req.client_ip.clone().unwrap_or_else(|| "unknown".into());
        match (path, m) {
            (p, Method::Get) if p.starts_with("/.well-known/oauth-protected-resource") => Ok(cors(Response::json(
                200,
                &serde_json::json!({
                    "resource": format!("{origin}/mcp"),
                    "authorization_servers": [origin],
                    "bearer_methods_supported": ["header"],
                    "resource_name": "Glyph",
                }),
            ))),
            ("/.well-known/oauth-authorization-server", Method::Get) => Ok(cors(Response::json(
                200,
                &serde_json::json!({
                    "issuer": origin,
                    "authorization_endpoint": format!("{origin}/oauth/authorize"),
                    "token_endpoint": format!("{origin}/oauth/token"),
                    "registration_endpoint": format!("{origin}/oauth/register"),
                    "response_types_supported": ["code"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["none"],
                    "scopes_supported": ["glyph"],
                }),
            ))),
            ("/oauth/register", Method::Post) => {
                self.limit(svc, &format!("mcp-register:{ip}"), 10, 3600).await?;
                let v: serde_json::Value = json_body(req, 16 * 1024)?;
                let uris: Vec<String> = v["redirect_uris"].as_array().map(|a| a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect()).unwrap_or_default();
                if uris.is_empty() || uris.len() > 10 || !uris.iter().all(|u| redirect_ok(u)) {
                    return Ok(oauth_err(400, "invalid_redirect_uri", "redirect_uris must be https, loopback http or an app scheme"));
                }
                let name: String = v["client_name"].as_str().unwrap_or("AI assistant").chars().filter(|c| !c.is_control()).take(60).collect();
                let id = format!("mcp_{}", self.random_hex(16));
                let c = Client { name: name.clone(), redirect_uris: uris.clone() };
                self.store.mcp_put(CLIENTS, &id, &serde_json::to_string(&c).unwrap_or_default(), now + CLIENT_TTL).await.map_err(store_err)?;
                Ok(cors(Response::json(
                    201,
                    &serde_json::json!({
                        "client_id": id,
                        "client_id_issued_at": now / 1000,
                        "client_name": name,
                        "redirect_uris": uris,
                        "grant_types": ["authorization_code", "refresh_token"],
                        "response_types": ["code"],
                        "token_endpoint_auth_method": "none",
                    }),
                )))
            }
            ("/oauth/authorize", Method::Get) => {
                let q = |k: &str| req.query.get(k).map(str::to_string);
                let (Some(client_id), Some(redirect_uri), Some(challenge)) = (q("client_id"), q("redirect_uri"), q("code_challenge")) else {
                    return Ok(authorize_page(None, "Missing client_id, redirect_uri or code_challenge."));
                };
                if q("response_type").as_deref() != Some("code") || q("code_challenge_method").as_deref().unwrap_or("S256") != "S256" {
                    return Ok(authorize_page(None, "Only response_type=code with S256 PKCE is supported."));
                }
                let Some(client) = self.store.mcp_get(CLIENTS, &client_id, now).await.map_err(store_err)?.and_then(|c| serde_json::from_str::<Client>(&c).ok()) else {
                    return Ok(authorize_page(None, "Unknown client. The assistant has to register again."));
                };
                if !client.redirect_uris.contains(&redirect_uri) {
                    return Ok(authorize_page(None, "This redirect address was not registered."));
                }
                let id = self.random_hex(16);
                let auth = Auth {
                    client_id,
                    client_name: client.name.clone(),
                    redirect_uri,
                    code_challenge: challenge,
                    state: q("state"),
                    user_id: None,
                    code: self.confirm_code(),
                    status: "pending".into(),
                };
                self.store.mcp_put(AUTHS, &id, &serde_json::to_string(&auth).unwrap_or_default(), now + AUTH_TTL).await.map_err(store_err)?;
                Ok(authorize_page(Some((&id, &client.name, &auth.code)), ""))
            }
            ("/oauth/authorize/start", Method::Post) => {
                self.limit(svc, &format!("mcp-auth:{ip}"), 20, 600).await?;
                let v: serde_json::Value = json_body(req, 4096)?;
                let id = v["id"].as_str().unwrap_or("");
                let Some(mut auth) = self.store.mcp_get(AUTHS, id, now).await.map_err(store_err)?.and_then(|a| serde_json::from_str::<Auth>(&a).ok()) else {
                    return Err(ApiErr::not_found("this sign-in expired; start again in your assistant"));
                };
                let handle = normalize_handle(v["handle"].as_str().unwrap_or("")).map_err(|_| ApiErr::bad_request("not a valid @handle"))?;
                let user = self.store.user_by_handle(&handle).await.map_err(store_err)?;
                // Unknown handles look the same as known ones (no account probing).
                if let Some(u) = user {
                    auth.user_id = Some(u.id.clone());
                    let a = glyph_protocol::mcp::Approval { id: id.to_string(), client: auth.client_name.clone(), code: auth.code.clone() };
                    self.store
                        .mcp_put(&u.id, &format!("approve:{id}"), &serde_json::to_string(&a).unwrap_or_default(), now + AUTH_TTL)
                        .await
                        .map_err(store_err)?;
                }
                self.store.mcp_put(AUTHS, id, &serde_json::to_string(&auth).unwrap_or_default(), now + AUTH_TTL).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "ok": true })))
            }
            ("/oauth/authorize/poll", Method::Get) => {
                let id = req.query.required("id")?;
                let Some(auth) = self.store.mcp_get(AUTHS, id, now).await.map_err(store_err)?.and_then(|a| serde_json::from_str::<Auth>(&a).ok()) else {
                    return Ok(Response::json(200, &serde_json::json!({ "status": "expired" })));
                };
                if auth.status != "allowed" {
                    return Ok(Response::json(200, &serde_json::json!({ "status": auth.status })));
                }
                let Some(user_id) = auth.user_id.clone() else { return Ok(Response::json(200, &serde_json::json!({ "status": "pending" }))) };
                // One code per approval: the authorization is used up here.
                self.store.mcp_delete(AUTHS, id).await.map_err(store_err)?;
                let code = self.random_hex(24);
                let g = Grant { user_id, client_id: auth.client_id, redirect_uri: auth.redirect_uri.clone(), code_challenge: auth.code_challenge };
                self.store.mcp_put(CODES, &hash(&code), &serde_json::to_string(&g).unwrap_or_default(), now + CODE_TTL).await.map_err(store_err)?;
                let mut target = url::Url::parse(&auth.redirect_uri).map_err(|_| ApiErr::bad_request("bad redirect"))?;
                target.query_pairs_mut().append_pair("code", &code);
                if let Some(s) = &auth.state {
                    target.query_pairs_mut().append_pair("state", s);
                }
                target.query_pairs_mut().append_pair("iss", &origin);
                Ok(Response::json(200, &serde_json::json!({ "status": "allowed", "redirect": target.to_string() })))
            }
            ("/oauth/token", Method::Post) => {
                self.limit(svc, &format!("mcp-token:{ip}"), 30, 600).await?;
                let f = form(&req.body);
                let issue = |user_id: String, client_id: String| async move {
                    let (access, refresh) = (self.random_hex(32), self.random_hex(32));
                    self.store.mcp_put(TOKENS, &hash(&access), &user_id, now + ACCESS_TTL_MCP).await.map_err(store_err)?;
                    let r = serde_json::json!({ "user_id": user_id, "client_id": client_id }).to_string();
                    self.store.mcp_put(REFRESH, &hash(&refresh), &r, now + REFRESH_TTL_MCP).await.map_err(store_err)?;
                    Ok::<_, ApiErr>(cors(Response::json(
                        200,
                        &serde_json::json!({ "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL_MCP / 1000, "refresh_token": refresh, "scope": "glyph" }),
                    )))
                };
                match f.get("grant_type").map(String::as_str) {
                    Some("authorization_code") => {
                        let code = f.get("code").cloned().unwrap_or_default();
                        let key = hash(&code);
                        let Some(g) = self.store.mcp_get(CODES, &key, now).await.map_err(store_err)?.and_then(|g| serde_json::from_str::<Grant>(&g).ok()) else {
                            return Ok(oauth_err(400, "invalid_grant", "code expired or unknown"));
                        };
                        self.store.mcp_delete(CODES, &key).await.map_err(store_err)?;
                        let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                        let challenge = b64(&Sha256::digest(verifier.as_bytes()));
                        if f.get("redirect_uri") != Some(&g.redirect_uri) || f.get("client_id") != Some(&g.client_id) || challenge != g.code_challenge {
                            return Ok(oauth_err(400, "invalid_grant", "redirect_uri, client_id or code_verifier does not match"));
                        }
                        issue(g.user_id, g.client_id).await
                    }
                    Some("refresh_token") => {
                        let key = hash(f.get("refresh_token").map(String::as_str).unwrap_or(""));
                        let Some(r) = self.store.mcp_get(REFRESH, &key, now).await.map_err(store_err)?.and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok()) else {
                            return Ok(oauth_err(400, "invalid_grant", "refresh token expired or unknown"));
                        };
                        // Rotated: the old refresh token is spent.
                        self.store.mcp_delete(REFRESH, &key).await.map_err(store_err)?;
                        let user_id = r["user_id"].as_str().unwrap_or("").to_string();
                        if self.store.user(&user_id).await.map_err(store_err)?.is_none() {
                            return Ok(oauth_err(400, "invalid_grant", "account deleted"));
                        }
                        issue(user_id, r["client_id"].as_str().unwrap_or("").to_string()).await
                    }
                    _ => Ok(oauth_err(400, "unsupported_grant_type", "authorization_code or refresh_token")),
                }
            }
            ("/mcp", Method::Post) => {
                let Some(user_id) = self.mcp_user(svc, req).await else { return Ok(self.unauth_mcp()) };
                self.limit(svc, &format!("mcp-call:{user_id}"), 120, 60).await?;
                let msg: serde_json::Value = json_body(req, 256 * 1024)?;
                let batch = msg.is_array();
                let msgs: Vec<serde_json::Value> = match msg {
                    serde_json::Value::Array(a) => a.into_iter().take(20).collect(),
                    m => vec![m],
                };
                let mut answers = Vec::new();
                for m in msgs {
                    if let Some(a) = self.mcp_answer(svc, &user_id, &m).await? {
                        answers.push(a);
                    }
                }
                if answers.is_empty() {
                    return Ok(cors(Response { status: 202, headers: Vec::new(), body: Body::Bytes(Vec::new()) }));
                }
                let out = if batch { serde_json::Value::Array(answers) } else { answers.into_iter().next().unwrap_or_default() };
                Ok(cors(Response::json(200, &out)))
            }
            ("/mcp", _) => Err(method()),
            ("/v1/mcp/token", Method::Post) => {
                let a = self.auth(svc, req).await?;
                let token = self.random_hex(32);
                self.store.mcp_put(TOKENS, &hash(&token), &a.user.id, now + REFRESH_TTL_MCP).await.map_err(store_err)?;
                Ok(Response::json(200, &glyph_protocol::mcp::TokenResponse { token, url: format!("{origin}/mcp") }))
            }
            ("/v1/mcp/pull", Method::Get) => {
                let a = self.auth(svc, req).await?;
                let deadline = now + glyph_protocol::mcp::PULL_WAIT_MS;
                loop {
                    let t = svc.rt.now_ms();
                    let calls: Vec<glyph_protocol::mcp::Call> =
                        self.store.mcp_take(&a.user.id, "call:", t).await.map_err(store_err)?.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect();
                    let approvals: Vec<glyph_protocol::mcp::Approval> =
                        self.store.mcp_take(&a.user.id, "approve:", t).await.map_err(store_err)?.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect();
                    if !calls.is_empty() || !approvals.is_empty() || t >= deadline {
                        return Ok(Response::json(200, &glyph_protocol::mcp::PullResponse { calls, approvals }));
                    }
                    svc.rt.sleep(POLL_MS * 2).await;
                }
            }
            ("/v1/mcp/result", Method::Post) => {
                let a = self.auth(svc, req).await?;
                let r: glyph_protocol::mcp::ResultRequest = json_body(req, 512 * 1024)?;
                if r.id.is_empty() || r.id.len() > 64 {
                    return Err(ApiErr::bad_request("bad call id"));
                }
                self.store.mcp_put(&a.user.id, &format!("result:{}", r.id), &r.result.to_string(), now + CALL_TTL).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({})))
            }
            ("/v1/mcp/approve", Method::Post) => {
                let a = self.auth(svc, req).await?;
                let d: glyph_protocol::mcp::Decision = json_body(req, 4096)?;
                let Some(mut auth) = self.store.mcp_get(AUTHS, &d.id, now).await.map_err(store_err)?.and_then(|x| serde_json::from_str::<Auth>(&x).ok()) else {
                    return Err(ApiErr::not_found("this request expired"));
                };
                if auth.user_id.as_deref() != Some(a.user.id.as_str()) {
                    return Err(ApiErr::not_found("this request expired"));
                }
                auth.status = if d.allow { "allowed" } else { "denied" }.into();
                self.store.mcp_put(AUTHS, &d.id, &serde_json::to_string(&auth).unwrap_or_default(), now + AUTH_TTL).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({})))
            }
            _ => Err(ApiErr::not_found("unknown endpoint")),
        }
    }

    /// One JSON-RPC message from an assistant (`None` for notifications).
    async fn mcp_answer(&self, svc: &Services, user_id: &str, m: &serde_json::Value) -> Result<Option<serde_json::Value>, ApiErr> {
        let Some(id) = m.get("id").cloned() else { return Ok(None) };
        let method = m["method"].as_str().unwrap_or("");
        let ok = |result: serde_json::Value| serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let err = |code: i64, message: String| serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
        if let Some(r) = glyph_protocol::mcp::handle_meta(method, "glyph-server", &svc.config.server_version) {
            return Ok(Some(match r {
                Ok(v) => ok(v),
                Err((c, msg)) => err(c, msg),
            }));
        }
        if method != "tools/call" {
            return Ok(None);
        }
        let name = m["params"]["name"].as_str().unwrap_or("").to_string();
        if !glyph_protocol::mcp::tools().iter().any(|t| t["name"] == name.as_str()) {
            return Ok(Some(err(-32602, format!("unknown tool: {name}"))));
        }
        let call_id = self.random_hex(12);
        let call = glyph_protocol::mcp::Call { id: call_id.clone(), name, arguments: m["params"].get("arguments").cloned().unwrap_or(serde_json::json!({})) };
        let now = svc.rt.now_ms();
        self.store.mcp_put(user_id, &format!("call:{call_id}"), &serde_json::to_string(&call).unwrap_or_default(), now + CALL_TTL).await.map_err(store_err)?;
        let deadline = now + glyph_protocol::mcp::CALL_TIMEOUT_MS;
        let key = format!("result:{call_id}");
        loop {
            svc.rt.sleep(POLL_MS).await;
            let t = svc.rt.now_ms();
            if let Some(r) = self.store.mcp_get(user_id, &key, t).await.map_err(store_err)? {
                let _ = self.store.mcp_delete(user_id, &key).await;
                let v = serde_json::from_str(&r).unwrap_or_else(|_| glyph_protocol::mcp::text_result(&"bad answer from the app".into(), true));
                return Ok(Some(ok(v)));
            }
            if t >= deadline {
                // Not picked up: take it back so it does not run late.
                let _ = self.store.mcp_delete(user_id, &format!("call:{call_id}")).await;
                return Ok(Some(ok(glyph_protocol::mcp::text_result(
                    &"Glyph did not answer. Open the app on a device signed in to this account and turn on Settings › Integrations › AI assistants (MCP) › Through the server.".into(),
                    true,
                ))));
            }
        }
    }
}

/// The sign-in page an assistant opens: the user enters their @handle and
/// confirms in the app; the page then sends the browser back to the assistant.
fn authorize_page(auth: Option<(&str, &str, &str)>, error: &str) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let body = match auth {
        Some((id, client, code)) => format!(
            r#"<h1>Connect {client}</h1>
<p class="sub">{client} wants to control Glyph for you: playback, queue, playlists and likes.</p>
<form id="f"><label for="h">Your Glyph account</label><input id="h" placeholder="@handle" autocomplete="username" autofocus required><div class="row"><button>Continue</button></div></form>
<div id="wait" hidden><p class="sub">Confirm in the Glyph app on a signed-in device. It shows the same code:</p><div class="code">{code}</div><p class="muted">Nothing appears? Turn on Settings › Integrations › AI assistants › Through the server.</p></div>
<p id="msg" class="muted"></p>
<script>
const id={id:?};const msg=document.getElementById('msg');
document.getElementById('f').onsubmit=async e=>{{e.preventDefault();
 const r=await fetch('/oauth/authorize/start',{{method:'POST',headers:{{'content-type':'application/json'}},body:JSON.stringify({{id,handle:document.getElementById('h').value}})}});
 if(!r.ok){{msg.textContent=(await r.json().catch(()=>({{}}))).message||'Something went wrong.';return}}
 document.getElementById('f').hidden=true;document.getElementById('wait').hidden=false;poll()}};
async function poll(){{const r=await fetch('/oauth/authorize/poll?id='+id);const j=await r.json();
 if(j.status==='allowed'){{msg.textContent='Connected. Returning to the assistant…';location.href=j.redirect;return}}
 if(j.status==='denied'){{msg.textContent='Declined in the app.';return}}
 if(j.status==='expired'){{msg.textContent='This sign-in expired. Start again in the assistant.';return}}
 setTimeout(poll,1500)}}
</script>"#,
            client = esc(client),
            code = esc(code),
            id = id,
        ),
        None => format!("<h1>Something went wrong</h1><p class=\"sub\">{}</p>", esc(error)),
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Glyph · Connect an assistant</title>
<style>*{{box-sizing:border-box}}body{{background:#09090b;color:#f2f2f2;font:14px/1.5 Inter,system-ui,-apple-system,"Segoe UI",sans-serif;display:grid;place-items:center;min-height:100vh;margin:0;padding:16px}}main{{width:100%;max-width:460px;padding:24px;border-radius:12px;background:#171717;display:flex;flex-direction:column;gap:18px}}.logo{{font:700 15px "JetBrains Mono",ui-monospace,monospace;color:#ff5fa2}}h1{{margin:0;font-size:20px;font-weight:700;color:rgba(255,255,255,.95)}}p{{margin:0}}.sub{{color:rgba(255,255,255,.7)}}form,#wait{{display:flex;flex-direction:column;gap:8px}}[hidden]{{display:none!important}}label{{font-size:13px;color:rgba(255,255,255,.55)}}input{{width:100%;height:40px;padding:0 12px;border-radius:8px;border:1px solid transparent;background:rgba(255,255,255,.07);color:#fff;font:inherit;font-size:14px;outline:none}}input:focus{{border-color:rgba(255,255,255,.2)}}input::placeholder{{color:rgba(255,255,255,.35)}}.row{{display:flex;justify-content:flex-end;margin-top:10px}}button{{height:40px;padding:0 24px;border-radius:8px;border:0;background:#fff;color:#000;font:inherit;font-weight:600;cursor:pointer}}button:hover{{background:rgba(255,255,255,.88)}}.code{{height:64px;display:flex;align-items:center;justify-content:center;border-radius:8px;background:rgba(255,255,255,.07);font:700 26px "JetBrains Mono",ui-monospace,monospace;letter-spacing:2px;color:#ff5fa2}}.muted{{color:rgba(255,255,255,.45);font-size:13px}}#msg:empty{{display:none}}</style></head><body><main><div class="logo">glyph</div>{body}</main></body></html>"#
    );
    Response {
        status: 200,
        headers: vec![
            ("content-type".into(), "text/html; charset=utf-8".into()),
            ("cache-control".into(), "no-store".into()),
            ("x-frame-options".into(), "DENY".into()),
            ("referrer-policy".into(), "no-referrer".into()),
            ("content-security-policy".into(), "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".into()),
        ],
        body: Body::Bytes(html.into_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::redirect_ok;

    #[test]
    fn redirects() {
        assert!(redirect_ok("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_ok("http://127.0.0.1:33418/callback"));
        assert!(redirect_ok("http://localhost:6274/oauth/callback"));
        assert!(redirect_ok("cursor://anysphere.cursor-retrieval/oauth/callback"));
        assert!(!redirect_ok("http://evil.example/cb"));
        assert!(!redirect_ok("javascript://alert(1)"));
        assert!(!redirect_ok("https://x.example/#frag"));
    }
}
