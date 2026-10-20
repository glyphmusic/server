use super::*;

impl Accounts {
    /// Where short playlist links point: the share origin, else this server.
    pub(super) fn share_link(&self, svc: &Services, id: &str) -> String {
        let origin = svc.config.share_origin.as_deref().unwrap_or(&self.origin);
        format!("{}/p/{id}", origin.trim_end_matches('/'))
    }

    pub(super) async fn shared(&self, id: &str) -> Result<SharedPlaylist, ApiErr> {
        let missing = || ApiErr::not_found("no such playlist link");
        if !crate::share::valid_id(id) {
            return Err(missing());
        }
        let s = self.store.share(id).await.map_err(store_err)?.ok_or_else(missing)?;
        let author = if s.kind == "album" { None } else { self.store.user(&s.owner_id).await.map_err(store_err)?.map(|u| u.handle) };
        Ok(SharedPlaylist { code: s.code, name: s.name, songs: s.songs, kind: s.kind, author })
    }

    /// `POST /v1/share`, `GET|DELETE /v1/share/<id>`, the `/p/<id>` page with
    /// its `/cover` and `/listen/<n>`, `/embed/p/<id>` and `/oembed`.
    pub(super) async fn share(&self, svc: &Services, req: &Request, path: &str) -> Result<Response, ApiErr> {
        let m = req.method;
        if path == "/oembed" || path.starts_with("/embed/p/") || path.starts_with("/p/") {
            if m != Method::Get {
                return Err(method());
            }
        }
        let lang = crate::share::lang(req.header("accept-language"));
        if path == "/oembed" {
            let id = req.query.get("url").and_then(crate::share::id_from_url).ok_or_else(|| ApiErr::not_found("not a Glyph link"))?;
            if req.query.get("format").is_some_and(|f| f != "json") {
                return Err(ApiErr::new(501, "not_implemented", "only format=json"));
            }
            let p = self.shared(&id).await?;
            let mut r = Response::json(200, &crate::share::oembed(&p, &self.share_link(svc, &id), lang));
            r.headers.push(("access-control-allow-origin".into(), "*".into()));
            r.headers.push(("cache-control".into(), "public, max-age=3600".into()));
            return Ok(r);
        }
        if let Some(id) = path.strip_prefix("/embed/p/") {
            return Ok(match self.shared(id).await {
                Ok(p) => crate::share::embed(Some((&p, &self.share_link(svc, id))), lang),
                Err(_) => crate::share::embed(None, lang),
            });
        }
        if let Some(rest) = path.strip_prefix("/p/") {
            let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
            if sub == "cover" {
                return self.share_cover(svc, req, id, 0).await;
            }
            if let Some(n) = sub.strip_prefix("cover/") {
                let n = n.parse().map_err(|_| ApiErr::not_found("no such song"))?;
                return self.share_cover(svc, req, id, n).await;
            }
            if let Some(n) = sub.strip_prefix("listen/") {
                return self.share_listen(svc, req, id, n).await;
            }
            if !sub.is_empty() {
                return Err(ApiErr::not_found("unknown endpoint"));
            }
            return Ok(match self.shared(id).await {
                Ok(p) => crate::share::page(Some((&p, &self.share_link(svc, id))), lang),
                Err(_) => crate::share::page(None, lang),
            });
        }
        if let Some(id) = path.strip_prefix("/v1/share/") {
            return match m {
                Method::Get => Ok(Response::json(200, &self.shared(id).await?)),
                Method::Delete => {
                    let a = self.auth(svc, req).await?;
                    if !self.store.delete_share(&a.user.id, id).await.map_err(store_err)? {
                        return Err(ApiErr::not_found("no such playlist link"));
                    }
                    Ok(Response::json(200, &serde_json::json!({})))
                }
                _ => Err(method()),
            };
        }
        if m != Method::Post {
            return Err(method());
        }
        let a = self.auth(svc, req).await?;
        self.limit(svc, &format!("share:{}", a.user.id), crate::share::CREATES_PER_HOUR, 3600).await?;
        let r: ShareRequest = json_body(req, crate::share::MAX_CODE_BYTES + 4096)?;
        let code = crate::share::clean_code(&r.code).ok_or_else(|| ApiErr::bad_request("code must be a glyph:playlist: code"))?;
        let mut id = crate::share::id_for(&a.user.id, &code, 8);
        if self.store.share(&id).await.map_err(store_err)?.is_some_and(|s| s.owner_id != a.user.id || s.code != code) {
            id = crate::share::id_for(&a.user.id, &code, crate::share::LONG_ID_LEN);
        }
        let share = Share {
            id: id.clone(),
            owner_id: a.user.id.clone(),
            name: crate::share::clean_name(r.name),
            songs: r.songs.unwrap_or(0),
            kind: crate::share::clean_kind(r.kind.as_deref()),
            created_at: svc.rt.now_ms(),
            code,
        };
        self.store.put_share(&share).await.map_err(store_err)?;
        Ok(Response::json(200, &ShareResponse { url: self.share_link(svc, &id), id }))
    }

    /// `GET /p/<id>/cover` (the first song's cover, for link cards) and
    /// `/p/<id>/cover/<n>` (song rows); the app icon when there is none.
    pub(super) async fn share_cover(&self, svc: &Services, req: &Request, id: &str, n: usize) -> Result<Response, ApiErr> {
        let ip = req.client_ip.clone().unwrap_or_else(|| "unknown".into());
        self.limit(svc, &format!("cover:{ip}"), 1200, 3600).await?;
        let p = self.shared(id).await?;
        let song = crate::share::songs(&p.code).into_iter().take(crate::share::MAX_LISTED).nth(n);
        let cover = match song {
            Some(t) => crate::server::find_cover(svc, &t).await,
            None => None,
        };
        Ok(redirect(&cover.unwrap_or_else(|| "/og.png".into()), "public, max-age=86400"))
    }

    /// `GET /p/<id>/listen/<n>`: resolves song `n` and redirects to its
    /// audio, for the web players. 404 for songs that only play in the app.
    pub(super) async fn share_listen(&self, svc: &Services, req: &Request, id: &str, n: &str) -> Result<Response, ApiErr> {
        let ip = req.client_ip.clone().unwrap_or_else(|| "unknown".into());
        self.limit(svc, &format!("listen:{ip}"), 300, 3600).await?;
        let n: usize = n.parse().map_err(|_| ApiErr::not_found("no such song"))?;
        let p = self.shared(id).await?;
        let track = crate::share::songs(&p.code).into_iter().take(crate::share::MAX_LISTED).nth(n).ok_or_else(|| ApiErr::not_found("no such song"))?;
        let mut resolved = crate::resolver::resolve(svc, glyph_protocol::ResolveRequest { youtube_only: false, track, priority: vec![] }).await;
        if svc.config.proxy_audio {
            crate::media::proxy_playback(&mut resolved);
        }
        use glyph_protocol::PlaybackSource;
        let (provider, url, preview) = match resolved.source {
            Some(PlaybackSource::Stream { provider, url, .. }) => (provider, url, false),
            Some(PlaybackSource::Preview { provider, url }) => (provider, url, true),
            _ => return Err(ApiErr::not_found("this song only plays in the Glyph app")),
        };
        // `?json`: the web players also show the source badge (`[YT]`) and
        // `[PREVIEW 0:30]` like the app's player bar.
        if req.query.get("json").is_some() {
            let mut r = Response::json(200, &serde_json::json!({ "url": url, "badge": provider.badge(), "preview": preview }));
            r.headers.push(("cache-control".into(), "no-store".into()));
            return Ok(r);
        }
        Ok(redirect(&url, "no-store"))
    }
}
