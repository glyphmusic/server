//! Exit codes and error output shared by every command. Scripts and AI agents
//! rely on these numbers; they are listed in the CLI reference
//! (https://docs.glyph.my/self-hosting/cli#exit-codes), so never reuse one.

use serde::Serialize;

pub const GENERAL: i32 = 1;
/// Wrong or missing arguments (clap uses 2 as well).
pub const USAGE: i32 = 2;
pub const NOT_LOGGED_IN: i32 = 3;
pub const UNREACHABLE: i32 = 4;
/// Wrong password, expired login, forbidden.
pub const DENIED: i32 = 5;
pub const NOT_FOUND: i32 = 6;
/// The user answered "no" to a confirmation.
pub const CANCELLED: i32 = 7;

const DOCS: &str = "https://docs.glyph.my/self-hosting/cli";

#[derive(Debug)]
pub struct CliError {
    pub code: i32,
    pub msg: String,
    pub hint: Option<String>,
}

impl CliError {
    pub fn new(code: i32, msg: impl Into<String>) -> Self {
        Self { code, msg: msg.into(), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    fn kind(&self) -> &'static str {
        match self.code {
            USAGE => "usage",
            NOT_LOGGED_IN => "not_logged_in",
            UNREACHABLE => "unreachable",
            DENIED => "denied",
            NOT_FOUND => "not_found",
            CANCELLED => "cancelled",
            _ => "error",
        }
    }
}

impl From<String> for CliError {
    fn from(msg: String) -> Self {
        Self::new(GENERAL, msg)
    }
}

impl From<&str> for CliError {
    fn from(msg: &str) -> Self {
        Self::new(GENERAL, msg)
    }
}

/// For callers that still report plain strings.
impl From<CliError> for String {
    fn from(e: CliError) -> Self {
        match e.hint {
            Some(h) => format!("{}\n{h}", e.msg),
            None => e.msg,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

/// Prints the error (as JSON on stdout with `--json`) and exits with its code.
pub fn fail(e: CliError, json: bool) -> ! {
    if json {
        #[derive(Serialize)]
        struct Out<'a> { error: Inner<'a> }
        #[derive(Serialize)]
        struct Inner<'a> { code: i32, kind: &'a str, message: &'a str, hint: Option<&'a str> }
        let out = Out { error: Inner { code: e.code, kind: e.kind(), message: &e.msg, hint: e.hint.as_deref() } };
        println!("{}", serde_json::to_string(&out).unwrap_or_default());
    } else {
        eprintln!("error: {}", e.msg);
        if let Some(h) = &e.hint {
            eprintln!("help: {h}");
        }
        if e.code != CANCELLED {
            eprintln!("docs: {DOCS}#exit-codes");
        }
    }
    std::process::exit(e.code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_their_code_and_hint() {
        let e = CliError::new(NOT_LOGGED_IN, "not logged in").hint("Run `glyph remote login <url>`.");
        assert_eq!((e.code, e.kind()), (3, "not_logged_in"));
        let s: String = e.into();
        assert!(s.contains("glyph remote login"));
        assert_eq!(CliError::from("x").code, GENERAL);
    }
}
