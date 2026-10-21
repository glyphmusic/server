use super::*;

impl Accounts {
    pub(super) async fn dispatch(&self, svc: &Arc<Services>, req: &Request) -> Result<Response, ApiErr> {
        let path = req.path.trim_end_matches('/');
        if path.starts_with("/v1/devices/playback/") {
            return self.playback_route(svc, req, path).await;
        }
        if mcp::owns(path) {
            return self.mcp_route(svc, req, path).await;
        }
        if jam::owns(path) {
            return self.jam_route(svc, req, path).await;
        }
        let m = req.method;
        if m == Method::Options {
            return Ok(Response { status: 204, headers: vec![("allow".into(), "GET, POST, PUT, PATCH, DELETE, OPTIONS".into())], body: Body::Bytes(vec![]) });
        }
        let now = svc.rt.now_ms();
        let ip = req.client_ip.clone().unwrap_or_else(|| "unknown".into());

        if let Some(id) = path.strip_prefix("/account/login/") {
            if m != Method::Get {
                return Err(method());
            }
            let _ = id;
            return Ok(page());
        }
        if let Some(hash_hex) = path.strip_prefix("/v1/covers/") {
            if m != Method::Get {
                return Err(method());
            }
            return self.cached_cover(svc, req, hash_hex).await;
        }
        if let Some(h) = path.strip_prefix("/v1/account/handle/") {
            if m != Method::Get {
                return Err(method());
            }
            self.limit(svc, &format!("acct-handle:{ip}"), 30, 60).await?;
            let check = match normalize_handle(h) {
                Err(reason) => HandleCheck { handle: h.to_ascii_lowercase(), available: false, reason: Some(reason.into()) },
                Ok(h) => {
                    let free = self.store.handle_free(&h, now).await.map_err(store_err)?;
                    HandleCheck { handle: h, available: free, reason: (!free).then(|| "taken".into()) }
                }
            };
            return Ok(Response::json(200, &check));
        }
        if let Some(h) = path.strip_prefix("/v1/account/password/salt/") {
            if m != Method::Get {
                return Err(method());
            }
            self.limit(svc, &format!("acct-salt:{ip}"), 30, 60).await?;
            let salt = match normalize_handle(h) {
                Ok(h) => self.store.user_by_handle(&h).await.map_err(store_err)?.and_then(|u| u.pw_salt),
                Err(_) => None,
            };
            let salt = salt.ok_or_else(|| ApiErr::new(404, "no_password", "no password set for this handle"))?;
            return Ok(Response::json(200, &PasswordSalt { salt }));
        }
        if path == "/v1/profile" || path.starts_with("/v1/profile/") || path.starts_with("/v1/profiles/") {
            return self.profile_route(svc, req, path).await;
        }
        if let Some(rest) = path.strip_prefix("/v1/devices/") {
            let a = self.auth(svc, req).await?;
            if rest == "self/signout" {
                if m != Method::Post {
                    return Err(method());
                }
                self.store.delete_device(&a.user.id, &a.device.id).await.map_err(store_err)?;
                return Ok(Response::json(200, &serde_json::json!({})));
            }
            return match m {
                Method::Delete => {
                    if !self.store.delete_device(&a.user.id, rest).await.map_err(store_err)? {
                        return Err(ApiErr::not_found("no such device"));
                    }
                    Ok(Response::json(200, &serde_json::json!({})))
                }
                Method::Patch => {
                    let p: DevicePatch = json_body(req, 4096)?;
                    let mut d = self.store.devices(&a.user.id).await.map_err(store_err)?.into_iter().find(|d| d.id == rest).ok_or_else(|| ApiErr::not_found("no such device"))?;
                    d.name = clean_name(&p.name);
                    self.store.put_device(&d).await.map_err(store_err)?;
                    Ok(Response::json(200, &wire_device(&d, &a.device.id)))
                }
                _ => Err(method()),
            };
        }
        if let Some(name) = path.strip_prefix("/v1/sync/covers/") {
            return self.own_cover(svc, req, name).await;
        }
        if path == "/v1/share" || path.starts_with("/v1/share/") || path.starts_with("/p/") || path.starts_with("/embed/p/") || path == "/oembed" {
            return self.share(svc, req, path).await;
        }

        match (path, m) {
            ("/v1/account/login/start", Method::Post) => {
                self.limit(svc, &format!("acct-start:{ip}"), 10, 600).await?;
                let r: LoginStartRequest = json_body(req, 4096)?;
                let login_id = b64(&self.random(16));
                let poll_secret = self.token("gs_");
                let login = Login {
                    id: login_id.clone(),
                    poll_hash: hash(&poll_secret),
                    user_code: self.user_code(),
                    device_name: clean_name(&r.device_name),
                    platform: clean_platform(&r.platform),
                    expires: now + LOGIN_TTL,
                    ..Default::default()
                };
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &LoginStartResponse {
                        url: format!("{}/account/login/{login_id}", self.origin.trim_end_matches('/')),
                        login_id,
                        poll_secret,
                        user_code: login.user_code,
                        expires_in_s: LOGIN_TTL / SEC,
                    },
                ))
            }
            ("/v1/account/login/poll", Method::Post) => {
                let r: LoginPollRequest = json_body(req, 4096)?;
                let Some(login) = self.store.login(&r.login_id).await.map_err(store_err)? else {
                    return Ok(Response::json(200, &LoginPollResponse::Expired));
                };
                if login.poll_hash != hash(&r.poll_secret) {
                    return Err(unauthorized());
                }
                if login.expires <= now {
                    self.store.delete_login(&login.id).await.map_err(store_err)?;
                    return Ok(Response::json(200, &LoginPollResponse::Expired));
                }
                let Some(user_id) = login.user_id.clone() else {
                    return Ok(Response::json(200, &LoginPollResponse::Pending));
                };
                let Some(user) = self.store.user(&user_id).await.map_err(store_err)? else {
                    self.store.delete_login(&login.id).await.map_err(store_err)?;
                    return Ok(Response::json(200, &LoginPollResponse::Expired));
                };
                if self.store.devices(&user.id).await.map_err(store_err)?.len() >= user.device_limit as usize {
                    return Ok(Response::json(200, &LoginPollResponse::DeviceLimit));
                }
                let session = self.new_device(&user, &login.device_name, &login.platform, now).await?;
                self.store.delete_login(&login.id).await.map_err(store_err)?;
                Ok(Response::json(200, &LoginPollResponse::Done { session }))
            }
            ("/v1/account/login/approve", Method::Post) => {
                let a = self.auth(svc, req).await?;
                self.limit(svc, &format!("acct-approve:{}", a.user.id), 10, 60).await?;
                let r: ApproveRequest = json_body(req, 1024)?;
                let code = normalize_user_code(&r.user_code).ok_or_else(|| ApiErr::bad_request("invalid code"))?;
                let mut login = self.store.login_by_code(&code).await.map_err(store_err)?
                    .filter(|l| l.expires > now && l.user_id.is_none())
                    .ok_or_else(|| ApiErr::not_found("unknown or expired code"))?;
                login.user_id = Some(a.user.id.clone());
                login.challenge = None;
                self.store.put_login(&login).await.map_err(store_err)?;
                let n = self.store.devices(&a.user.id).await.map_err(store_err)?.len();
                Ok(Response::json(
                    200,
                    &ApproveResponse { device_name: login.device_name, platform: login.platform, device_limit_reached: n >= a.user.device_limit as usize },
                ))
            }
            ("/v1/account/login/devices", Method::Post) | ("/v1/account/login/remove_device", Method::Post) => {
                let r: PageReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                let (Some(user_id), Some(ph)) = (login.user_id.as_deref(), login.page_hash.as_deref()) else { return Err(unauthorized()) };
                if ph != hash(&r.page_secret) {
                    return Err(unauthorized());
                }
                if let Some(id) = r.device_id.as_deref().filter(|_| path.ends_with("remove_device")) {
                    self.store.delete_device(user_id, id).await.map_err(store_err)?;
                }
                let user = self.store.user(user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                let devices = self.store.devices(user_id).await.map_err(store_err)?;
                Ok(Response::json(200, &DevicesResponse { devices: devices.iter().map(|d| wire_device(d, "")).collect(), device_limit: user.device_limit }))
            }
            ("/v1/account/passkey/register/options", Method::Post) => {
                self.limit(svc, &format!("acct-wa:{ip}"), 20, 600).await?;
                let r: RegisterOptionsReq = json_body(req, 4096)?;
                let mut login = self.pending(svc, &r.login_id).await?;
                let (handle, user_id) = if login.link {
                    (login.pending_handle.clone().unwrap_or_default(), login.pending_user_id.clone().unwrap_or_default())
                } else {
                    let handle = normalize_handle(&r.handle).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                    if !self.store.handle_free(&handle, now).await.map_err(store_err)? {
                        return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                    }
                    (handle, format!("u_{}", hex(&self.random(12))))
                };
                let challenge = self.challenge();
                login.challenge = Some(challenge.clone());
                login.pending_handle = Some(handle.clone());
                login.pending_user_id = Some(user_id.clone());
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "publicKey": {
                    "challenge": challenge,
                    "rp": { "id": self.rp_id(), "name": "Glyph" },
                    "user": { "id": b64(user_id.as_bytes()), "name": format!("@{handle}"), "displayName": format!("@{handle}") },
                    "pubKeyCredParams": [{ "type": "public-key", "alg": webauthn::ES256 }, { "type": "public-key", "alg": webauthn::RS256 }],
                    "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "required" },
                    "attestation": "none",
                    "timeout": 120000
                }})))
            }
            ("/v1/account/passkey/register/verify", Method::Post) => {
                let r: RegisterVerifyReq = json_body(req, 64 * 1024)?;
                let login = self.pending(svc, &r.login_id).await?;
                let (Some(ch), Some(handle), Some(user_id)) = (login.challenge.clone(), login.pending_handle.clone(), login.pending_user_id.clone()) else {
                    return Err(ApiErr::bad_request("request options first"));
                };
                let cd = unb64(&r.client_data_json).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let att = unb64(&r.attestation_object).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let origins = self.origins();
                let cred = webauthn::verify_registration(&Expect { rp_id: self.rp_id(), origins: &origins, challenge: &ch }, &cd, &att)
                    .map_err(|e| ApiErr::new(400, "passkey_invalid", e.to_string()))?;
                let pk = Passkey { cred_id: cred.cred_id, user_id: user_id.clone(), alg: cred.alg, public_key: cred.public_key, sign_count: cred.sign_count, created_at: now };
                if self.store.passkey(&pk.cred_id).await.map_err(store_err)?.is_some() {
                    return Err(ApiErr::new(409, "passkey_exists", "this passkey is already registered"));
                }
                if login.link {
                    // Adding a passkey to a signed-in account: no new device, no new session.
                    self.store.user(&user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                    self.store.add_passkey(&pk).await.map_err(store_err)?;
                    self.store.delete_login(&login.id).await.map_err(store_err)?;
                    return Ok(Response::json(200, &serde_json::json!({ "handle": handle, "linked": true })));
                }
                let recovery = self.recovery_code();
                let user = self.blank_user(user_id, handle, &recovery, now).await?;
                if !self.store.create_user(&user, Some(&pk), now).await.map_err(store_err)? {
                    return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                }
                self.approve_on_page(login, &user, Some(recovery)).await
            }
            ("/v1/account/passkey/login/options", Method::Post) => {
                self.limit(svc, &format!("acct-wa:{ip}"), 20, 600).await?;
                let r: LoginIdReq = json_body(req, 4096)?;
                let mut login = self.pending(svc, &r.login_id).await?;
                let challenge = self.challenge();
                login.challenge = Some(challenge.clone());
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "publicKey": {
                    "challenge": challenge,
                    "rpId": self.rp_id(),
                    "allowCredentials": [],
                    "userVerification": "required",
                    "timeout": 120000
                }})))
            }
            ("/v1/account/passkey/login/verify", Method::Post) => {
                let r: AssertVerifyReq = json_body(req, 64 * 1024)?;
                let mut login = self.pending(svc, &r.login_id).await?;
                let Some(ch) = login.challenge.take() else { return Err(ApiErr::bad_request("request options first")) };
                // One attempt per challenge.
                self.store.put_login(&login).await.map_err(store_err)?;
                let bad = || ApiErr::new(400, "passkey_invalid", "unknown passkey");
                let pk = self.store.passkey(r.id.trim_end_matches('=')).await.map_err(store_err)?.ok_or_else(bad)?;
                if let Some(uh) = r.user_handle.as_deref().filter(|s| !s.is_empty()) {
                    if unb64(uh).as_deref() != Some(pk.user_id.as_bytes()) {
                        return Err(bad());
                    }
                }
                let cd = unb64(&r.client_data_json).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let ad = unb64(&r.authenticator_data).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let sig = unb64(&r.signature).ok_or_else(|| ApiErr::bad_request("bad base64"))?;
                let origins = self.origins();
                let count = webauthn::verify_assertion(&Expect { rp_id: self.rp_id(), origins: &origins, challenge: &ch }, pk.alg, &pk.public_key, pk.sign_count, &cd, &ad, &sig)
                    .map_err(|e| ApiErr::new(400, "passkey_invalid", e.to_string()))?;
                self.store.set_sign_count(&pk.cred_id, count).await.map_err(store_err)?;
                let user = self.store.user(&pk.user_id).await.map_err(store_err)?.ok_or_else(bad)?;
                self.approve_on_page(login, &user, None).await
            }
            ("/v1/account/recover", Method::Post) => {
                self.limit(svc, &format!("acct-recover:{ip}"), 5, 600).await?;
                let r: RecoverReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                let handle = normalize_handle(&r.handle).map_err(|_| ApiErr::new(401, "recovery_failed", "handle or code wrong"))?;
                self.limit(svc, &format!("acct-recover-h:{handle}"), 5, 3600).await?;
                let user = self.store.user_by_handle(&handle).await.map_err(store_err)?
                    .filter(|u| u.recovery_hash == hash(r.recovery_code.trim()))
                    .ok_or_else(|| ApiErr::new(401, "recovery_failed", "handle or code wrong"))?;
                // A used code is replaced right away.
                let fresh = self.recovery_code();
                self.store.set_recovery(&user.id, &hash(&fresh)).await.map_err(store_err)?;
                self.approve_on_page(login, &user, Some(fresh)).await
            }
            ("/v1/account/login/info", Method::Post) => {
                let r: LoginIdReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                Ok(Response::json(200, &serde_json::json!({
                    "link": login.link,
                    "handle": if login.link { login.pending_handle.clone() } else { None },
                    "user_code": if login.link { None } else { Some(login.user_code.clone()) },
                })))
            }
            ("/v1/account/passkey/link", Method::Post) => {
                let a = self.auth(svc, req).await?;
                self.limit(svc, &format!("acct-link:{}", a.user.id), 10, 600).await?;
                let login_id = b64(&self.random(16));
                let login = Login {
                    id: login_id.clone(),
                    // Nobody polls a link login: an unguessable, unused poll hash.
                    poll_hash: hash(&self.token("gs_")),
                    user_code: self.user_code(),
                    device_name: a.device.name.clone(),
                    platform: a.device.platform.clone(),
                    pending_handle: Some(a.user.handle.clone()),
                    pending_user_id: Some(a.user.id.clone()),
                    link: true,
                    expires: now + LOGIN_TTL,
                    ..Default::default()
                };
                self.store.put_login(&login).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &LinkStartResponse { url: format!("{}/account/login/{login_id}", self.origin.trim_end_matches('/')), expires_in_s: LOGIN_TTL / SEC },
                ))
            }
            ("/v1/account/login/password", Method::Post) | ("/v1/account/login/password_register", Method::Post) => {
                let r: PagePasswordReq = json_body(req, 4096)?;
                let login = self.pending(svc, &r.login_id).await?;
                if login.link || login.user_id.is_some() {
                    return Err(ApiErr::bad_request("this sign-in is already finished"));
                }
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                if path.ends_with("password_register") {
                    self.limit(svc, &format!("acct-signup:{ip}"), 5, 3600).await?;
                    let handle = normalize_handle(&r.handle).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                    let salt = decode_exact(r.salt.as_deref().unwrap_or_default(), glyph_protocol::password_kdf::SALT_LEN, "salt")?;
                    let recovery = self.recovery_code();
                    let mut user = self.blank_user(format!("u_{}", hex(&self.random(12))), handle, &recovery, now).await?;
                    user.pw_salt = Some(b64(&salt));
                    user.pw_hash = Some(key_hash(&key));
                    if !self.store.create_user(&user, None, now).await.map_err(store_err)? {
                        return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                    }
                    return self.approve_on_page(login, &user, Some(recovery)).await;
                }
                self.limit(svc, &format!("acct-pw:{ip}"), 20, 600).await?;
                let failed = || ApiErr::new(401, "login_failed", "handle or password wrong");
                let handle = normalize_handle(&r.handle).map_err(|_| failed())?;
                self.limit(svc, &format!("acct-pw-h:{handle}"), 10, 3600).await?;
                let user = self.store.user_by_handle(&handle).await.map_err(store_err)?
                    .filter(|u| u.pw_hash.as_deref() == Some(key_hash(&key).as_str()))
                    .ok_or_else(failed)?;
                self.approve_on_page(login, &user, None).await
            }
            ("/v1/account/password/register", Method::Post) => {
                self.limit(svc, &format!("acct-signup:{ip}"), 5, 3600).await?;
                let r: PasswordRegisterRequest = json_body(req, 4096)?;
                let handle = normalize_handle(&r.handle).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                let salt = decode_exact(&r.salt, glyph_protocol::password_kdf::SALT_LEN, "salt")?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                let recovery = self.recovery_code();
                let mut user = self.blank_user(format!("u_{}", hex(&self.random(12))), handle, &recovery, now).await?;
                user.pw_salt = Some(b64(&salt));
                user.pw_hash = Some(key_hash(&key));
                if !self.store.create_user(&user, None, now).await.map_err(store_err)? {
                    return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                }
                let session = self.new_device(&user, &r.device_name, &r.platform, now).await?;
                Ok(Response::json(200, &SignupResponse { session, recovery_code: recovery }))
            }
            ("/v1/account/password/login", Method::Post) => {
                self.limit(svc, &format!("acct-pw:{ip}"), 20, 600).await?;
                let r: PasswordLoginRequest = json_body(req, 4096)?;
                let failed = || ApiErr::new(401, "login_failed", "handle or password wrong");
                let handle = normalize_handle(&r.handle).map_err(|_| failed())?;
                self.limit(svc, &format!("acct-pw-h:{handle}"), 10, 3600).await?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                let user = self.store.user_by_handle(&handle).await.map_err(store_err)?
                    .filter(|u| u.pw_hash.as_deref() == Some(key_hash(&key).as_str()))
                    .ok_or_else(failed)?;
                if self.store.devices(&user.id).await.map_err(store_err)?.len() >= user.device_limit as usize {
                    return Err(ApiErr::new(409, "device_limit", "remove a device first"));
                }
                let session = self.new_device(&user, &r.device_name, &r.platform, now).await?;
                Ok(Response::json(200, &session))
            }
            // A device signed in without the password (passkey, browser) checks
            // it before deriving the key for the encrypted settings with it.
            ("/v1/account/password/check", Method::Post) => {
                let a = self.auth(svc, req).await?;
                self.limit(svc, &format!("acct-pw-h:{}", a.user.handle), 10, 3600).await?;
                let r: glyph_protocol::PasswordCheckRequest = json_body(req, 4096)?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                if a.user.pw_hash.as_deref() != Some(key_hash(&key).as_str()) {
                    // 403, not 401: the session itself is fine.
                    return Err(ApiErr::new(403, "wrong_password", "the password is wrong"));
                }
                Ok(Response::json(200, &serde_json::json!({ "ok": true })))
            }
            ("/v1/account/password", Method::Put) => {
                let a = self.auth(svc, req).await?;
                let r: PasswordSetRequest = json_body(req, 4096)?;
                let salt = decode_exact(&r.salt, glyph_protocol::password_kdf::SALT_LEN, "salt")?;
                let key = decode_exact(&r.key, glyph_protocol::password_kdf::KEY_LEN, "key")?;
                self.store.set_password(&a.user.id, &b64(&salt), &key_hash(&key)).await.map_err(store_err)?;
                let user = self.store.user(&a.user.id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                Ok(Response::json(200, &account_of(&user)))
            }
            ("/v1/account/token/refresh", Method::Post) => {
                self.limit(svc, &format!("acct-refresh:{ip}"), 30, 60).await?;
                let r: RefreshRequest = json_body(req, 4096)?;
                let mut d = self.store.device_by_refresh(&hash(r.refresh_token.trim())).await.map_err(store_err)?
                    .filter(|d| d.refresh_exp > now)
                    .ok_or_else(unauthorized)?;
                let user = self.store.user(&d.user_id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                let access = self.token("ga_");
                let refresh = self.token("gr_");
                d.access_hash = hash(&access);
                d.access_exp = now + ACCESS_TTL;
                d.refresh_hash = hash(&refresh);
                d.refresh_exp = now + REFRESH_TTL;
                d.last_seen = now;
                if req.approximate_location.is_some() { d.approximate_location = req.approximate_location.clone(); }
                self.store.put_device(&d).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &Session { access_token: access, refresh_token: refresh, access_expires_in_s: ACCESS_TTL / SEC, device_id: d.id, account: account_of(&user) },
                ))
            }
            ("/v1/account/recovery_code", Method::Post) => {
                let a = self.auth(svc, req).await?;
                let fresh = self.recovery_code();
                self.store.set_recovery(&a.user.id, &hash(&fresh)).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "recovery_code": fresh })))
            }
            ("/v1/account", Method::Get) => {
                let a = self.auth(svc, req).await?;
                Ok(Response::json(200, &account_of(&a.user)))
            }
            ("/v1/account", Method::Patch) => {
                let a = self.auth(svc, req).await?;
                let p: AccountPatch = json_body(req, 4096)?;
                // The client is open source and untrusted: limits are server settings.
                if p.device_limit.is_some() {
                    return Err(ApiErr::new(403, "server_setting", "the device limit is set by the server operator"));
                }
                if let Some(h) = p.handle.as_deref() {
                    let h = normalize_handle(h).map_err(|why| ApiErr::new(400, &format!("handle_{why}"), "this handle cannot be used"))?;
                    if h != a.user.handle {
                        if a.user.handle_changed_at != 0 && now < a.user.handle_changed_at + HANDLE_COOLDOWN {
                            return Err(ApiErr::new(409, "handle_cooldown", "the handle can change once every 30 days"));
                        }
                        if !self.store.change_handle(&a.user.id, &h, now, now + HANDLE_HOLD).await.map_err(store_err)? {
                            return Err(ApiErr::new(409, "handle_taken", "this handle is taken"));
                        }
                    }
                }
                let user = self.store.user(&a.user.id).await.map_err(store_err)?.ok_or_else(unauthorized)?;
                Ok(Response::json(200, &account_of(&user)))
            }
            ("/v1/account", Method::Delete) => {
                let a = self.auth(svc, req).await?;
                let r: DeleteAccountRequest = json_body(req, 1024)?;
                if r.confirm_handle.trim().trim_start_matches('@').to_ascii_lowercase() != a.user.handle {
                    return Err(ApiErr::bad_request("confirm_handle does not match"));
                }
                if let Some(b) = &self.blobs {
                    b.delete_prefix(&format!("users/{}/", a.user.id)).await.map_err(store_err)?;
                }
                self.store.delete_user(&a.user.id, now + HANDLE_HOLD).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({})))
            }
            ("/v1/devices", Method::Get) => {
                let a = self.auth(svc, req).await?;
                let devices = self.store.devices(&a.user.id).await.map_err(store_err)?;
                Ok(Response::json(
                    200,
                    &DevicesResponse { devices: devices.iter().map(|d| wire_device(d, &a.device.id)).collect(), device_limit: a.user.device_limit },
                ))
            }
            ("/v1/sync/push", Method::Post) => {
                let a = self.auth(svc, req).await?;
                let r: SyncPushRequest = json_body(req, MAX_PUSH_BYTES)?;
                if r.items.len() > MAX_PUSH_ITEMS {
                    return Err(ApiErr::new(413, "payload_too_large", "at most 500 items per push"));
                }
                let mut items = Vec::with_capacity(r.items.len());
                for i in r.items {
                    let kind_ok = (1..=24).contains(&i.kind.len()) && i.kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
                    if !kind_ok || i.key.is_empty() || i.key.len() > 512 || i.deleted == i.value.is_some() {
                        return Err(ApiErr::bad_request("invalid sync item"));
                    }
                    let value = match i.value {
                        Some(v) => {
                            if i.kind == "playlist" {
                                let tracks = v.get("tracks").and_then(serde_json::Value::as_array)
                                    .ok_or_else(|| ApiErr::bad_request("playlist tracks must be an array"))?;
                                if tracks.len() > svc.config.playlist_max_tracks {
                                    return Err(ApiErr::bad_request(format!("playlists may contain at most {} songs", svc.config.playlist_max_tracks)));
                                }
                            }
                            let s = v.to_string();
                            if s.len() > MAX_ITEM_BYTES {
                                return Err(ApiErr::new(413, "payload_too_large", "sync item too large"));
                            }
                            Some(s)
                        }
                        None => None,
                    };
                    items.push(Item {
                        kind: i.kind,
                        key: i.key,
                        value,
                        updated_at: i.updated_at.min(now + CLOCK_SKEW),
                        device_id: a.device.id.clone(),
                        deleted: i.deleted,
                        rev: i.rev,
                    });
                }
                self.store.purge_tombstones(&a.user.id, now.saturating_sub(TOMBSTONE_TTL)).await.map_err(store_err)?;
                let (accepted, rev) = self.store.push_items(&a.user.id, &items).await.map_err(store_err)?;
                let lost: Vec<(String, String)> = items.iter().zip(&accepted).filter(|(_, ok)| !**ok).map(|(i, _)| (i.kind.clone(), i.key.clone())).collect();
                let stale = if lost.is_empty() { vec![] } else { self.store.items(&a.user.id, &lost).await.map_err(store_err)? };
                Ok(Response::json(
                    200,
                    &SyncPushResponse { rev, accepted: accepted.iter().filter(|a| **a).count() as u32, stale: stale.into_iter().map(to_wire).collect() },
                ))
            }
            ("/v1/sync/pull", Method::Get) => {
                let a = self.auth(svc, req).await?;
                let since: u64 = match req.query.get("since") {
                    Some(s) => s.parse().map_err(|_| ApiErr::bad_request("since must be a number"))?,
                    None => 0,
                };
                let limit: u32 = match req.query.get("limit") {
                    Some(s) => s.parse().map_err(|_| ApiErr::bad_request("limit must be a number"))?,
                    None => 500,
                }
                .clamp(1, 1000);
                if since > 0 && since < a.user.purged_rev {
                    return Ok(Response::json(200, &SyncPullResponse { reset: true, ..Default::default() }));
                }
                let mut items = self.store.items_since(&a.user.id, since, limit + 1).await.map_err(store_err)?;
                let more = items.len() > limit as usize;
                items.truncate(limit as usize);
                let last = items.last().map_or(since, |i| i.rev);
                let rev = if more { last } else { a.user.rev.max(last) };
                Ok(Response::json(200, &SyncPullResponse { items: items.into_iter().map(to_wire).collect(), rev, more, reset: false }))
            }
            ("/v1/sync/export", Method::Get) => {
                let a = self.auth(svc, req).await?;
                let items = self.store.items_since(&a.user.id, 0, 1_000_000).await.map_err(store_err)?;
                let live: Vec<SyncItem> = items.into_iter().filter(|i| !i.deleted).map(to_wire).collect();
                let mut r = Response::json(200, &serde_json::json!({ "account": account_of(&a.user), "items": live }));
                r.headers.push(("content-disposition".into(), "attachment; filename=\"glyph-export.json\"".into()));
                Ok(r)
            }
            (p, _) if matches!(p, "/v1/account" | "/v1/devices" | "/v1/sync/push" | "/v1/sync/pull" | "/v1/sync/export" | "/v1/account/recovery_code")
                || p.starts_with("/v1/account/login/") || p.starts_with("/v1/account/passkey/") || p.starts_with("/v1/account/password")
                || p == "/v1/account/recover" || p == "/v1/account/token/refresh" =>
            {
                Err(method())
            }
            _ => Err(ApiErr::not_found("unknown endpoint")),
        }
    }

    /// `GET /v1/covers/{sha256(url)}?u={url}`: fetch once, keep in R2.
    pub(super) async fn cached_cover(&self, svc: &Arc<Services>, req: &Request, name: &str) -> Result<Response, ApiErr> {
        let Some(blobs) = &self.blobs else { return Err(ApiErr::not_found("cover cache disabled")) };
        let url = req.query.required("u")?;
        if !is_sha256_hex(name) || hash(url) != name || !glyph_protocol::cover_cache_allowed(url) {
            return Err(ApiErr::bad_request("cover url not allowed"));
        }
        const CACHE: &str = "public, max-age=31536000, immutable";
        let key = format!("covers/{name}");
        if let Some((bytes, ct)) = blobs.get(&key).await.map_err(store_err)? {
            return Ok(image(200, bytes, &ct, CACHE));
        }
        let fetched = crate::traits::with_timeout(&*svc.rt, svc.config.provider_timeout_ms.max(5000), svc.http.send(crate::HttpRequest::get(url))).await;
        let Some(Ok(resp)) = fetched else { return Err(ApiErr::new(502, "upstream_error", "cover fetch failed")) };
        if resp.status != 200 || resp.body.len() > MAX_CACHED_COVER {
            return Err(ApiErr::new(502, "upstream_error", "cover fetch failed"));
        }
        let ct = sniff_image(&resp.body).ok_or_else(|| ApiErr::new(502, "upstream_error", "not an image"))?;
        blobs.put(&key, resp.body.clone(), ct).await.map_err(store_err)?;
        Ok(image(200, resp.body, ct, CACHE))
    }

    /// `PUT|GET /v1/sync/covers/{sha256}`: the user's own playlist pictures.
    pub(super) async fn own_cover(&self, svc: &Arc<Services>, req: &Request, name: &str) -> Result<Response, ApiErr> {
        let Some(blobs) = &self.blobs else { return Err(ApiErr::not_found("cover storage disabled")) };
        let a = self.auth(svc, req).await?;
        if !is_sha256_hex(name) {
            return Err(ApiErr::bad_request("name must be the sha256 of the image"));
        }
        let key = format!("users/{}/covers/{name}", a.user.id);
        match req.method {
            Method::Put => {
                if req.body.len() > MAX_OWN_COVER {
                    return Err(ApiErr::new(413, "payload_too_large", "cover larger than 512 KB"));
                }
                if hex(&Sha256::digest(&req.body)) != name {
                    return Err(ApiErr::bad_request("sha256 does not match"));
                }
                let ct = sniff_image(&req.body).ok_or_else(|| ApiErr::bad_request("JPEG, PNG, WebP or GIF only"))?;
                blobs.put(&key, req.body.clone(), ct).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({})))
            }
            Method::Get => {
                let (bytes, ct) = blobs.get(&key).await.map_err(store_err)?.ok_or_else(|| ApiErr::not_found("no such cover"))?;
                Ok(image(200, bytes, &ct, "private, max-age=31536000, immutable"))
            }
            _ => Err(method()),
        }
    }
}
