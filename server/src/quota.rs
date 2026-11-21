use crate::{
    native::{Build, clean_command, stamp},
    paths, rpc,
    store::Config,
    types::{Error, Result, now},
};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use time::{
    OffsetDateTime,
    format_description::well_known::{Rfc2822, Rfc3339},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub fn auth_stamp(c: &Config) -> String {
    stamp(&c.home.join(if c.tool == "claude" {
        ".credentials.json"
    } else {
        "auth.json"
    }))
}
fn unavailable() -> Error {
    Error::unavailable("unavailable", "Quota unavailable.")
}
fn signin() -> Error {
    Error::bad(
        "sign_in_required",
        "Renew this config's native login to read quota.",
    )
}
fn percent(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
}
fn used_percent(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|n| n.is_finite() && *n >= 0.)
        .map(|n| n.min(100.))
}
fn seconds(v: &Value) -> Option<i64> {
    v.as_i64()?.checked_mul(1000)
}
fn display(v: &Value) -> Option<&str> {
    v.as_str()
        .filter(|s| s.len() <= 120 && !s.chars().any(char::is_control))
}
fn length(min: Option<i64>) -> String {
    match min {
        Some(m) if m > 0 && m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
        Some(m) if m > 0 => format!("{m}m"),
        _ => "window".into(),
    }
}
pub fn claude_snapshot(v: &Value) -> Result<Value> {
    let fields = v.as_object().ok_or_else(unavailable)?;
    let mut windows = Vec::new();
    for (k, w) in fields {
        let (kind, label) = match k.as_str() {
            "five_hour" => ("account", "5h".into()),
            "seven_day" => ("account", "7d".into()),
            k if k.starts_with("seven_day_")
                && k.len() < 60
                && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
            {
                ("model", format!("7d {}", &k[10..]))
            }
            _ => continue,
        };
        let Some(p) = used_percent(&w["utilization"]) else {
            continue;
        };
        let reset = w["resets_at"]
            .as_str()
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
            .map(|t| t.unix_timestamp() * 1000);
        windows.push(json!({"kind":kind,"label":label,"used_pct":p,"reset_at_ms":reset}));
    }
    Ok(
        json!({"plan":display(&v["plan_type"]),"windows":windows,"credits":null,"spend_control":null}),
    )
}
pub fn codex_snapshot(v: &Value) -> Result<Value> {
    let mut buckets = Vec::new();
    if let Some(map) = v["rateLimitsByLimitId"].as_object() {
        buckets.extend(map.iter().map(|(k, v)| (k.as_str(), v)));
    }
    if buckets.is_empty() {
        if !v["rateLimits"].is_object() {
            return Err(unavailable());
        }
        let fallback = &v["rateLimits"];
        buckets.push((fallback["limitId"].as_str().unwrap_or("codex"), fallback));
    }
    let mut windows = Vec::new();
    let mut plan = None;
    let mut credits = Value::Null;
    let mut spend = Value::Null;
    for (k, bucket) in buckets {
        let account = k == "codex";
        plan = plan.or_else(|| display(&bucket["planType"]));
        if account {
            let c = &bucket["credits"];
            if c.is_object() {
                credits = json!({"unlimited":c["unlimited"].as_bool(),"balance":display(&c["balance"]),"has_credits":c["hasCredits"].as_bool()});
            }
            let s = &bucket["individualLimit"];
            if s.is_object() || bucket["spendControlReached"].is_boolean() {
                spend = json!({"remaining_pct":percent(&s["remainingPercent"]),"reset_at_ms":seconds(&s["resetsAt"]),"limit":display(&s["limit"]),"used":display(&s["used"]),"reached":bucket["spendControlReached"].as_bool()});
            }
        }
        for key in ["primary", "secondary"] {
            let w = &bucket[key];
            let Some(p) = used_percent(&w["usedPercent"]) else {
                continue;
            };
            let duration = length(w["windowDurationMins"].as_i64());
            let label = if account {
                duration
            } else {
                format!(
                    "{} {duration}",
                    display(&bucket["limitName"])
                        .unwrap_or(k)
                        .chars()
                        .take(80)
                        .collect::<String>()
                )
            };
            windows.push(json!({"kind":if account{"account"}else{"model"},"label":label,"used_pct":p,"reset_at_ms":seconds(&w["resetsAt"])}));
        }
    }
    Ok(json!({"plan":plan,"windows":windows,"credits":credits,"spend_control":spend}))
}
pub struct FetchError {
    pub status: &'static str,
    pub retry_after_ms: Option<i64>,
}
impl From<Error> for FetchError {
    fn from(e: Error) -> Self {
        Self {
            status: match e.1 {
                "sign_in_required" => "sign_in_required",
                "rate_limited" => "rate_limited",
                _ => "unavailable",
            },
            retry_after_ms: None,
        }
    }
}
type Fetch = std::result::Result<Value, FetchError>;
fn curl_string(s: &str) -> Result<String> {
    if s.chars().any(char::is_control) {
        return Err(unavailable());
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
async fn claude(c: &Config, curl: &Path, cwd: &Path) -> Fetch {
    let cred = c.home.join(".credentials.json");
    if !cred.symlink_metadata().is_ok_and(|m| m.is_file()) {
        return Err(signin().into());
    }
    let v: Value =
        serde_json::from_slice(&paths::read_limited(&cred, 1 << 20)?).map_err(|_| signin())?;
    let oauth = &v["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(signin)?;
    if oauth["expiresAt"].as_i64().is_some_and(|t| t <= now()) {
        return Err(signin().into());
    }
    let mut opts = format!(
        "url = \"https://api.anthropic.com/api/oauth/usage\"\nheader = {}\nheader = \"Accept: application/json\"\nheader = \"anthropic-beta: oauth-2025-04-20\"\nuser-agent = \"cc-profile/{}\"\n",
        curl_string(&format!("Authorization: Bearer {token}"))?,
        env!("CARGO_PKG_VERSION")
    );
    if c.mode == "custom"
        && let Some(p) = c.https.as_ref().or(c.http.as_ref())
    {
        opts += &format!("proxy = {}\n", curl_string(p)?);
    }
    let mut child = clean_command(curl, cwd, Some(c))
        .args([
            "-q",
            "--silent",
            "--proto",
            "=https",
            "--connect-timeout",
            "3",
            "--max-time",
            "8",
            "--max-filesize",
            "262144",
            "--config",
            "-",
            "--write-out",
            "\n%{response_code}\n%header{retry-after}",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| unavailable())?;
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let res = tokio::time::timeout(Duration::from_secs(10), async {
        input.write_all(opts.as_bytes()).await?;
        drop(input);
        let mut bytes = Vec::new();
        output.take(262657).read_to_end(&mut bytes).await?;
        if bytes.len() > 262656 || !child.wait().await?.success() {
            return Err(unavailable());
        }
        Ok(bytes)
    })
    .await
    .unwrap_or_else(|_| Err(unavailable()));
    if res.is_err() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let bytes = res?;
    let mut parts = bytes.rsplitn(3, |b| *b == b'\n');
    let retry = std::str::from_utf8(parts.next().unwrap_or_default())
        .unwrap_or("")
        .trim();
    match parts.next() {
        Some(b"200") => {
            let body = serde_json::from_slice(parts.next().unwrap_or_default())
                .map_err(|_| unavailable())?;
            Ok(claude_snapshot(&body)?)
        }
        Some(b"401") => Err(signin().into()),
        Some(b"429") => Err(FetchError {
            status: "rate_limited",
            retry_after_ms: retry
                .parse::<i64>()
                .ok()
                .and_then(|s| s.checked_mul(1000))
                .or_else(|| {
                    OffsetDateTime::parse(retry, &Rfc2822)
                        .ok()
                        .map(|t| t.unix_timestamp() * 1000 - now())
                }),
        }),
        _ => Err(unavailable().into()),
    }
}
pub async fn fetch(c: &Config, build: &Build, curl: Option<&Path>, cwd: &Path) -> Fetch {
    if !c.present() {
        return Err(unavailable().into());
    }
    if c.tool == "codex" {
        let v = rpc::codex(
            build,
            c,
            cwd,
            "account/rateLimits/read",
            json!({"excludeResetCreditDetails":true}),
        )
        .await?;
        Ok(codex_snapshot(&v)?)
    } else {
        claude(c, curl.ok_or_else(unavailable)?, cwd).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_is_not_inferred_and_zero_is_a_value() {
        let s=claude_snapshot(&json!({"five_hour":{"utilization":0,"resets_at":"1970-01-01T00:00:00Z"},"seven_day":{"utilization":75.5},"seven_day_sonnet":{"utilization":2}})).unwrap();
        assert_eq!(s["windows"][0]["reset_at_ms"], 0);
        assert_eq!(s["windows"][0]["used_pct"], 0.0);
        assert_eq!(s["windows"][1]["used_pct"], 75.5);
        assert_eq!(s["windows"][2]["kind"], "model");
        let s = codex_snapshot(&json!({"rateLimits":{"planType":"business"}})).unwrap();
        assert_eq!(s["windows"], json!([]));
        assert!(s["credits"].is_null());
        let b = json!({"primary":{"windowDurationMins":10080,"usedPercent":0,"resetsAt":10},"individualLimit":{"remainingPercent":0,"resetsAt":20},"credits":{"unlimited":true}});
        let s=codex_snapshot(&json!({"rateLimitsByLimitId":{"codex":b,"spark":{"primary":{"usedPercent":100}}},"rateLimits":b})).unwrap();
        assert_eq!(s["windows"].as_array().unwrap().len(), 2);
        assert_eq!(s["windows"][0]["label"], "7d");
        assert_eq!(s["windows"][0]["reset_at_ms"], 10000);
        assert_eq!(s["spend_control"]["remaining_pct"], 0.0);
        assert_eq!(s["credits"]["unlimited"], true);
    }
    #[test]
    fn over_limit_usage_shows_an_exhausted_window() {
        let c = claude_snapshot(
            &json!({"five_hour":{"utilization":112},"seven_day":{"utilization":-1}}),
        )
        .unwrap();
        assert_eq!(c["windows"].as_array().unwrap().len(), 1);
        assert_eq!(c["windows"][0]["used_pct"], 100.);
        let c = codex_snapshot(&json!({"rateLimits":{"primary":{"usedPercent":104}}})).unwrap();
        assert_eq!(c["windows"][0]["used_pct"], 100.);
    }
}
