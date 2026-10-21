//! Jam pushes (`GET /v1/jam/{code}/live`): one Durable Object per jam code
//! relays to the apps' WebSockets. It stores nothing (the jam lives in D1)
//! and sleeps between messages: sockets hibernate, and the apps' keepalives
//! are answered by the runtime without waking it. The core counts every
//! connection and push against its daily budget, so the free plan holds.
use async_trait::async_trait;
use glyph_protocol::jam::{Live, LIVE_PING, LIVE_PONG};
use glyph_server_core::accounts::jam::{JamPush, To};
use serde::{Deserialize, Serialize};
use worker::{
    durable_object, DurableObject, Env, Headers, ObjectNamespace, Request, RequestInit, Response, Result, State,
    WebSocket, WebSocketIncomingMessage, WebSocketPair,
};

pub const BINDING: &str = "JAM_ROOM";
const HOST: &str = "host";
const GUEST: &str = "guest";

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
                if !matches!(role.as_str(), HOST | GUEST | "device") || handle.is_empty() {
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
                }
                Response::from_websocket(pair.client)
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

    /// The apps send nothing but keepalives (answered without this).
    async fn websocket_message(&self, _ws: WebSocket, _message: WebSocketIncomingMessage) -> Result<()> {
        Ok(())
    }

    async fn websocket_close(&self, ws: WebSocket, code: usize, _reason: String, _was_clean: bool) -> Result<()> {
        self.gone(&ws);
        // Completes the closing handshake (1005/1006 cannot be sent back).
        let code = u16::try_from(code).ok().filter(|c| !matches!(c, 1005 | 1006)).unwrap_or(1000);
        let _ = ws.close(Some(code), Some(""));
        Ok(())
    }

    async fn websocket_error(&self, ws: WebSocket, _error: worker::Error) -> Result<()> {
        self.gone(&ws);
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
