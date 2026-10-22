//! Jam: listening together (see `glyph_protocol::jam`).
//!
//! The server only relays. The host's app owns the queue and the playback;
//! it publishes the jam's state and picks up what the members ask for.
//! Everything lives in the short-lived mailbox of the account store under
//! the scope `_jam:<CODE>`, one writer per key wherever two writers could
//! meet, so no write can lose another:
//!
//! * `meta`: who hosts it, since when (changes only on a takeover);
//! * `state`: the last published [`State`] (only the host writes it);
//! * `rev`: `<rev>:<ms>` of the last publish, so waiting and publishing
//!   read a few bytes instead of the whole state (up to [`MAX_STATE_BYTES`]);
//! * `now`: a newer position than the state's (`/now`, with its revision);
//! * `seen`: the host's last `/ops` (with `rev`: its last sign of life);
//! * `member:<handle>`: `{"id": user id}`, written when someone gets in;
//! * `knock:<handle>`: someone waiting for the host to let them in;
//! * `ban:<user id>`, `deny:<user id>`: removed / not let in;
//! * `op:<ms>:<rand>`: requests waiting for the host (taken by `/ops`).
//!
//! Host and members must be signed in. A jam ends when the host ends it or
//! after [`MAX_AGE_MS`]; a host has at most one jam at a time. When the
//! host's app is gone for [`HANDOVER_MS`], a member may take over.
//!
//! * `POST   /v1/jam`              start (host), answers the first state
//! * `GET    /v1/jam/{code}?rev=`  state, waits up to [`WAIT_MS`] for a newer one
//! * `DELETE /v1/jam/{code}`       end (host)
//! * `GET    /v1/jam/{code}/time`  server clock (clock sync)
//! * `POST   /v1/jam/{code}/join`  join: the state, or waiting for the host
//! * `POST   /v1/jam/{code}/leave` leave
//! * `POST   /v1/jam/{code}/op`    a request for the host ([`Op`])
//! * `GET    /v1/jam/{code}/ops`   the host's waiting requests (`?wait=0`: no waiting)
//! * `GET    /v1/jam/{code}/live`  WebSocket for pushes ([`Live`]), when the host has [`JamPush`]
//! * `POST   /v1/jam/{code}/state` publish (host)
//! * `POST   /v1/jam/{code}/now`   publish only the position ([`Now`]; host)
//! * `POST   /v1/jam/{code}/kick`  remove someone (host)
//! * `POST   /v1/jam/{code}/admit` let someone in, or not (host)
//! * `POST   /v1/jam/{code}/claim` take over from an offline host (member)
//! * `GET    /jam/{code}`          invite page (no sign-in; the app's QR code)
//!
//! Pushes are an extra: the mailbox stays the truth, and a member that
//! misses a push (or gets [`Live::Fallback`]) long polls as before. They
//! stay within a daily budget ([`PUSH_BUDGET`]), so a free Cloudflare
//! plan is never exceeded.

use super::*;
use glyph_protocol::jam::{
    AdmitRequest, JoinResponse, KickRequest, Live, Member, Now, Op, OpFrom, OpsResponse, State, TimeResponse,
    CODE_ALPHABET, CODE_LEN, HANDOVER_MS, HOST_TIMEOUT_MS, LIVE_CLOSE_AGAIN, LIVE_CLOSE_ENDED, LIVE_CLOSE_REMOVED,
    MAX_AFTER, MAX_AGE_MS, MAX_FEED, MAX_MEMBERS, MAX_QUEUE, OPS_WAIT_MS, WAIT_MS,
};

/// Host -> code of the running jam.
const HOSTS: &str = "_jam_host";
/// Largest published state.
pub const MAX_STATE_BYTES: usize = 512 * 1024;
const POLL_MS: u64 = 1000;
/// A knock waits this long for the host.
const KNOCK_TTL: u64 = 10 * MIN;
/// After a takeover, no other one for this long.
const CLAIM_COOLDOWN: u64 = MIN;
/// The host's own clock estimate is trusted this far off the server's.
const MAX_CLOCK_SKEW: u64 = 10 * SEC;
/// Pushes and `/live` connections per UTC day, all jams together. Durable
/// Objects are free up to 100,000 requests a day (then calls fail until
/// 00:00 UTC); the rest is left for the apps' keepalives (20 count as one)
/// and the last [`Live::Fallback`] of each jam.
pub const PUSH_BUDGET: u32 = 70_000;
const DAY_MS: u64 = 24 * 3600 * SEC;
/// Header of a `/live` answer that tells the host to hand the WebSocket to
/// its push relay: `host <handle>` or `guest <handle>`. Never sent to apps.
pub const LIVE_HEADER: &str = "x-glyph-jam-live";

/// Who a push goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum To {
    All,
    Host,
    /// Everyone but the host.
    Guests,
    /// One member's sockets (handle).
    Member(String),
}

/// Pushes to the open `/live` sockets of a jam (on the Worker a Durable
/// Object). Hosts without it answer `/live` with 404 and the apps long poll.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait JamPush: MaybeSendSync {
    /// Sends `msg` (a [`Live`] as JSON) to `to`; with `close`, then closes
    /// their sockets with this code. Failures are dropped: the apps catch up
    /// by asking.
    async fn send(&self, code: &str, to: To, msg: Option<&str>, close: Option<u16>);
    /// Account playback rooms (`playback:{user}`) also keep the devices'
    /// states, so reports cost no database writes: stores `report` for
    /// `device` (when given; changes go to the other devices at once) and
    /// answers the state of every device online there. `None`: unsupported
    /// or failed, the core keeps states in the account store instead.
    async fn playback_states(
        &self,
        _scope: &str,
        _device: &str,
        _report: Option<&glyph_protocol::playback::State>,
    ) -> Option<Vec<(String, glyph_protocol::playback::State)>> {
        None
    }
}

/// Paths this module serves.
pub fn owns(path: &str) -> bool {
    path == "/v1/jam" || path.starts_with("/v1/jam/") || path.starts_with("/jam/")
}

pub fn max_body_bytes(path: &str) -> Option<usize> {
    (owns(path) && path.ends_with("/state")).then_some(MAX_STATE_BYTES)
}

#[derive(Serialize, Deserialize)]
struct Meta {
    host_id: String,
    host: String,
    created: u64,
    /// The host's profile picture.
    #[serde(default)]
    avatar: Option<String>,
    /// Last takeover (0 = none).
    #[serde(default)]
    claimed: u64,
}

/// Value of the `now` key.
#[derive(Serialize, Deserialize)]
struct NowKey {
    rev: u64,
    now: Now,
}

/// Value of `member:` and `knock:` keys.
#[derive(Serialize, Deserialize)]
struct Who {
    id: String,
    #[serde(default)]
    avatar: Option<String>,
}

fn scope(code: &str) -> String {
    format!("_jam:{code}")
}

fn ended() -> ApiErr {
    ApiErr::new(404, "jam_ended", "this jam has ended")
}

fn not_member() -> ApiErr {
    ApiErr::new(403, "not_in_jam", "join this jam first")
}

fn host_only() -> ApiErr {
    ApiErr::new(403, "not_jam_host", "only the host can do this")
}

/// The host's own clock estimate is more exact (no upload delay), unless
/// its clock sync is off or it has none yet.
fn fix_clock(n: &mut Now, now: u64) {
    if n.at_ms.abs_diff(now) > MAX_CLOCK_SKEW {
        n.at_ms = now;
    }
}

fn ok() -> Response {
    Response::json(200, &serde_json::json!({}))
}

/// `/v1/jam/<code>[/<action>]` -> (code, action).
fn split(path: &str) -> Option<(String, &str)> {
    let rest = path.strip_prefix("/v1/jam/")?;
    let (code, action) = rest.split_once('/').unwrap_or((rest, ""));
    let code = glyph_protocol::jam::normalize_code(code)?;
    Some((code, action))
}

impl Accounts {
    fn jam_code(&self) -> String {
        let mut b = [0u8; CODE_LEN];
        self.entropy.fill(&mut b);
        b.iter().map(|x| CODE_ALPHABET[*x as usize % CODE_ALPHABET.len()] as char).collect()
    }

    async fn jam_get<T: serde::de::DeserializeOwned>(&self, code: &str, key: &str, now: u64) -> Result<Option<T>, ApiErr> {
        Ok(self.store.mcp_get(&scope(code), key, now).await.map_err(store_err)?.and_then(|v| serde_json::from_str(&v).ok()))
    }

    async fn jam_has(&self, code: &str, key: &str, now: u64) -> Result<bool, ApiErr> {
        Ok(self.store.mcp_get(&scope(code), key, now).await.map_err(store_err)?.is_some())
    }

    async fn jam_meta(&self, code: &str, now: u64) -> Result<Meta, ApiErr> {
        self.jam_get(code, "meta", now).await?.ok_or_else(ended)
    }

    /// The published state, with a newer position from `/now`.
    async fn jam_state(&self, code: &str, now: u64) -> Result<State, ApiErr> {
        let mut s: State = self.jam_get(code, "state", now).await?.ok_or_else(ended)?;
        if let Some(n) = self.jam_get::<NowKey>(code, "now", now).await?.filter(|n| n.rev > s.rev) {
            s.rev = n.rev;
            s.now = Some(n.now);
        }
        Ok(s)
    }

    /// The revision the next publish gets.
    async fn jam_next_rev(&self, code: &str, now: u64) -> Result<u64, ApiErr> {
        Ok(match self.jam_rev(code, now).await? {
            Some((rev, _)) => rev,
            None => self.jam_state(code, now).await?.rev,
        } + 1)
    }

    async fn jam_put(&self, code: &str, key: &str, value: &str, meta: &Meta) -> Result<(), ApiErr> {
        self.store.mcp_put(&scope(code), key, value, meta.created + MAX_AGE_MS).await.map_err(store_err)
    }

    async fn jam_is_member(&self, code: &str, handle: &str, now: u64) -> Result<bool, ApiErr> {
        self.jam_has(code, &format!("member:{handle}"), now).await
    }

    /// The host showed a sign of life.
    async fn jam_seen(&self, code: &str, meta: &Meta, now: u64) -> Result<(), ApiErr> {
        self.jam_put(code, "seen", &now.to_string(), meta).await
    }

    /// When the host was last seen (ms); the start when never.
    async fn jam_last_seen(&self, code: &str, meta: &Meta, now: u64) -> Result<u64, ApiErr> {
        let seen = self.store.mcp_get(&scope(code), "seen", now).await.map_err(store_err)?.and_then(|v| v.parse::<u64>().ok());
        let published = self.jam_rev(code, now).await?.map_or(0, |(_, at)| at);
        Ok(seen.unwrap_or(meta.created).max(published).max(meta.claimed))
    }

    /// Revision and time (ms) of the last publish; `None` for jams started
    /// before `rev` existed (their state has it).
    async fn jam_rev(&self, code: &str, now: u64) -> Result<Option<(u64, u64)>, ApiErr> {
        let v = self.store.mcp_get(&scope(code), "rev", now).await.map_err(store_err)?;
        Ok(v.and_then(|v| {
            let (rev, at) = v.split_once(':')?;
            Some((rev.parse().ok()?, at.parse().ok()?))
        }))
    }

    /// Stores a published state; it also counts as the host's sign of life.
    async fn jam_publish(&self, code: &str, state: &State, meta: &Meta, now: u64) -> Result<(), ApiErr> {
        self.jam_put(code, "state", &serde_json::to_string(state).unwrap_or_default(), meta).await?;
        // After the state: whoever sees the new revision finds it.
        self.jam_put(code, "rev", &format!("{}:{now}", state.rev), meta).await
    }

    /// Queues a request for the host and tells its app.
    #[allow(clippy::too_many_arguments)]
    async fn jam_op(&self, svc: &Services, code: &str, meta: &Meta, from: &str, op: Op, avatar: Option<String>, now: u64) -> Result<(), ApiErr> {
        // Same-millisecond ops keep their order (per instance); the random
        // part keeps keys of different instances apart.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut r = [0u8; 4];
        self.entropy.fill(&mut r);
        let key = format!("op:{now:013}:{seq:08x}{}", hex(&r));
        let v = serde_json::to_string(&OpFrom { from: from.to_string(), op, avatar }).unwrap_or_default();
        self.jam_put(code, &key, &v, meta).await?;
        self.jam_push(svc, code, To::Host, Some(&Live::Ops), None).await;
        Ok(())
    }

    /// Spends one unit of the day's push budget, if any is left.
    async fn jam_push_budget(&self, svc: &Services, now: u64) -> bool {
        let key = format!("jamdo:{}", now / DAY_MS);
        svc.limiter.check(&key, PUSH_BUDGET, DAY_MS / SEC).await == RateDecision::Allowed
    }

    /// Pushes to the jam's open sockets, within the day's budget. Once it is
    /// used up, everyone connected gets one [`Live::Fallback`] and long polls.
    async fn jam_push(&self, svc: &Services, code: &str, to: To, msg: Option<&Live>, close: Option<u16>) {
        let Some(push) = &self.push else { return };
        let now = svc.rt.now_ms();
        // Nobody of this jam ever connected (older apps): nothing to spend.
        if !self.jam_has(code, "live", now).await.unwrap_or(false) {
            return;
        }
        if !self.jam_push_budget(svc, now).await {
            let last = format!("jamdo-fallback:{}:{code}", now / DAY_MS);
            if svc.limiter.check(&last, 1, DAY_MS / SEC).await == RateDecision::Allowed {
                let msg = serde_json::to_string(&Live::Fallback).unwrap_or_default();
                push.send(code, To::All, Some(&msg), Some(1000)).await;
            }
            return;
        }
        let msg = msg.map(|m| serde_json::to_string(m).unwrap_or_default());
        push.send(code, to, msg.as_deref(), close).await;
    }

    /// Ends a jam: its state and meta go, the rest expires with them.
    async fn jam_end(&self, code: &str, host_id: &str) -> Result<(), ApiErr> {
        let s = scope(code);
        self.store.mcp_delete(&s, "rev").await.map_err(store_err)?;
        self.store.mcp_delete(&s, "state").await.map_err(store_err)?;
        self.store.mcp_delete(&s, "meta").await.map_err(store_err)?;
        self.store.mcp_delete(HOSTS, host_id).await.map_err(store_err)
    }

    async fn jam_avatar(&self, user_id: &str) -> Option<String> {
        self.store.profile(user_id).await.ok().flatten().and_then(|p| p.avatar)
    }

    /// The invite link of a jam (the share origin, else this server).
    fn jam_link(&self, svc: &Services, code: &str) -> String {
        let origin = svc.config.share_origin.as_deref().unwrap_or(&self.origin);
        format!("{}/jam/{code}", origin.trim_end_matches('/'))
    }

    /// The state as members get it: with the server clock, the link and
    /// whether the host is around.
    async fn jam_view(&self, svc: &Services, code: &str, meta: &Meta, state: &State) -> Result<State, ApiErr> {
        let now = svc.rt.now_ms();
        let mut s = state.clone();
        s.host_online = now.saturating_sub(self.jam_last_seen(code, meta, now).await?) < HOST_TIMEOUT_MS;
        s.server_ms = svc.rt.now_ms();
        s.link = Some(self.jam_link(svc, &s.code));
        Ok(s)
    }

    pub(crate) async fn jam_route(&self, svc: &Arc<Services>, req: &Request, path: &str) -> Result<Response, ApiErr> {
        let m = req.method;
        if m == Method::Options {
            return Ok(Response { status: 204, headers: vec![("allow".into(), "GET, POST, DELETE, OPTIONS".into())], body: Body::Bytes(vec![]) });
        }
        if let Some(code) = path.strip_prefix("/jam/") {
            if m != Method::Get {
                return Err(method());
            }
            let lang = crate::share::lang(req.header("accept-language"));
            let now = svc.rt.now_ms();
            let code = glyph_protocol::jam::normalize_code(code);
            let host = match &code {
                Some(code) => self.jam_meta(code, now).await.ok().map(|m| m.host),
                None => None,
            };
            return Ok(crate::share::jam_page(code.as_deref().unwrap_or_default(), host.as_deref(), lang));
        }
        let a = self.auth(svc, req).await?;
        let me = a.user.handle.clone();
        let now = svc.rt.now_ms();

        if path == "/v1/jam" {
            if m != Method::Post {
                return Err(method());
            }
            self.limit(svc, &format!("jam-start:{}", a.user.id), 20, 3600).await?;
            // One jam per host: a new one ends the old one.
            if let Some(old) = self.store.mcp_get(HOSTS, &a.user.id, now).await.map_err(store_err)? {
                self.jam_end(&old, &a.user.id).await?;
            }
            let mut code = self.jam_code();
            for _ in 0..5 {
                if self.store.mcp_get(&scope(&code), "meta", now).await.map_err(store_err)?.is_none() {
                    break;
                }
                code = self.jam_code();
            }
            let avatar = self.jam_avatar(&a.user.id).await;
            let meta = Meta { host_id: a.user.id.clone(), host: me.clone(), created: now, avatar: avatar.clone(), claimed: 0 };
            let state = State {
                code: code.clone(),
                rev: 1,
                host: me.clone(),
                members: vec![Member { handle: me.clone(), host: true, avatar: avatar.clone() }],
                ..Default::default()
            };
            self.jam_put(&code, "meta", &serde_json::to_string(&meta).unwrap_or_default(), &meta).await?;
            let who = Who { id: a.user.id.clone(), avatar };
            self.jam_put(&code, &format!("member:{me}"), &serde_json::to_string(&who).unwrap_or_default(), &meta).await?;
            self.jam_publish(&code, &state, &meta, now).await?;
            self.store.mcp_put(HOSTS, &a.user.id, &code, now + MAX_AGE_MS).await.map_err(store_err)?;
            return Ok(Response::json(200, &self.jam_view(svc, &code, &meta, &state).await?));
        }

        let (code, action) = split(path).ok_or_else(|| ApiErr::not_found("no jam with this code"))?;
        let meta = self.jam_meta(&code, now).await?;
        let host = meta.host_id == a.user.id;
        match (action, m) {
            ("", Method::Get) => {
                if !host && !self.jam_is_member(&code, &me, now).await? {
                    return Err(not_member());
                }
                let since: u64 = req.query.get("rev").and_then(|r| r.parse().ok()).unwrap_or(0);
                let deadline = now + WAIT_MS;
                loop {
                    let t = svc.rt.now_ms();
                    // Only the revision while waiting: the state is parsed once.
                    let rev = self.jam_rev(&code, t).await?.map(|(r, _)| r);
                    if rev.is_none_or(|r| r != since) || t >= deadline {
                        let state = self.jam_state(&code, t).await?;
                        return Ok(Response::json(200, &self.jam_view(svc, &code, &meta, &state).await?));
                    }
                    svc.rt.sleep(POLL_MS).await;
                }
            }
            ("", Method::Delete) => {
                if !host {
                    return Err(host_only());
                }
                self.jam_end(&code, &a.user.id).await?;
                self.jam_push(svc, &code, To::All, None, Some(LIVE_CLOSE_ENDED)).await;
                Ok(ok())
            }
            ("time", Method::Get) => Ok(Response::json(200, &TimeResponse { server_ms: svc.rt.now_ms() })),
            ("join", Method::Post) => {
                self.limit(svc, &format!("jam-join:{}", a.user.id), 120, 3600).await?;
                let state = self.jam_state(&code, now).await?;
                if host || self.jam_is_member(&code, &me, now).await? {
                    let state = self.jam_view(svc, &code, &meta, &state).await?;
                    return Ok(Response::json(200, &JoinResponse { state: Some(state), waiting: false }));
                }
                if self.jam_has(&code, &format!("ban:{}", a.user.id), now).await? {
                    return Err(ApiErr::new(403, "jam_removed", "the host removed you from this jam"));
                }
                if self.jam_has(&code, &format!("deny:{}", a.user.id), now).await? {
                    return Err(ApiErr::new(403, "jam_denied", "the host did not let you in"));
                }
                if state.members.len() >= MAX_MEMBERS {
                    return Err(ApiErr::new(409, "jam_full", "this jam is full"));
                }
                let avatar = self.jam_avatar(&a.user.id).await;
                let who = serde_json::to_string(&Who { id: a.user.id.clone(), avatar: avatar.clone() }).unwrap_or_default();
                if state.approve_joins {
                    // Knock once; asking again only waits.
                    let key = format!("knock:{me}");
                    if !self.jam_has(&code, &key, now).await? {
                        self.store.mcp_put(&scope(&code), &key, &who, (now + KNOCK_TTL).min(meta.created + MAX_AGE_MS)).await.map_err(store_err)?;
                        self.jam_op(svc, &code, &meta, &me, Op::Knock, avatar, now).await?;
                    }
                    return Ok(Response::json(202, &JoinResponse { state: None, waiting: true }));
                }
                self.jam_put(&code, &format!("member:{me}"), &who, &meta).await?;
                self.jam_op(svc, &code, &meta, &me, Op::Join, avatar, now).await?;
                let state = self.jam_view(svc, &code, &meta, &state).await?;
                Ok(Response::json(200, &JoinResponse { state: Some(state), waiting: false }))
            }
            ("leave", Method::Post) => {
                if host {
                    self.jam_end(&code, &a.user.id).await?;
                    self.jam_push(svc, &code, To::All, None, Some(LIVE_CLOSE_ENDED)).await;
                } else if self.jam_is_member(&code, &me, now).await? {
                    self.store.mcp_delete(&scope(&code), &format!("member:{me}")).await.map_err(store_err)?;
                    self.jam_op(svc, &code, &meta, &me, Op::Leave, None, now).await?;
                } else {
                    // Gave up waiting.
                    self.store.mcp_delete(&scope(&code), &format!("knock:{me}")).await.map_err(store_err)?;
                }
                Ok(ok())
            }
            ("op", Method::Post) => {
                if !host && !self.jam_is_member(&code, &me, now).await? {
                    return Err(not_member());
                }
                self.limit(svc, &format!("jam-op:{}", a.user.id), 120, 60).await?;
                let op: Op = json_body(req, 64 * 1024)?;
                if matches!(op, Op::Join | Op::Leave | Op::Knock) {
                    return Err(ApiErr::bad_request("use /join and /leave"));
                }
                self.jam_op(svc, &code, &meta, &me, op, None, now).await?;
                Ok(ok())
            }
            ("ops", Method::Get) => {
                if !host {
                    return Err(host_only());
                }
                self.jam_seen(&code, &meta, now).await?;
                // A host on push only picks up what is there.
                let wait = if req.query.get("wait").is_some_and(|w| w == "0") { 0 } else { OPS_WAIT_MS };
                let deadline = now + wait;
                loop {
                    let t = svc.rt.now_ms();
                    let mut ops: Vec<(String, String)> = self.store.mcp_take(&scope(&code), "op:", t).await.map_err(store_err)?;
                    if !ops.is_empty() || t >= deadline {
                        ops.sort_by(|a, b| a.0.cmp(&b.0));
                        let ops = ops.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect();
                        return Ok(Response::json(200, &OpsResponse { ops }));
                    }
                    // Ended or taken over meanwhile.
                    if self.jam_meta(&code, t).await?.host_id != a.user.id {
                        return Err(host_only());
                    }
                    svc.rt.sleep(POLL_MS).await;
                }
            }
            ("state", Method::Post) => {
                if !host {
                    return Err(host_only());
                }
                let mut s: State = json_body(req, MAX_STATE_BYTES)?;
                if s.queue.len() > MAX_QUEUE || s.members.len() > MAX_MEMBERS {
                    return Err(ApiErr::bad_request("too many songs or members"));
                }
                s.after.truncate(MAX_AFTER);
                if s.feed.len() > MAX_FEED {
                    let cut = s.feed.len() - MAX_FEED;
                    s.feed.drain(..cut);
                }
                s.code = code.clone();
                s.host = meta.host.clone();
                s.rev = self.jam_next_rev(&code, now).await?;
                s.server_ms = 0;
                s.host_online = true;
                s.link = None;
                if let Some(n) = s.now.as_mut() {
                    fix_clock(n, now);
                }
                // The host is always in, and listed first.
                s.members.retain(|x| x.handle != meta.host);
                s.members.insert(0, Member { handle: meta.host.clone(), host: true, avatar: meta.avatar.clone() });
                for x in s.members.iter_mut().skip(1) {
                    x.host = false;
                }
                self.jam_publish(&code, &s, &meta, now).await?;
                let rev = s.rev;
                if self.push.is_some() {
                    // As `GET` answers it (the host was just seen); the host
                    // has it already.
                    s.server_ms = svc.rt.now_ms();
                    s.link = Some(self.jam_link(svc, &code));
                    self.jam_push(svc, &code, To::Guests, Some(&Live::State { state: Box::new(s) }), None).await;
                }
                Ok(Response::json(200, &serde_json::json!({ "rev": rev })))
            }
            ("now", Method::Post) => {
                if !host {
                    return Err(host_only());
                }
                // A few hundred bytes instead of the whole state: what the
                // host sends when only the position moved.
                let mut n: Now = json_body(req, 64 * 1024)?;
                fix_clock(&mut n, now);
                let rev = self.jam_next_rev(&code, now).await?;
                let v = serde_json::to_string(&NowKey { rev, now: n.clone() }).unwrap_or_default();
                self.jam_put(&code, "now", &v, &meta).await?;
                self.jam_put(&code, "rev", &format!("{rev}:{now}"), &meta).await?;
                if self.push.is_some() {
                    let msg = Live::Now { rev, now: n, server_ms: svc.rt.now_ms() };
                    self.jam_push(svc, &code, To::Guests, Some(&msg), None).await;
                }
                Ok(Response::json(200, &serde_json::json!({ "rev": rev })))
            }
            ("kick", Method::Post) => {
                if !host {
                    return Err(host_only());
                }
                let k: KickRequest = json_body(req, 4096)?;
                let who = k.handle.trim_start_matches('@').to_ascii_lowercase();
                if who.is_empty() || who == meta.host {
                    return Err(ApiErr::bad_request("cannot remove this member"));
                }
                let key = format!("member:{who}");
                // Banned by account, so a new @handle does not get them back in.
                if let Some(w) = self.jam_get::<Who>(&code, &key, now).await? {
                    self.jam_put(&code, &format!("ban:{}", w.id), "1", &meta).await?;
                }
                self.store.mcp_delete(&scope(&code), &key).await.map_err(store_err)?;
                self.jam_push(svc, &code, To::Member(who), None, Some(LIVE_CLOSE_REMOVED)).await;
                Ok(ok())
            }
            ("admit", Method::Post) => {
                if !host {
                    return Err(host_only());
                }
                let r: AdmitRequest = json_body(req, 4096)?;
                let handle = r.handle.trim_start_matches('@').to_ascii_lowercase();
                let key = format!("knock:{handle}");
                let Some(w) = self.jam_get::<Who>(&code, &key, now).await? else {
                    return Err(ApiErr::not_found("nobody is waiting with this handle"));
                };
                self.store.mcp_delete(&scope(&code), &key).await.map_err(store_err)?;
                if r.allow {
                    let avatar = w.avatar.clone();
                    self.jam_put(&code, &format!("member:{handle}"), &serde_json::to_string(&w).unwrap_or_default(), &meta).await?;
                    self.jam_op(svc, &code, &meta, &handle, Op::Join, avatar, now).await?;
                } else {
                    self.store.mcp_put(&scope(&code), &format!("deny:{}", w.id), "1", (now + KNOCK_TTL).min(meta.created + MAX_AGE_MS)).await.map_err(store_err)?;
                }
                Ok(ok())
            }
            ("claim", Method::Post) => {
                if host {
                    return Ok(Response::json(200, &self.jam_view(svc, &code, &meta, &self.jam_state(&code, now).await?).await?));
                }
                if !self.jam_is_member(&code, &me, now).await? {
                    return Err(not_member());
                }
                let offline = now.saturating_sub(self.jam_last_seen(&code, &meta, now).await?);
                if offline < HANDOVER_MS || now.saturating_sub(meta.claimed) < CLAIM_COOLDOWN {
                    return Err(ApiErr::new(409, "host_online", "the host is still here"));
                }
                let avatar = self.jam_avatar(&a.user.id).await;
                let new = Meta { host_id: a.user.id.clone(), host: me.clone(), created: meta.created, avatar: avatar.clone(), claimed: now };
                self.jam_put(&code, "meta", &serde_json::to_string(&new).unwrap_or_default(), &new).await?;
                self.store.mcp_delete(HOSTS, &meta.host_id).await.map_err(store_err)?;
                self.store.mcp_put(HOSTS, &a.user.id, &code, meta.created + MAX_AGE_MS).await.map_err(store_err)?;
                // The old host stays a member and can come back as a guest.
                let mut s = self.jam_state(&code, now).await?;
                s.host = me.clone();
                s.rev += 1;
                s.members.retain(|x| x.handle != me);
                for x in s.members.iter_mut() {
                    x.host = false;
                }
                s.members.insert(0, Member { handle: me.clone(), host: true, avatar });
                self.jam_publish(&code, &s, &new, now).await?;
                // Everyone connects again, as host or guest now.
                self.jam_push(svc, &code, To::All, None, Some(LIVE_CLOSE_AGAIN)).await;
                Ok(Response::json(200, &self.jam_view(svc, &code, &new, &s).await?))
            }
            ("live", Method::Get) => {
                if !host && !self.jam_is_member(&code, &me, now).await? {
                    return Err(not_member());
                }
                if self.push.is_none() {
                    return Err(ApiErr::new(404, "jam_push_off", "this server does not push; ask instead"));
                }
                if !req.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket")) {
                    return Err(ApiErr::new(426, "websocket_required", "connect with a WebSocket"));
                }
                self.limit(svc, &format!("jam-live:{}", a.user.id), 60, 3600).await?;
                if !self.jam_push_budget(svc, now).await {
                    return Err(ApiErr::new(503, "jam_push_busy", "no pushes until tomorrow; ask instead"));
                }
                self.jam_put(&code, "live", "1", &meta).await?;
                let role = if host { "host" } else { "guest" };
                Ok(Response { status: 200, headers: vec![(LIVE_HEADER.into(), format!("{role} {me}"))], body: Body::Bytes(vec![]) })
            }
            ("" | "time" | "join" | "leave" | "op" | "ops" | "state" | "now" | "kick" | "admit" | "claim" | "live", _) => Err(method()),
            _ => Err(ApiErr::not_found("unknown endpoint")),
        }
    }
}
