use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const VERSION: u8 = 3;
pub const ACP_LAUNCH: &str = "/v4/agy-acp/launch";
pub const ACP_CHECK: &str = "/v4/agy-acp/check";
pub const ACP_LIST: &str = "/v4/agy-acp/list";
pub const ACP_LISTED: &str = "/v4/agy-acp/listed";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingMode {
    RoundRobin,
    MostRemaining,
    LeastRemaining,
    EarliestExpiry,
    Priority,
}
impl RoutingMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::RoundRobin => "round_robin",
            Self::MostRemaining => "most_remaining",
            Self::LeastRemaining => "least_remaining",
            Self::EarliestExpiry => "earliest_expiry",
            Self::Priority => "priority",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::RoundRobin,
            Self::MostRemaining,
            Self::LeastRemaining,
            Self::EarliestExpiry,
            Self::Priority,
        ]
        .into_iter()
        .find(|m| m.name() == s)
    }
    pub fn uses_remaining(self) -> bool {
        matches!(
            self,
            Self::MostRemaining | Self::LeastRemaining | Self::EarliestExpiry
        )
    }
}

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
impl Request {
    pub fn managed_agy_acp(&self) -> bool {
        self.tool == "agy"
            && self.login_profile.is_none()
            && self.argv.first().is_some_and(|a| a == "acp")
            && self.argv != ["acp", "--help"]
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListedPage {
    pub request_id: String,
    pub ticket: String,
    // Only native IDs and workspaces cross the socket; titles stay on the ACP stream.
    pub sessions: Vec<(String, String)>,
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
