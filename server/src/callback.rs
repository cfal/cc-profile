use axum::body::Body;
use ccpd::{
    paths,
    types::{Error, Result},
};
use hyper::{Request, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{
    env,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    time::Duration,
};
use tokio::net::UnixStream;

async fn deliver(body: &[u8]) -> Result<()> {
    let dir = paths::runtime_dir()?;
    let m = std::fs::symlink_metadata(&dir)?;
    if !m.is_dir() || dir.canonicalize()? != dir || m.uid() != paths::uid() || m.mode() & 0o077 != 0
    {
        return Err(Error::denied());
    }
    let s = UnixStream::connect(dir.join("launch.sock")).await?;
    if s.peer_cred()?.uid() != paths::uid() {
        return Err(Error::denied());
    }
    let failed = || {
        Error::unavailable(
            "callback",
            "Unable to deliver the Codex session ID to ccpd.",
        )
    };
    let (mut sender, conn) = http1::handshake(TokioIo::new(s))
        .await
        .map_err(|_| failed())?;
    tokio::spawn(conn);
    let req = Request::post("/v1/observations")
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .map_err(|_| failed())?;
    let status = sender
        .send_request(req)
        .await
        .map_err(|_| failed())?
        .status();
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
