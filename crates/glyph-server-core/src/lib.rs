//! Platform-neutral core of glyph-server.
//!
//! Everything that touches the outside world (HTTP, cache, rate limiting, time)
//! sits behind the traits in [`traits`], so the same routing and resolver logic
//! runs on Cloudflare Workers (wasm32, `?Send` futures) and on a native
//! tokio/axum binary (`Send` futures).
//!
//! Rules enforced here: only metadata, IDs and official stream URLs leave the
//! server, never audio; provider keys never appear in responses or errors.

pub mod admin;
pub mod cache;
pub mod config;
pub mod error;
pub mod import;
pub mod merge;
pub mod params;
pub mod providers;
pub mod ratelimit;
pub mod resolver;
pub mod search;
pub mod server;
pub mod traits;

#[cfg(test)]
pub(crate) mod testutil;

pub use config::{Config, Secrets};
pub use error::ApiErr;
pub use server::{Body, Request, Response, Server};
pub use traits::{
    Cache, HttpClient, HttpError, HttpRequest, HttpResponse, Method, RateDecision, RateLimiter,
    Runtime, Services,
};

/// Version reported by `/v1/health` when the host does not override it.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
