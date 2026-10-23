//! Claims and verified data.
//!
//! The app is open source, so every request body is a [`Claimed`] value:
//! well-formed (see [`glyph_protocol::trust`]) but not true. Only the client
//! that sent it gets answers built from it. Caches that every client reads
//! take [`Verified`] values only, and those come from code that checked them
//! against what the server found itself (see the resolver and the hints).

use crate::cache;
use crate::traits::Cache;
use glyph_protocol::{ResolveRequest, Track, YouTubeHintRequest};
use serde::Serialize;

/// A value a client sent, after the protocol's shape checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Claimed<T>(T);

impl<T> Claimed<T> {
    pub fn get(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl From<Track> for Claimed<Track> {
    fn from(mut t: Track) -> Self {
        t.sanitize();
        Self(t)
    }
}

impl From<ResolveRequest> for Claimed<ResolveRequest> {
    fn from(mut r: ResolveRequest) -> Self {
        r.track.sanitize();
        Self(r)
    }
}

impl From<YouTubeHintRequest> for Claimed<YouTubeHintRequest> {
    fn from(mut r: YouTubeHintRequest) -> Self {
        r.track.sanitize();
        Self(r)
    }
}

/// A value the server found or checked itself. Construct it only where that
/// check happens; nothing derived from a [`Claimed`] value without such a
/// check may become one.
#[derive(Debug, Clone, PartialEq)]
pub struct Verified<T>(T);

impl<T> Verified<T> {
    /// The caller has checked `value` (keep the reason next to the call).
    pub(crate) fn checked(value: T) -> Self {
        Self(value)
    }

    pub fn get(&self) -> &T {
        &self.0
    }
}

/// Writes a cache entry that every client may read.
pub async fn put_shared<T: Serialize>(cache: &dyn Cache, key: &str, value: &Verified<T>, ttl_s: u64) {
    cache::put_json(cache, key, &value.0, ttl_s).await;
}
