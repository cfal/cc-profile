mod agy_usage;
mod claude_usage;

use crate::{
    native::{Build, PROXIES, stamp},
    rpc,
    store::Config,
    types::{Error, Result},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const FRESH_MS: i64 = 300_000;

pub fn auth_stamp(c: &Config) -> String {
    let path = match c.tool.as_str() {
        "claude" => c.home.join(".credentials.json"),
        "codex" => c.home.join("auth.json"),
        _ => return String::new(),
    };
    stamp(&path)
}
// Claude and Codex observations are independent of edits to label, priority or routing state.
fn input_stamp(c: &Config, auth: &str) -> (String, bool) {
    let inherited = (c.mode == "inherit").then(|| PROXIES.map(|k| std::env::var(k).ok()));
    // Older normalizers collapsed malformed reset timestamps into trustworthy-looking nulls.
    let mut fields = json!([
        if c.tool == "claude" {
            "claude-get-usage-v1"
        } else {
            "quota-v2"
        },
        c.dev,
        c.ino,
        auth,
        c.mode,
        c.proxy_url,
        inherited
    ]);
    let identifiable = !auth.is_empty() && (c.tool != "codex" || unmanaged_file(c));
    if c.tool == "codex" {
        fields
            .as_array_mut()
            .unwrap()
            .push(json!([stamp(&c.home.join("config.toml")), identifiable]));
    }
    (
        format!("{:x}", Sha256::digest(fields.to_string())),
        identifiable,
    )
}
pub fn inputs(c: &Config) -> Option<String> {
    if !c.directory_matches() {
        return None;
    }
    let auth = if c.tool == "agy" {
        let login = crate::agy::login_stamp(&c.home).ok()?;
        json!(["agy-usage-v1", c.revision, login]).to_string()
    } else {
        auth_stamp(c)
    };
    Some(input_stamp(c, &auth).0)
}
fn absent(path: &Path) -> bool {
    path.symlink_metadata()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}
// Codex searches up to a .git file or a .git directory with HEAD (or cwd alone).
// Don't assume the quota worker and a new launch resolved the same project overrides.
pub fn no_codex_project_config(cwd: &Path) -> bool {
    let mut root = cwd;
    for p in cwd.ancestors() {
        let git = p.join(".git");
        match git.metadata() {
            Ok(m) if m.is_dir() && git.join("HEAD").metadata().is_err() => continue,
            Ok(_) => {
                root = p;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    cwd.ancestors()
        .take_while(|p| p.starts_with(root))
        .all(|p| absent(&p.join(".codex/config.toml")))
}
fn unmanaged_file(c: &Config) -> bool {
    c.home
        .join("auth.json")
        .symlink_metadata()
        .is_ok_and(|m| m.is_file())
        && absent(&c.home.join("cloud-config-bundle-cache.json"))
        && ["config.toml", "managed_config.toml", "requirements.toml"]
            .iter()
            .all(|name| absent(&Path::new("/etc/codex").join(name)))
}
// Ordering for expiry routing: spend caps can exclude a profile, but only a
// positive account window can promote it out of Unknown.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum WindowExpiry {
    At(i64),
    Undated,
    Unknown,
}

#[derive(Clone, Copy)]
pub(crate) struct Allowance {
    pub remaining: f64,
    pub expiry: WindowExpiry,
}

fn allowance(snapshot: &Value, ms: i64) -> Option<Allowance> {
    let live =
        |w: &Value| w["reset_at_ms"].is_null() || w["reset_at_ms"].as_i64().is_some_and(|t| t > ms);
    let spend = &snapshot["spend_control"];
    let mut remaining = if !live(spend) {
        None
    } else if spend["reached"] == true {
        Some(0.)
    } else {
        percent(&spend["remaining_pct"])
    };
    let mut expiry = WindowExpiry::Unknown;
    for w in snapshot["windows"].as_array().into_iter().flatten() {
        if w["kind"] == "account"
            && live(w)
            && let Some(used) = percent(&w["used_pct"])
        {
            let left = 100. - used;
            remaining = Some(remaining.map_or(left, |r| r.min(left)));
            if left > 0. {
                expiry = expiry.min(
                    w["reset_at_ms"]
                        .as_i64()
                        .map_or(WindowExpiry::Undated, WindowExpiry::At),
                );
            }
        }
    }
    remaining.map(|remaining| Allowance { remaining, expiry })
}

// Only called for present profiles. Cache failures or unidentifiable logins cannot deny a launch.
pub(crate) fn cached_allowance(db: &Connection, c: &Config, ms: i64) -> Option<Allowance> {
    if c.tool != "codex" {
        // Claude can return undated cached usage; Antigravity reports model pools only.
        return None;
    }
    let (stamp, snapshot): (String, String) = db
        .prepare_cached(
            "SELECT stamp,snapshot FROM quota_cache WHERE config_id=? AND status='fresh' AND failures=0
             AND attempt_ms=success_ms
             AND typeof(success_ms)='integer' AND success_ms>? AND success_ms<=?",
        )
        .ok()?
        .query_row(params![c.id, ms.saturating_sub(FRESH_MS), ms], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok()?;
    let (current, identifiable) = input_stamp(c, &auth_stamp(c));
    if !identifiable || current != stamp {
        return None;
    }
    let snapshot: Value = serde_json::from_str(&snapshot).ok()?;
    if snapshot["file_auth_verified"] != true {
        return None;
    }
    allowance(&snapshot, ms)
}

pub fn cached_remaining(db: &Connection, c: &Config, ms: i64) -> Option<f64> {
    cached_allowance(db, c, ms).map(|a| a.remaining)
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
fn claude_window(w: &Value, kind: &str, label: String) -> Option<Value> {
    let p = used_percent(&w["utilization"])?;
    let reset = match &w["resets_at"] {
        Value::Null => None,
        v => Some(
            i64::try_from(
                OffsetDateTime::parse(v.as_str()?, &Rfc3339)
                    .ok()?
                    .unix_timestamp_nanos()
                    / 1_000_000,
            )
            .ok()?,
        ),
    };
    Some(json!({"kind":kind,"label":label,"used_pct":p,"reset_at_ms":reset}))
}
fn claude_snapshot(v: &Value) -> Result<Value> {
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
        windows.extend(claude_window(w, kind, label));
    }
    if let Some(scoped) = v["model_scoped"].as_array() {
        for w in scoped.iter().take(64) {
            if let Some(name) = display(&w["display_name"]).filter(|s| !s.trim().is_empty()) {
                windows.extend(claude_window(w, "model", format!("7d {name}")));
            }
        }
    }
    Ok(json!({"windows":windows,"credits":null,"spend_control":null}))
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
        let key = match &fallback["limitId"] {
            Value::Null => "codex",
            Value::String(key) => key,
            _ => return Err(unavailable()),
        };
        buckets.push((key, fallback));
    }
    let mut windows = Vec::new();
    let mut plan = None;
    let mut credits = Value::Null;
    let mut spend = Value::Null;
    for (k, bucket) in buckets {
        let account = k == "codex" && (bucket["limitId"].is_null() || bucket["limitId"] == "codex");
        plan = plan.or_else(|| display(&bucket["planType"]));
        if account {
            let c = &bucket["credits"];
            if c.is_object() {
                credits = json!({"unlimited":c["unlimited"].as_bool(),"balance":display(&c["balance"]),"has_credits":c["hasCredits"].as_bool()});
            }
            let s = &bucket["individualLimit"];
            if (s.is_object() || bucket["spendControlReached"].is_boolean())
                && (s["resetsAt"].is_null() || seconds(&s["resetsAt"]).is_some())
            {
                spend = json!({"remaining_pct":percent(&s["remainingPercent"]),"reset_at_ms":seconds(&s["resetsAt"]),"limit":display(&s["limit"]),"used":display(&s["used"]),"reached":bucket["spendControlReached"].as_bool()});
            }
        }
        for key in ["primary", "secondary"] {
            let w = &bucket[key];
            let Some(p) = used_percent(&w["usedPercent"]) else {
                continue;
            };
            if !w["resetsAt"].is_null() && seconds(&w["resetsAt"]).is_none() {
                continue;
            }
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
    let resets = v["rateLimitResetCredits"]["availableCount"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok());
    Ok(
        json!({"plan":plan,"windows":windows,"credits":credits,"spend_control":spend,"earned_resets":resets}),
    )
}
pub async fn fetch(c: &Config, build: &Build, cwd: &Path) -> Result<Value> {
    if !c.present() {
        return Err(unavailable());
    }
    if c.tool == "agy" {
        return agy_usage::fetch(c, build).await;
    }
    if c.tool == "codex" {
        let (mut client, quota) = tokio::time::timeout(Duration::from_secs(12), async {
            let mut client = rpc::Codex::open(build, c, cwd).await?;
            let quota = client
                .call(
                    2,
                    "account/rateLimits/read",
                    json!({"excludeResetCreditDetails":true}),
                )
                .await
                .and_then(|v| codex_snapshot(&v));
            Ok::<_, Error>((client, quota))
        })
        .await
        .map_err(|_| unavailable())??;
        // A failed/unsupported probe does not hide otherwise valid display data.
        let bound = quota.is_ok()
            && unmanaged_file(c)
            && no_codex_project_config(cwd)
            && matches!(
                tokio::time::timeout(Duration::from_secs(2), client.file_auth(cwd)).await,
                Ok(Ok(true))
            );
        client.close().await;
        let mut snapshot = quota?;
        snapshot["file_auth_verified"] = bound.into();
        Ok(snapshot)
    } else {
        claude_usage::fetch(c, build, cwd).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn remaining(snapshot: &Value, ms: i64) -> Option<f64> {
        allowance(snapshot, ms).map(|a| a.remaining)
    }
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
    fn earned_resets_are_an_explicit_count_not_quota_or_credits() {
        let mut report = json!({"rateLimits":{"credits":{"hasCredits":true}}});
        for (count, expected) in [
            (Value::Null, Value::Null),
            (json!(-1), Value::Null),
            (json!(0), json!(0)),
            (json!(3), json!(3)),
        ] {
            report["rateLimitResetCredits"] =
                json!({"availableCount":count,"credits":[{"id":"private"}]});
            let s = codex_snapshot(&report).unwrap();
            assert_eq!(s["earned_resets"], expected);
            assert!(!s.to_string().contains("private"));
            assert_eq!(remaining(&s, 0), None);
        }
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
    #[test]
    fn only_live_account_allowances_bound_admission() {
        let ms = 10_000;
        let mut s = json!({"windows":[
            {"kind":"model","used_pct":100},
            {"kind":"account","used_pct":100,"reset_at_ms":ms},
            {"kind":"account","used_pct":100,"reset_at_ms":"later"},
            {"kind":"account","reset_at_ms":ms+1},
            {"kind":"account","used_pct":-1},
            {"kind":"account","used_pct":"100"}
        ],"credits":{"balance":"0","has_credits":false}});
        assert_eq!(remaining(&s, ms), None);
        s["windows"].as_array_mut().unwrap().extend([
            json!({"kind":"account","used_pct":20}),
            json!({"kind":"account","used_pct":70.25,"reset_at_ms":ms+1}),
        ]);
        assert_eq!(remaining(&s, ms), Some(29.75));
        assert_eq!(remaining(&s, ms + 1), Some(80.));
        for (spend, expected) in [
            (json!({"remaining_pct":10.5}), 10.5),
            (json!({"remaining_pct":0}), 0.),
            (json!({"reached":true}), 0.),
            (json!({"reached":true,"reset_at_ms":ms}), 29.75),
            (json!({"reached":true,"reset_at_ms":false}), 29.75),
            (json!({"remaining_pct":-1,"reached":false}), 29.75),
        ] {
            s["spend_control"] = spend;
            assert_eq!(remaining(&s, ms), Some(expected));
        }
    }
    #[test]
    fn providers_do_not_turn_malformed_resets_into_unbounded_exhaustion() {
        let s=claude_snapshot(&json!({"five_hour":{"utilization":100,"resets_at":"bad"},"seven_day":{"utilization":20,"resets_at":null}})).unwrap();
        assert_eq!(remaining(&s, 1000), Some(80.));
        let s = codex_snapshot(&json!({"rateLimits":{
            "primary":{"usedPercent":100,"resetsAt":"bad"},"secondary":{"usedPercent":25},
            "individualLimit":{"remainingPercent":0,"resetsAt":false},"spendControlReached":true
        }}))
        .unwrap();
        assert_eq!(remaining(&s, 1000), Some(75.));
        let s = codex_snapshot(&json!({"rateLimitsByLimitId":{"codex":{
            "limitId":"model-only","primary":{"usedPercent":100},"spendControlReached":true
        }}}))
        .unwrap();
        assert_eq!(remaining(&s, 1000), None);
    }
}
