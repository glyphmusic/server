//! Profiles: a picture and a banner per account, and a page other signed-in
//! users open by handle. Nothing shows until the owner makes the profile
//! public; then only the playlists they picked and, if they want, the songs
//! they played last. Everything goes away with the account (`delete_user`
//! and the `users/{id}/` blob prefix).
//!
//! - `GET|PATCH /v1/profile`: own settings ([`ProfileSettings`], [`ProfilePatch`]).
//! - `PUT|DELETE /v1/profile/avatar|banner`: the image as the raw body.
//! - `PUT /v1/profile/recent`: the songs played last (`{"tracks": [...]}`),
//!   kept only while `show_recent` is on.
//! - `GET /v1/profiles/{handle}`: [`PublicProfile`].
//! - `GET /v1/profiles/{handle}/playlists/{id}`: one shown playlist with its songs.
//! - `GET /v1/profiles/{handle}/images/{sha256}`: picture, banner or a shown
//!   playlist's cover.

use super::store::{Profile, User};
use super::{hex, image, is_sha256_hex, json_body, method, store_err, Accounts};
use crate::error::ApiErr;
use crate::server::{Request, Response};
use crate::traits::{Method, Services};
use glyph_protocol::sync_kind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_AVATAR: usize = 512 * 1024;
pub const MAX_BANNER: usize = 2 * 1024 * 1024;
/// Playlists one profile can show.
pub const MAX_SHOWN: usize = 100;
/// Songs kept for "recently played".
pub const MAX_RECENT: usize = 20;
const MAX_RECENT_BYTES: usize = 48 * 1024;

/// The owner's view (`GET|PATCH /v1/profile`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSettings {
    pub public: bool,
    pub show_recent: bool,
    /// Ids of the playlists shown on the profile.
    pub playlists: Vec<String>,
    /// sha256 of the picture / banner, if set.
    pub avatar: Option<String>,
    pub banner: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProfilePatch {
    pub public: Option<bool>,
    pub show_recent: Option<bool>,
    pub playlists: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfilePlaylist {
    pub id: String,
    pub name: String,
    pub songs: u32,
    /// sha256 of the cover (`.../images/{cover}`).
    pub cover: Option<String>,
    /// First songs, for the app's cover mosaic when there is no cover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<serde_json::Value>,
}

/// What a viewer sees (`GET /v1/profiles/{handle}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub handle: String,
    pub created_at: u64,
    pub avatar: Option<String>,
    pub banner: Option<String>,
    /// The profile is private: no playlists, no songs.
    pub private: bool,
    /// The viewer's own profile (shown even while private).
    pub own: bool,
    pub playlists: Vec<ProfilePlaylist>,
    /// Songs played last, newest first (`glyph_protocol::Track` objects).
    pub recent: Vec<serde_json::Value>,
}

fn settings_of(p: &Profile) -> ProfileSettings {
    ProfileSettings { public: p.public, show_recent: p.show_recent, playlists: p.playlists.clone(), avatar: p.avatar.clone(), banner: p.banner.clone() }
}

fn valid_playlist_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn blob_key(user_id: &str, sha: &str) -> String {
    format!("users/{user_id}/profile/{sha}")
}

impl Accounts {
    async fn profile_of(&self, user_id: &str) -> Result<Profile, ApiErr> {
        Ok(self.store.profile(user_id).await.map_err(store_err)?.unwrap_or_default())
    }

    /// A shown playlist's synced value (`name`, `tracks`, `cover`), if it still exists.
    async fn shown_playlist(&self, user: &User, p: &Profile, id: &str) -> Result<Option<serde_json::Value>, ApiErr> {
        if !p.playlists.iter().any(|x| x == id) {
            return Ok(None);
        }
        let items = self.store.items(&user.id, &[(sync_kind::PLAYLIST.to_string(), id.to_string())]).await.map_err(store_err)?;
        Ok(items.into_iter().find(|i| !i.deleted).and_then(|i| i.value).and_then(|v| serde_json::from_str(&v).ok()))
    }

    pub(super) async fn profile_route(&self, svc: &Services, req: &Request, path: &str) -> Result<Response, ApiErr> {
        let a = self.auth(svc, req).await?;
        let m = req.method;
        if let Some(rest) = path.strip_prefix("/v1/profiles/") {
            if m != Method::Get {
                return Err(method());
            }
            self.limit(svc, &format!("prof-view:{}", a.user.id), 240, 60).await?;
            return self.view(&a.user, rest).await;
        }
        let now = svc.rt.now_ms();
        let mut p = self.profile_of(&a.user.id).await?;
        match (path, m) {
            ("/v1/profile", Method::Get) => Ok(Response::json(200, &settings_of(&p))),
            ("/v1/profile", Method::Patch) => {
                let r: ProfilePatch = json_body(req, 16 * 1024)?;
                if let Some(v) = r.public {
                    p.public = v;
                }
                if let Some(v) = r.show_recent {
                    p.show_recent = v;
                    if !v {
                        p.recent = None;
                    }
                }
                if let Some(ids) = r.playlists {
                    if ids.len() > MAX_SHOWN || !ids.iter().all(|i| valid_playlist_id(i)) {
                        return Err(ApiErr::bad_request(format!("at most {MAX_SHOWN} valid playlist ids")));
                    }
                    let mut seen = std::collections::HashSet::new();
                    p.playlists = ids.into_iter().filter(|i| seen.insert(i.clone())).collect();
                }
                p.updated_at = now;
                self.store.put_profile(&a.user.id, &p).await.map_err(store_err)?;
                Ok(Response::json(200, &settings_of(&p)))
            }
            ("/v1/profile/recent", Method::Put) => {
                self.limit(svc, &format!("prof-recent:{}", a.user.id), 30, 60).await?;
                #[derive(Deserialize)]
                struct Recent {
                    tracks: Vec<serde_json::Value>,
                }
                let r: Recent = json_body(req, MAX_RECENT_BYTES)?;
                if !r.tracks.iter().all(|t| t.is_object()) {
                    return Err(ApiErr::bad_request("tracks must be song objects"));
                }
                if !p.show_recent {
                    return Ok(Response::json(200, &serde_json::json!({ "stored": false })));
                }
                let tracks: Vec<_> = r.tracks.into_iter().take(MAX_RECENT).collect();
                p.recent = Some(serde_json::Value::Array(tracks).to_string());
                p.updated_at = now;
                self.store.put_profile(&a.user.id, &p).await.map_err(store_err)?;
                Ok(Response::json(200, &serde_json::json!({ "stored": true })))
            }
            ("/v1/profile/avatar" | "/v1/profile/banner", Method::Put | Method::Delete) => {
                let Some(blobs) = &self.blobs else { return Err(ApiErr::not_found("image storage disabled")) };
                let banner = path.ends_with("banner");
                self.limit(svc, &format!("prof-img:{}", a.user.id), 20, 600).await?;
                let new = if m == Method::Put {
                    let max = if banner { MAX_BANNER } else { MAX_AVATAR };
                    if req.body.len() > max {
                        return Err(ApiErr::new(413, "payload_too_large", format!("image larger than {} KB", max / 1024)));
                    }
                    let bytes = normalize_image(&req.body, banner)?;
                    let sha = hex(&Sha256::digest(&bytes));
                    blobs.put(&blob_key(&a.user.id, &sha), bytes, "image/webp").await.map_err(store_err)?;
                    Some(sha)
                } else {
                    None
                };
                let slot = if banner { &mut p.banner } else { &mut p.avatar };
                let old = std::mem::replace(slot, new.clone());
                // Avatar and banner may be the same picture.
                let other = if banner { p.avatar.clone() } else { p.banner.clone() };
                if let Some(old) = old.filter(|o| Some(o) != new.as_ref() && Some(o) != other.as_ref()) {
                    blobs.delete_prefix(&blob_key(&a.user.id, &old)).await.map_err(store_err)?;
                }
                p.updated_at = now;
                self.store.put_profile(&a.user.id, &p).await.map_err(store_err)?;
                Ok(Response::json(200, &settings_of(&p)))
            }
            ("/v1/profile" | "/v1/profile/recent" | "/v1/profile/avatar" | "/v1/profile/banner", _) => Err(method()),
            _ => Err(ApiErr::not_found("unknown endpoint")),
        }
    }

    async fn view(&self, viewer: &User, rest: &str) -> Result<Response, ApiErr> {
        let mut parts = rest.splitn(3, '/');
        let raw = parts.next().unwrap_or_default();
        let gone = || ApiErr::not_found("no such profile");
        let handle = super::normalize_handle(raw).map_err(|_| gone())?;
        let user = self.store.user_by_handle(&handle).await.map_err(store_err)?.ok_or_else(gone)?;
        let p = self.profile_of(&user.id).await?;
        let own = user.id == viewer.id;
        let open = p.public || own;
        match (parts.next(), parts.next()) {
            (None, _) => {
                let mut playlists = Vec::new();
                if open {
                    let keys: Vec<(String, String)> = p.playlists.iter().map(|id| (sync_kind::PLAYLIST.to_string(), id.clone())).collect();
                    let items = self.store.items(&user.id, &keys).await.map_err(store_err)?;
                    // In the order the owner picked.
                    for id in &p.playlists {
                        let Some(v) = items.iter().find(|i| &i.key == id && !i.deleted).and_then(|i| i.value.as_deref()).and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok()) else { continue };
                        playlists.push(ProfilePlaylist {
                            id: id.clone(),
                            name: v["name"].as_str().unwrap_or_default().to_string(),
                            songs: v["tracks"].as_array().map_or(0, |t| t.len() as u32),
                            cover: v["cover"].as_str().filter(|s| is_sha256_hex(s)).map(String::from),
                            preview: v["tracks"].as_array().map(|t| t.iter().take(12).cloned().collect()).unwrap_or_default(),
                        });
                    }
                }
                let recent = match (&p.recent, open && p.show_recent) {
                    (Some(r), true) => serde_json::from_str(r).unwrap_or_default(),
                    _ => vec![],
                };
                Ok(Response::json(
                    200,
                    &PublicProfile { handle: user.handle, created_at: user.created_at, avatar: p.avatar, banner: p.banner, private: !p.public, own, playlists, recent },
                ))
            }
            (Some("playlists"), Some(id)) => {
                let v = if open { self.shown_playlist(&user, &p, id).await? } else { None };
                let v = v.ok_or_else(|| ApiErr::not_found("no such playlist"))?;
                Ok(Response::json(
                    200,
                    &serde_json::json!({
                        "id": id,
                        "name": v["name"].as_str().unwrap_or_default(),
                        "tracks": v["tracks"].as_array().cloned().unwrap_or_default(),
                        "cover": v["cover"].as_str().filter(|s| is_sha256_hex(s)),
                        "handle": user.handle,
                    }),
                ))
            }
            (Some("images"), Some(sha)) => {
                let Some(blobs) = &self.blobs else { return Err(ApiErr::not_found("image storage disabled")) };
                let missing = || ApiErr::not_found("no such image");
                if !is_sha256_hex(sha) {
                    return Err(missing());
                }
                let key = if p.avatar.as_deref() == Some(sha) || p.banner.as_deref() == Some(sha) {
                    blob_key(&user.id, sha)
                } else if open && self.shown_cover(&user, &p, sha).await? {
                    format!("users/{}/covers/{sha}", user.id)
                } else {
                    return Err(missing());
                };
                let (bytes, ct) = blobs.get(&key).await.map_err(store_err)?.ok_or_else(missing)?;
                Ok(image(200, bytes, &ct, "private, max-age=31536000, immutable"))
            }
            _ => Err(ApiErr::not_found("unknown endpoint")),
        }
    }

    async fn shown_cover(&self, user: &User, p: &Profile, sha: &str) -> Result<bool, ApiErr> {
        let keys: Vec<(String, String)> = p.playlists.iter().map(|id| (sync_kind::PLAYLIST.to_string(), id.clone())).collect();
        let items = self.store.items(&user.id, &keys).await.map_err(store_err)?;
        Ok(items.iter().filter(|i| !i.deleted).filter_map(|i| i.value.as_deref()).any(|v| {
            serde_json::from_str::<serde_json::Value>(v).ok().is_some_and(|v| v["cover"].as_str() == Some(sha))
        }))
    }
}

/// Decode untrusted uploads with bounded resources, discard metadata/animation,
/// and encode our own canonical WebP. Never trust MIME or a magic-byte check.
fn normalize_image(bytes: &[u8], banner: bool) -> Result<Vec<u8>, ApiErr> {
    let invalid = || ApiErr::bad_request("invalid or oversized image");
    let mut reader = ::image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|_| invalid())?;
    if !matches!(reader.format(), Some(::image::ImageFormat::Png | ::image::ImageFormat::Jpeg | ::image::ImageFormat::WebP | ::image::ImageFormat::Gif)) { return Err(invalid()); }
    let mut limits = ::image::Limits::default();
    limits.max_alloc = Some(32 * 1024 * 1024);
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| invalid())?;
    let mut picture = if banner { decoded.thumbnail(1500, 500) } else { decoded.thumbnail(512, 512) };
    loop {
        let rgba = picture.to_rgba8();
        let mut output = Vec::new();
        ::image::codecs::webp::WebPEncoder::new_lossless(&mut output)
            .encode(&rgba, rgba.width(), rgba.height(), ::image::ExtendedColorType::Rgba8).map_err(|_| invalid())?;
        if output.len() <= MAX_AVATAR { return Ok(output); }
        if picture.width().max(picture.height()) <= 64 { return Err(invalid()); }
        picture = picture.thumbnail((picture.width()*3/4).max(1), (picture.height()*3/4).max(1));
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;
    #[test]
    fn rejects_spoofed_images_and_reencodes_valid_uploads() {
        assert!(normalize_image(b"RIFF0000WEBPfake", false).is_err());
        assert!(normalize_image(b"\x89PNG\r\n\x1a\nfake", false).is_err());
        let source = ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_pixel(800, 600, ::image::Rgba([40,80,120,128])));
        let mut png = std::io::Cursor::new(Vec::new());
        source.write_to(&mut png, ::image::ImageFormat::Png).unwrap();
        let output = normalize_image(png.get_ref(), false).unwrap();
        assert_eq!(&output[8..12], b"WEBP");
        let decoded = ::image::load_from_memory(&output).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (512,384));
        assert_eq!(decoded.get_pixel(0,0).0[3],128);
        assert!(output.len() <= MAX_AVATAR);
    }
}
