use ccpd::{
    socket,
    types::{Error, Result},
};
use serde_json::{Value, json};
use std::{
    env,
    io::{self, Read},
    time::Duration,
};

async fn deliver(body: &[u8]) -> Result<()> {
    let (status, _) = socket::post("/v1/observations", body).await?;
    let failed = || {
        Error::unavailable(
            "callback",
            "Unable to deliver the Codex session ID to ccpd.",
        )
    };
    if status.is_client_error() {
        return Err(Error::bad(
            "rejected",
            "Codex session observation refused; check Startups.",
        ));
    }
    if !status.is_success() {
        return Err(failed());
    }
    Ok(())
}
pub async fn run() -> Result<()> {
    let (Ok(id), Ok(ticket)) = (env::var("CCP_LAUNCH_ID"), env::var("CCP_OBSERVE_TICKET")) else {
        return Ok(());
    };
    let mut input = Vec::new();
    io::stdin().take(65537).read_to_end(&mut input)?;
    if input.len() > 65536 {
        return Err(Error::bad("hook_input", "Hook event too large."));
    }
    let v: Value = serde_json::from_slice(&input)
        .map_err(|_| Error::bad("hook_input", "Invalid hook event."))?;
    if v["hook_event_name"] != "SessionStart"
        || v["source"] != "startup"
        || !v["agent_id"].is_null()
    {
        return Ok(());
    }
    let body=json!({"request_id":id,"ticket":ticket,"event":"SessionStart","source":"startup","native_id":v["session_id"]}).to_string();
    for _ in 0..2 {
        match tokio::time::timeout(Duration::from_secs(2), deliver(body.as_bytes())).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) if e.0.is_client_error() => return Err(e),
            _ => {}
        }
    }
    Err(Error::unavailable(
        "not_observed",
        "Codex session ID was not recorded; managed resume may be unavailable.",
    ))
}
