use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

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
    buckets: Vec<Bucket>,
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

#[derive(Serialize)]
pub struct Report {
    windows: Vec<Window>,
}

#[derive(Serialize)]
struct Window {
    group: &'static str,
    window: &'static str,
    remaining_pct: f64,
    reset_at_ms: Option<i64>,
}

pub fn parse(bytes: &[u8]) -> Option<Report> {
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
        for b in group.buckets {
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
            windows.push(Window {
                group: label,
                window,
                remaining_pct: fraction * 100.0,
                reset_at_ms,
            });
        }
    }
    (!windows.is_empty()).then_some(Report { windows })
}
