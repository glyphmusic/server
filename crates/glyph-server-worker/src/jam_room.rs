//! Jam pushes (`GET /v1/jam/{code}/live`): one Durable Object per jam code
//! relays to the apps' WebSockets. For jams it stores nothing (the jam lives
//! in D1) and sleeps between messages: sockets hibernate, and the apps'
//! keepalives are answered by the runtime without waking it. The core counts
//! every connection and push against its daily budget, so the free plan holds.
//!
//! An account's room (`playback:{user}`, devices' `/v1/devices/playback/live`)
//! also keeps each device's playback state in its own storage instead of D1:
//! a device reports changes over its socket and the room passes them on to
//! the other devices at once; reports by HTTP poll come in through `/states`.
use async_trait::async_trait;
use glyph_protocol::jam::{Live, LIVE_PING, LIVE_PONG};
use glyph_protocol::playback::{Live as DeviceLive, State as DeviceState};
use std::collections::HashMap;
use glyph_server_core::accounts::jam::{JamPush, To};
use serde::{Deserialize, Serialize};
use worker::{
    durable_object, DurableObject, Env, Headers, ObjectNamespace, Request, RequestInit, Response, Result, State,
    WebSocket, WebSocketIncomingMessage, WebSocketPair,
};

pub const BINDING: &str = "JAM_ROOM";
const HOST: &str = "host";
const GUEST: &str = "guest";
const DEVICE: &str = "device";
/// Storage key of the devices' states (account rooms only).
const DEVICES: &str = "devices";
/// A device without a socket counts as online this long after its last report
/// (the core's poll presence).
const PRESENCE_MS: u64 = 45_000;
/// Reports equal to the stored one are still stored after this (presence).
const REFRESH_MS: u64 = 20_000;
/// States not reported for this long are dropped.
const KEEP_MS: u64 = 86_400_000;
/// Largest report taken over a socket (as the core's poll body limit).
const MAX_REPORT: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct Stored {
    state: DeviceState,
    at: u64,
}

/// `POST /states`: a device's report (if any) and the states it gets back.
#[derive(Serialize, Deserialize)]
struct States {
    device: String,
    report: Option<DeviceState>,
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

/// What the Worker asks a room to do (`POST /send`).
#[derive(Serialize, Deserialize)]
struct Send {
    /// `all`, `host`, `guest` or `u:<handle>`.
    to: String,
    msg: Option<String>,
    close: Option<u16>,
}

fn user_tag(handle: &str) -> String {
    format!("u:{handle}")
}

// `websocket` also exports `fetch`.
#[durable_object(websocket)]
pub struct JamRoom {
    state: State,
}

impl JamRoom {
    fn tell_guests(&self, online: bool) {
        let msg = serde_json::to_string(&Live::Host { online }).unwrap_or_default();
        for ws in self.state.get_websockets_with_tag(GUEST) {
            let _ = ws.send_with_str(&msg);
        }
    }

    /// A socket went away: when it was the host's last one, the guests
    /// learn that the host is gone.
    fn gone(&self, ws: &WebSocket) {
        if !self.state.get_tags(ws).iter().any(|t| t == HOST) {
            return;
        }
        if !self.state.get_websockets_with_tag(HOST).iter().any(|other| other != ws) {
            self.tell_guests(false);
        }
    }

    /// The device id of an account room's socket.
    fn device_of(&self, ws: &WebSocket) -> Option<String> {
        let tags = self.state.get_tags(ws);
        if !tags.iter().any(|t| t == DEVICE) {
            return None;
        }
        tags.iter().find_map(|t| t.strip_prefix("u:").map(String::from))
    }

    /// A device's last socket closed: the others ask once (it may still poll).
    fn device_gone(&self, ws: &WebSocket) {
        let Some(device) = self.device_of(ws) else { return };
        if self.state.get_websockets_with_tag(&user_tag(&device)).iter().any(|other| other != ws) {
            return;
        }
        let msg = serde_json::to_string(&DeviceLive::Gone { device }).unwrap_or_default();
        for other in self.state.get_websockets_with_tag(DEVICE) {
            if &other != ws {
                let _ = other.send_with_str(&msg);
            }
        }
    }

    async fn stored(&self) -> Result<HashMap<String, Stored>> {
        Ok(self.state.storage().get::<HashMap<String, Stored>>(DEVICES).await?.unwrap_or_default())
    }

    /// Stores `device`'s state; a change goes to the other devices at once.
    /// Unchanged reports only refresh the presence now and then.
    async fn report(&self, device: &str, state: DeviceState) -> Result<()> {
        let now = now_ms();
        let mut all = self.stored().await?;
        let old = all.get(device);
        let changed = old.is_none_or(|old| old.state.differs(&state, now.saturating_sub(old.at)));
        if !changed && old.is_some_and(|old| now.saturating_sub(old.at) < REFRESH_MS) {
            return Ok(());
        }
        if changed {
            let msg = serde_json::to_string(&DeviceLive::State { device: device.to_string(), state: state.clone() }).unwrap_or_default();
            let own = user_tag(device);
            for ws in self.state.get_websockets_with_tag(DEVICE) {
                if !self.state.get_tags(&ws).contains(&own) {
                    let _ = ws.send_with_str(&msg);
                }
            }
        }
        all.retain(|_, s| now.saturating_sub(s.at) < KEEP_MS);
        all.insert(device.to_string(), Stored { state, at: now });
        self.state.storage().put(DEVICES, &all).await
    }

    /// Devices with an open socket or a recent report, with their states.
    /// Devices report only changes, so a playing song's position is moved on
    /// by the time since its report.
    async fn online(&self) -> Result<Vec<(String, DeviceState)>> {
        let now = now_ms();
        Ok(self
            .stored()
            .await?
            .into_iter()
            .filter(|(id, s)| now.saturating_sub(s.at) < PRESENCE_MS || !self.state.get_websockets_with_tag(&user_tag(id)).is_empty())
            .map(|(id, mut s)| {
                if s.state.playing {
                    let position = s.state.position_ms.saturating_add(now.saturating_sub(s.at));
                    s.state.position_ms = s.state.duration_ms.map_or(position, |d| position.min(d));
                }
                (id, s.state)
            })
            .collect())
    }

    /// A device just connected: the other devices' states, so it needs no poll
    /// to show them (and knows this room passes states on).
    async fn greet(&self, ws: &WebSocket, device: &str) -> Result<()> {
        for (id, state) in self.online().await? {
            if id != device {
                let _ = ws.send_with_str(serde_json::to_string(&DeviceLive::State { device: id, state }).unwrap_or_default());
            }
        }
        Ok(())
    }
}

impl DurableObject for JamRoom {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        // Answered by the runtime itself: keepalives never wake the room.
        if let Ok(pair) = worker::worker_sys::WebSocketRequestResponsePair::new(LIVE_PING, LIVE_PONG) {
            self.state.set_websocket_auto_response(&pair);
        }
        match req.path().as_str() {
            "/connect" => {
                let role = req.headers().get("x-glyph-role")?.unwrap_or_default();
                let handle = req.headers().get("x-glyph-handle")?.unwrap_or_default();
                if !matches!(role.as_str(), HOST | GUEST | DEVICE) || handle.is_empty() {
                    return Response::error("bad connect", 400);
                }
                let pair = WebSocketPair::new()?;
                let tag = user_tag(&handle);
                self.state.accept_websocket_with_tags(&pair.server, &[role.as_str(), tag.as_str()]);
                if role == HOST {
                    self.tell_guests(true);
                } else if role == GUEST {
                    // Guests learn right away whether the host is on push too;
                    // if not, they keep asking.
                    let online = !self.state.get_websockets_with_tag(HOST).is_empty();
                    let _ = pair.server.send_with_str(serde_json::to_string(&Live::Host { online }).unwrap_or_default());
                } else if role == DEVICE {
                    self.greet(&pair.server, &handle).await?;
                }
                Response::from_websocket(pair.client)
            }
            "/states" => {
                let s: States = req.json().await?;
                if let Some(state) = s.report {
                    self.report(&s.device, state).await?;
                }
                Response::from_json(&self.online().await?)
            }
            "/send" => {
                let s: Send = req.json().await?;
                let sockets = if s.to == "all" { self.state.get_websockets() } else { self.state.get_websockets_with_tag(&s.to) };
                for ws in sockets {
                    if let Some(msg) = &s.msg {
                        let _ = ws.send_with_str(msg);
                    }
                    if let Some(code) = s.close {
                        let _ = ws.close(Some(code), Some(""));
                    }
                }
                Response::empty()
            }
            _ => Response::error("not found", 404),
        }
    }

    /// Jam apps send nothing but keepalives (answered without this); devices
    /// also report their playback state.
    async fn websocket_message(&self, ws: WebSocket, message: WebSocketIncomingMessage) -> Result<()> {
        let WebSocketIncomingMessage::String(text) = message else { return Ok(()) };
        if text.len() > MAX_REPORT {
            return Ok(());
        }
        let Some(device) = self.device_of(&ws) else { return Ok(()) };
        if let Ok(DeviceLive::Report { state }) = serde_json::from_str::<DeviceLive>(&text) {
            if state.volume.is_finite() && (0.0..=1.0).contains(&state.volume) {
                self.report(&device, state).await?;
            }
        }
        Ok(())
    }

    async fn websocket_close(&self, ws: WebSocket, code: usize, _reason: String, _was_clean: bool) -> Result<()> {
        self.gone(&ws);
        self.device_gone(&ws);
        // Completes the closing handshake (1005/1006 cannot be sent back).
        let code = u16::try_from(code).ok().filter(|c| !matches!(c, 1005 | 1006)).unwrap_or(1000);
        let _ = ws.close(Some(code), Some(""));
        Ok(())
    }

    async fn websocket_error(&self, ws: WebSocket, _error: worker::Error) -> Result<()> {
        self.gone(&ws);
        self.device_gone(&ws);
        Ok(())
    }
}

/// The core's pushes, through the jam's room.
pub struct RoomPush {
    pub ns: ObjectNamespace,
}

#[async_trait(?Send)]
impl JamPush for RoomPush {
    async fn send(&self, code: &str, to: To, msg: Option<&str>, close: Option<u16>) {
        let to = match to {
            To::All => "all".to_string(),
            To::Host => HOST.to_string(),
            To::Guests => GUEST.to_string(),
            To::Member(handle) => user_tag(&handle),
        };
        let body = Send { to, msg: msg.map(String::from), close };
        let work = async {
            let stub = self.ns.get_by_name(code)?;
            let headers = Headers::new();
            headers.set("content-type", "application/json")?;
            let mut init = RequestInit::new();
            init.with_method(worker::Method::Post)
                .with_headers(headers)
                .with_body(Some(serde_json::to_string(&body).map_err(|e| worker::Error::RustError(e.to_string()))?.into()));
            stub.fetch_with_request(Request::new_with_init("https://jam-room/send", &init)?).await
        };
        if let Err(e) = work.await {
            worker::console_error!("jam push failed: {e}");
        }
    }

    async fn playback_states(&self, scope: &str, device: &str, report: Option<&DeviceState>) -> Option<Vec<(String, DeviceState)>> {
        let body = States { device: device.to_string(), report: report.cloned() };
        let work = async {
            let stub = self.ns.get_by_name(scope)?;
            let headers = Headers::new();
            headers.set("content-type", "application/json")?;
            let mut init = RequestInit::new();
            init.with_method(worker::Method::Post)
                .with_headers(headers)
                .with_body(Some(serde_json::to_string(&body).map_err(|e| worker::Error::RustError(e.to_string()))?.into()));
            let mut resp = stub.fetch_with_request(Request::new_with_init("https://jam-room/states", &init)?).await?;
            if resp.status_code() != 200 {
                return Err(worker::Error::RustError(format!("status {}", resp.status_code())));
            }
            resp.json::<Vec<(String, DeviceState)>>().await
        };
        match work.await {
            Ok(states) => Some(states),
            Err(e) => {
                worker::console_error!("playback states failed: {e}");
                None
            }
        }
    }
}

/// Hands an app's WebSocket to the jam's room (`live`: `<role> <handle>`,
/// as the core checked it).
pub async fn connect(env: &Env, code: &str, live: &str) -> Result<worker::Response> {
    let (role, handle) = live.split_once(' ').unwrap_or_default();
    let stub = env.durable_object(BINDING)?.get_by_name(code)?;
    let headers = Headers::new();
    headers.set("upgrade", "websocket")?;
    headers.set("x-glyph-role", role)?;
    headers.set("x-glyph-handle", handle)?;
    let mut init = RequestInit::new();
    init.with_method(worker::Method::Get).with_headers(headers);
    stub.fetch_with_request(Request::new_with_init("https://jam-room/connect", &init)?).await
}
