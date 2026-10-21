//! Private, expiring playback mailboxes. Identity always comes from authentication.
use super::*;
use glyph_protocol::playback::{Command, Delivery, Output, Poll, Receipt, Send, State};

pub const LIVE_HEADER: &str = "x-glyph-playback-live";

const PREFIX: &str = "/v1/devices/playback/";
const BODY_LIMIT: usize = 256 * 1024;
const PRESENCE_TTL: u64 = 45_000;
const COMMAND_TTL: u64 = 30_000;

impl Accounts {
    pub(super) async fn playback_route(
        &self,
        svc: &Arc<Services>,
        req: &Request,
        path: &str,
    ) -> Result<Response, ApiErr> {
        let a = self.auth(svc, req).await?;
        if req.method != Method::Post && !(req.method == Method::Get && path.ends_with("/live")) {
            return Err(method());
        }
        let now = svc.rt.now_ms();
        let scope = format!("playback:{}", a.user.id);
        let devices = self.store.devices(&a.user.id).await.map_err(store_err)?;
        self.limit(svc, &format!("playback:{}", a.device.id), 600, 60)
            .await?;
        match path.strip_prefix(PREFIX) {
            Some("live") => {
                if self.push.is_none() { return Err(ApiErr::not_found("playback push unavailable")); }
                if !req.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket")) { return Err(ApiErr::new(426, "websocket_required", "connect with a WebSocket")); }
                self.limit(svc, &format!("playback-live:{}", a.device.id), 60, 3600).await?;
                Ok(Response { status: 200, headers: vec![(LIVE_HEADER.into(), format!("{} {}", a.user.id, a.device.id))], body: Body::Bytes(vec![]) })
            }
            Some("ack") => {
                let mut receipt: Receipt = json_body(req, 4096)?;
                if receipt.id.len() != 16 || !receipt.id.bytes().all(|c| c.is_ascii_hexdigit()) { return Err(ApiErr::bad_request("invalid receipt")); }
                let key = format!("delivery:{}:{}", a.device.id, receipt.id);
                if self.store.mcp_get(&scope, &key, now).await.map_err(store_err)?.is_none() { return Err(ApiErr::not_found("command expired or not addressed to this device")); }
                receipt.done = true;
                receipt.error = receipt.error.map(|e| e.chars().take(512).collect());
                self.store.mcp_put(&scope, &format!("receipt:{}", receipt.id), &serde_json::to_string(&receipt).unwrap(), now + COMMAND_TTL).await.map_err(store_err)?;
                self.store.mcp_delete(&scope, &key).await.map_err(store_err)?;
                Ok(Response::json(200, &receipt))
            }
            Some("receipt") => {
                let receipt: Receipt = json_body(req, 4096)?;
                let owner = self.store.mcp_get(&scope, &format!("sender:{}", receipt.id), now).await.map_err(store_err)?;
                if owner.as_deref() != Some(a.device.id.as_str()) { return Err(ApiErr::not_found("command not sent by this device")); }
                let receipt = self.store.mcp_get(&scope, &format!("receipt:{}", receipt.id), now).await.map_err(store_err)?.and_then(|v| serde_json::from_str::<Receipt>(&v).ok()).unwrap_or(receipt);
                Ok(Response::json(200, &receipt))
            }
            Some("poll") => {
                let state: State = json_body(req, BODY_LIMIT)?;
                if !state.volume.is_finite() || !(0.0..=1.0).contains(&state.volume) {
                    return Err(ApiErr::bad_request("invalid volume"));
                }
                self.store
                    .mcp_put(
                        &scope,
                        &format!("state:{}", a.device.id),
                        &serde_json::to_string(&state).unwrap(),
                        now + PRESENCE_TTL,
                    )
                    .await
                    .map_err(store_err)?;
                let mut outputs = Vec::new();
                for device in devices {
                    if let Some(raw) = self
                        .store
                        .mcp_get(&scope, &format!("state:{}", device.id), now)
                        .await
                        .map_err(store_err)?
                    {
                        if let Ok(state) = serde_json::from_str::<State>(&raw) {
                            outputs.push(Output {
                                device: wire_device(&device, &a.device.id),
                                online: true,
                                state,
                            });
                            continue;
                        }
                    }
                    outputs.push(Output {
                        device: wire_device(&device, &a.device.id),
                        online: false,
                        state: State::default(),
                    });
                }
                let commands = self
                    .store
                    .mcp_take(&scope, &format!("command:{}:", a.device.id), now)
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .filter_map(|(_, v)| serde_json::from_str(&v).ok())
                    .collect();
                let deliveries = self.store.mcp_list(&scope, &format!("delivery:{}:", a.device.id), now).await.map_err(store_err)?.into_iter().filter_map(|(_, v)| serde_json::from_str(&v).ok()).collect();
                Ok(Response::json(
                    200,
                    &Poll {
                        devices: outputs,
                        commands,
                        deliveries,
                    },
                ))
            }
            Some("command") => {
                let send: Send = json_body(req, BODY_LIMIT)?;
                if let Some(request_id) = &send.request_id {
                    if request_id.len() != 32 || !request_id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(ApiErr::bad_request("invalid command request id")); }
                    if let Some(id) = self.store.mcp_get(&scope, &format!("request:{}:{request_id}", a.device.id), now).await.map_err(store_err)? {
                        return Ok(Response::json(200, &serde_json::json!({"id":id})));
                    }
                }
                if !devices.iter().any(|d| d.id == send.target) {
                    return Err(ApiErr::not_found("device is not in this account"));
                }
                let state = self
                    .store
                    .mcp_get(&scope, &format!("state:{}", send.target), now)
                    .await
                    .map_err(store_err)?
                    .and_then(|s| serde_json::from_str::<State>(&s).ok());
                let confirm = req.query.get("confirm").is_some_and(|s| s == "1");
                if !state.is_some_and(|s| s.available && (!confirm || s.acknowledgements)) {
                    return Err(ApiErr::new(
                        409,
                        "device_unavailable",
                        "device is offline or busy",
                    ));
                }
                match &send.command {
                    Command::Transfer {
                        tracks,
                        index,
                        position_ms,
                        ..
                    } if tracks.is_empty()
                        || tracks.len() > 100
                        || *index >= tracks.len()
                        || *position_ms > 604_800_000 =>
                    {
                        return Err(ApiErr::bad_request("invalid transfer"))
                    }
                    Command::Volume { volume }
                        if !volume.is_finite() || !(0.0..=1.0).contains(volume) =>
                    {
                        return Err(ApiErr::bad_request("invalid volume"))
                    }
                    Command::Seek { position_ms } if *position_ms > 604_800_000 => {
                        return Err(ApiErr::bad_request("invalid seek"))
                    }
                    _ => {}
                }
                let id = hex(&self.random(8));
                let key = if confirm { format!("delivery:{}:{id}", send.target) } else { format!("command:{}:{now:020}:{id}", send.target) };
                let payload = if confirm { serde_json::to_string(&Delivery { id: id.clone(), command: send.command.clone() }).unwrap() } else { serde_json::to_string(&send.command).unwrap() };
                self.store
                    .mcp_put(
                        &scope,
                        &key,
                        &payload,
                        now + COMMAND_TTL,
                    )
                    .await
                    .map_err(store_err)?;
                if confirm { self.store.mcp_put(&scope, &format!("sender:{id}"), &a.device.id, now + COMMAND_TTL * 2).await.map_err(store_err)?; }
                if confirm {
                    if let Some(request_id) = send.request_id { self.store.mcp_put(&scope, &format!("request:{}:{request_id}", a.device.id), &id, now + COMMAND_TTL * 2).await.map_err(store_err)?; }
                }
                if let Some(push) = &self.push { push.send(&scope, jam::To::Member(send.target), Some("{\"t\":\"wake\"}"), None).await; }
                Ok(Response::json(200, &serde_json::json!({"id": if confirm { Some(id) } else { None }})))
            }
            _ => Err(ApiErr::not_found("unknown playback endpoint")),
        }
    }
}
