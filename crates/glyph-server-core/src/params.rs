//! Query-string parsing and validation.

use crate::error::ApiErr;

pub const MAX_PARAM_LEN: usize = 200;

#[derive(Debug, Default, Clone)]
pub struct Query(pub Vec<(String, String)>);

impl Query {
    pub fn parse(raw: &str) -> Self {
        Self(url::form_urlencoded::parse(raw.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
    }

    /// First value for `name`, trimmed; empty counts as absent.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    }

    pub fn required(&self, name: &str) -> Result<&str, ApiErr> {
        let v = self
            .get(name)
            .ok_or_else(|| ApiErr::bad_request(format!("missing query parameter `{name}`")))?;
        if v.chars().count() > MAX_PARAM_LEN {
            return Err(ApiErr::bad_request(format!("`{name}` is too long (max {MAX_PARAM_LEN})")));
        }
        Ok(v)
    }

    pub fn optional(&self, name: &str) -> Result<Option<&str>, ApiErr> {
        match self.get(name) {
            Some(v) if v.chars().count() > MAX_PARAM_LEN => {
                Err(ApiErr::bad_request(format!("`{name}` is too long (max {MAX_PARAM_LEN})")))
            }
            other => Ok(other),
        }
    }

    /// Duration in seconds (integer or decimal) -> milliseconds.
    pub fn duration_ms(&self, name: &str) -> Result<Option<u64>, ApiErr> {
        match self.get(name) {
            None => Ok(None),
            Some(v) => {
                let secs: f64 = v
                    .parse()
                    .map_err(|_| ApiErr::bad_request(format!("`{name}` must be a number of seconds")))?;
                if !secs.is_finite() || !(0.0..=86_400.0).contains(&secs) {
                    return Err(ApiErr::bad_request(format!("`{name}` out of range")));
                }
                Ok(Some((secs * 1000.0).round() as u64))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_percent_and_plus() {
        let q = Query::parse("q=daft%20punk+one&x=");
        assert_eq!(q.get("q"), Some("daft punk one"));
        assert_eq!(q.get("x"), None);
        assert_eq!(q.get("nope"), None);
    }

    #[test]
    fn required_and_length() {
        let q = Query::parse(&format!("q={}", "a".repeat(201)));
        assert_eq!(q.required("q").unwrap_err().status, 400);
        assert_eq!(Query::parse("").required("q").unwrap_err().body.code, "bad_request");
        assert_eq!(Query::parse("q=%20%20").required("q").unwrap_err().status, 400);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(Query::parse("duration=215").duration_ms("duration").unwrap(), Some(215_000));
        assert_eq!(Query::parse("duration=215.5").duration_ms("duration").unwrap(), Some(215_500));
        assert_eq!(Query::parse("").duration_ms("duration").unwrap(), None);
        assert!(Query::parse("duration=abc").duration_ms("duration").is_err());
        assert!(Query::parse("duration=-4").duration_ms("duration").is_err());
        assert!(Query::parse("duration=NaN").duration_ms("duration").is_err());
    }
}
