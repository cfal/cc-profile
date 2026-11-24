pub use crate::paths::workspace;
use crate::types::{Error, Result};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
};

pub const FRAME: usize = 8 << 20;
pub const MAX_LIST_ROWS: usize = 2048;

fn capabilities() -> Value {
    json!({"loadSession":true,"sessionCapabilities":{"resume":{},"list":{},"close":{}},
            "promptCapabilities":{"image":true,"embeddedContext":true}})
}
pub fn initialize_result(version: &str) -> Value {
    json!({"protocolVersion":1,"agentInfo":{"name":"CCP managed Antigravity","version":version},
        "agentCapabilities":capabilities(),"authMethods":[]})
}

fn supports(native: &Value, promised: &Value) -> bool {
    match promised.as_object() {
        Some(fields) => native.is_object() && fields.iter().all(|(k, v)| supports(&native[k], v)),
        None => native == promised,
    }
}

pub fn check_initialize(reply: &Value) -> Result<()> {
    if reply["protocolVersion"] != 1 || !supports(&reply["agentCapabilities"], &capabilities()) {
        return Err(Error::conflict(
            "acp_version",
            "Native AgY does not support the managed ACP handshake.",
        ));
    }
    Ok(())
}

pub async fn write(pipe: &mut ChildStdin, value: &Value) -> Result<()> {
    pipe.write_all(value.to_string().as_bytes()).await?;
    pipe.write_all(b"\n").await?;
    pipe.flush().await?;
    Ok(())
}

// No callbacks or client requests are outstanding during a private exchange.
pub async fn exchange(
    pipe: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    msg: &Value,
) -> Result<Value> {
    let bad = || Error::unavailable("acp_reply", "AgY did not return the expected ACP reply.");
    write(pipe, msg).await?;
    let mut line = Vec::new();
    reader
        .take(FRAME as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if line.len() > FRAME || !line.ends_with(b"\n") {
        return Err(bad());
    }
    let mut v: Value = serde_json::from_slice(&line).map_err(|_| bad())?;
    if v["jsonrpc"] != "2.0"
        || v["id"] != msg["id"]
        || v.get("method").is_some()
        || v.get("error").is_some()
        || !v["result"].is_object()
    {
        return Err(bad());
    }
    Ok(v["result"].take())
}
