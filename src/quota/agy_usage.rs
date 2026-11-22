use super::{signin, unavailable};
use crate::{
    agy,
    native::{Build, clean_command},
    store::Config,
    types::Result,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::io::AsyncReadExt;

#[derive(Deserialize)]
struct NativeResult {
    status: String,
    num_turns: u64,
    command: Command,
}

#[derive(Deserialize)]
struct Command {
    name: String,
    data: Usage,
}

#[derive(Deserialize)]
struct Usage {
    groups: Vec<Group>,
}

#[derive(Deserialize)]
struct Group {
    name: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    buckets: Value,
}

#[derive(Deserialize)]
struct Bucket {
    id: Option<String>,
    window: Option<String>,
    remaining_fraction: Option<f64>,
    reset_time: Option<String>,
    #[serde(default)]
    disabled: bool,
}

fn snapshot(bytes: &[u8]) -> Option<Value> {
    let result: NativeResult = serde_json::from_slice(bytes).ok()?;
    if result.status != "SUCCESS" || result.num_turns != 0 || result.command.name != "usage" {
        return None;
    }
    let mut windows = Vec::new();
    for group in result.command.data.groups {
        if group.disabled {
            continue;
        }
        let Some(label) = ["Gemini Models", "Claude and GPT models", "Gemini 4 Argon"]
            .into_iter()
            .find(|name| name.eq_ignore_ascii_case(&group.name))
        else {
            continue;
        };
        let Value::Array(rows) = group.buckets else {
            continue;
        };
        for b in rows
            .into_iter()
            .filter_map(|v| serde_json::from_value::<Bucket>(v).ok())
        {
            let Some(fraction) = b.remaining_fraction.filter(|v| (0.0..=1.0).contains(v)) else {
                continue;
            };
            if b.disabled {
                continue;
            }
            let window = match (b.window.as_deref(), label, b.id.as_deref()) {
                (Some("weekly"), _, _)
                | (None, "Gemini Models", Some("gemini-weekly"))
                | (None, "Claude and GPT models", Some("3p-weekly")) => "Weekly",
                (Some("5h"), _, _)
                | (None, "Gemini Models", Some("gemini-5h"))
                | (None, "Claude and GPT models", Some("3p-5h")) => "5 hours",
                _ => continue,
            };
            let reset_at_ms = b
                .reset_time
                .as_deref()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
                .filter(|ms| *ms > 0);
            windows.push(json!({
                "kind": "model", "label": format!("{label}, {window}"),
                "used_pct": 100. - fraction * 100., "reset_at_ms": reset_at_ms,
            }));
        }
    }
    (!windows.is_empty()).then(|| json!({"windows": windows}))
}

pub(super) async fn fetch(c: &Config, build: &Build) -> Result<Value> {
    if !build.supported || !build.unchanged() {
        return Err(unavailable());
    }
    if !agy::has_login(&c.home)? {
        return Err(signin());
    }
    let rt = agy::Runtime::new()?;
    let mut cmd = clean_command(&build.bin, &c.home, Some(c));
    rt.configure(&mut cmd, &c.home, &agy::usage_args(true));
    let mut child = cmd
        .env("NO_COLOR", "1")
        .env("BROWSER", "/bin/false")
        .stdout(Stdio::piped())
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(15), async {
        const LIMIT: usize = 64 * 1024;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .take(LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > LIMIT || !child.wait().await?.success() || !build.unchanged() {
            return Err(unavailable());
        }
        snapshot(&bytes).ok_or_else(unavailable)
    })
    .await
    .map_err(|_| unavailable())?
}
