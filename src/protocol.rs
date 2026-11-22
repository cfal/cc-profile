use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const VERSION: u8 = 2;

pub fn profile_login_args(tool: &str, status: bool) -> Option<&'static [&'static str]> {
    match (tool, status) {
        ("claude", false) => Some(&["auth", "login"]),
        ("claude", true) => Some(&["auth", "status"]),
        ("codex", false) => Some(&["login"]),
        ("codex", true) => Some(&["login", "status"]),
        ("agy", false) => Some(&["acp", "--login"]),
        ("agy", true) => Some(crate::agy::STATUS),
        _ => None,
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: String,
    pub ticket: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_profile: Option<i64>,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub binary: Option<PathBuf>,
    pub override_names: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub v: u8,
    pub request_id: String,
    pub tool: String,
    pub config_id: i64,
    pub label: String,
    pub decision: String,
    pub env_set: BTreeMap<String, String>,
    pub env_unset: Vec<String>,
    pub argv: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Failure {
    pub request_id: String,
    pub ticket: String,
    pub errno: i32,
}
