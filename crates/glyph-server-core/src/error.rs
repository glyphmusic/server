use glyph_protocol::ApiError;

/// An HTTP status plus the protocol's JSON error body.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiErr {
    pub status: u16,
    pub body: ApiError,
}

impl ApiErr {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiError { code: code.into(), message: message.into(), retry_after_s: None },
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "bad_request", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found", message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(503, "provider_unavailable", message)
    }
    pub fn rate_limited(retry_after_s: u64) -> Self {
        Self {
            status: 429,
            body: ApiError {
                code: "rate_limited".into(),
                message: "too many requests".into(),
                retry_after_s: Some(retry_after_s),
            },
        }
    }
}
