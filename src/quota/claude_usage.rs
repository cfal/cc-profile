use super::{claude_snapshot, display, signin, unavailable};
use crate::{
    native::{Build, clean_command},
    store::Config,
    types::Result,
};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LIMIT: usize = 1024 * 1024;
// Only SDK control requests, never a user message. Skip the native seven-day transcript scan.
const REQUESTS: &[u8] = concat!(
    "{\"type\":\"control_request\",\"request_id\":\"init\",\"request\":{\"subtype\":\"initialize\"}}\n",
    "{\"type\":\"control_request\",\"request_id\":\"usage\",\"request\":{\"subtype\":\"get_usage\",\"skip_behaviors\":true}}\n",
).as_bytes();

fn report(bytes: &[u8]) -> Result<Value> {
    let mut initialized = false;
    let mut snapshot = None;
    for line in bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_slice(line).map_err(|_| unavailable())?;
        if v["type"] != "control_response" {
            continue;
        }
        let r = &v["response"];
        match r["request_id"].as_str() {
            Some("init") if !initialized && r["subtype"] == "success" => initialized = true,
            Some("usage") if initialized && snapshot.is_none() && r["subtype"] == "success" => {
                let data = &r["response"];
                if data["rate_limits_available"] != true {
                    return Err(unavailable());
                }
                let mut s = claude_snapshot(&data["rate_limits"])?;
                if s["windows"].as_array().is_none_or(Vec::is_empty) {
                    return Err(unavailable());
                }
                s["plan"] = json!(display(&data["subscription_type"]));
                snapshot = Some(s);
            }
            Some("init" | "usage") => return Err(unavailable()),
            _ => {}
        }
    }
    snapshot.ok_or_else(unavailable)
}

pub(super) async fn fetch(c: &Config, build: &Build, cwd: &Path) -> Result<Value> {
    if !build.supported || !build.unchanged() {
        return Err(unavailable());
    }
    // Only use the registered file-backed profile. The CLI, not ccpd, reads its credentials.
    if !c
        .home
        .join(".credentials.json")
        .symlink_metadata()
        .is_ok_and(|m| m.is_file())
    {
        return Err(signin());
    }
    let mut cmd = clean_command(&build.bin, cwd, Some(c));
    let mut child = cmd
        .args([
            "--print",
            "--verbose",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--no-session-persistence",
            "--setting-sources",
            "",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--tools",
            "",
        ])
        // CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC also suppresses the usage lookup.
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(REQUESTS).await?;
        drop(stdin);
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
        report(&bytes)
    })
    .await
    .map_err(|_| unavailable())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_correlated_subscription_report_is_projected() {
        let response = |id, data: Value| {
            json!({"type":"control_response", "response":{
            "subtype":"success","request_id":id,"response":data}})
        };
        let init = response("init", json!({}));
        let data = json!({"rate_limits_available":true, "subscription_type":"max","session":{"private":"secret"},
        "rate_limits":{"five_hour":{"utilization":0}, "model_scoped":[
            {"display_name":"Fable","utilization":100,"resets_at":"2030-01-01T00:00:00Z"},
            {"display_name":"Bad\nlabel","utilization":9}
        ]}});
        let usage = response("usage", data.clone());
        let wire = |frames: &[&Value]| frames.iter().map(|v| format!("{v}\n")).collect::<String>();
        let s = report(wire(&[&init, &usage]).as_bytes()).unwrap();
        assert_eq!(
            s["windows"],
            json!([
            {"kind":"account","label":"5h","used_pct":0.0,"reset_at_ms":null},
            {"kind":"model","label":"7d Fable","used_pct":100.0,"reset_at_ms":1893456000000_i64}])
        );
        assert_eq!(s["plan"], "max");
        assert!(!s.to_string().contains("secret"));
        for frames in [
            vec![&usage],
            vec![&usage, &init],
            vec![&init],
            vec![&init, &usage, &usage],
        ] {
            assert!(report(wire(&frames).as_bytes()).is_err());
        }
        for bad in [
            response("other", data),
            response(
                "usage",
                json!({"rate_limits_available":true, "rate_limits":null}),
            ),
            response(
                "usage",
                json!({"rate_limits_available":false,"rate_limits":{"five_hour":{"utilization":100}}}),
            ),
            response(
                "usage",
                json!({"rate_limits_available":true,"rate_limits":{"unrecognized":{}}}),
            ),
        ] {
            assert!(report(wire(&[&init, &bad]).as_bytes()).is_err());
        }
    }
}
