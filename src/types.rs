use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Error(pub StatusCode, pub &'static str, pub String);
impl Error {
    pub fn bad(code: &'static str, msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, code, msg.into())
    }
    pub fn conflict(code: &'static str, msg: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, code, msg.into())
    }
    pub fn missing(code: &'static str, msg: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, code, msg.into())
    }
    pub fn unavailable(code: &'static str, msg: impl Into<String>) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, code, msg.into())
    }
    pub fn denied() -> Self {
        Self(StatusCode::FORBIDDEN, "forbidden", "Access denied.".into())
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.2)
    }
}
impl std::error::Error for Error {}
impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        if err.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
            return Self::conflict("conflict", "This entry conflicts with an existing record.");
        }
        Self::unavailable("database", "Router database unavailable.")
    }
}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::unavailable("filesystem", "A local filesystem operation failed.")
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1,"message":self.2}))).into_response()
    }
}
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub const TOOLS: [&str; 3] = ["claude", "codex", "agy"];

pub fn tool(s: &str) -> Result<&str> {
    match s {
        "claude" | "codex" | "agy" => Ok(s),
        _ => Err(Error::bad("unknown_tool", "Expected claude, codex or agy.")),
    }
}
