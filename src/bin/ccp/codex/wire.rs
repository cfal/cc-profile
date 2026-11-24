use super::super::failed;
use ccpd::types::{Error, Result};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

pub(super) const LIMIT: usize = 16 << 20;
pub(super) fn bad() -> Error {
    Error::bad("codex_protocol", "Invalid or oversized Codex stdio frame.")
}
pub(super) fn value(line: &[u8]) -> Result<Value> {
    if line.len() > LIMIT || !line.ends_with(b"\n") {
        return Err(bad());
    }
    let v: Value = serde_json::from_slice(line).map_err(|_| bad())?;
    if !v.is_object() {
        return Err(bad());
    }
    Ok(v)
}
pub(super) fn bytes(v: &Value) -> Result<Vec<u8>> {
    let mut b = serde_json::to_vec(v).map_err(|_| bad())?;
    b.push(b'\n');
    if b.len() > LIMIT {
        return Err(bad());
    }
    Ok(b)
}
pub(super) fn error(id: &Value, message: &str) -> Value {
    json!({"id":id,"error":{"code":-32001,"message":message}})
}
pub(super) async fn write(w: &mut (impl AsyncWrite + Unpin), line: &[u8]) -> Result<()> {
    w.write_all(line).await?;
    w.flush().await?;
    Ok(())
}
pub(super) async fn read(r: &mut BufReader<ChildStdout>) -> Result<Option<Vec<u8>>> {
    let mut b = Vec::new();
    if r.take((LIMIT + 1) as u64).read_until(b'\n', &mut b).await? == 0 {
        return Ok(None);
    }
    if b.len() > LIMIT || !b.ends_with(b"\n") {
        return Err(bad());
    }
    Ok(Some(b))
}

pub(super) struct Server {
    pub child: Child,
    pub input: ChildStdin,
    pub output: BufReader<ChildStdout>,
}
impl Server {
    pub fn spawn(mut cmd: Command) -> Result<Self> {
        let mut child = cmd
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            child,
            input,
            output,
        })
    }
    pub async fn exchange(&mut self, req: &Value) -> Result<Value> {
        timeout(Duration::from_secs(10), async {
            write(&mut self.input, &bytes(req)?).await?;
            // Notifications from a private handshake or utility worker do not
            // belong to the public session. Never replay a native request.
            for _ in 0..32 {
                let msg = value(&read(&mut self.output).await?.ok_or_else(bad)?)?;
                if msg.get("method").is_some() && msg.get("id").is_none() {
                    continue;
                }
                if msg.get("method").is_some()
                    || msg.get("id") != req.get("id")
                    || msg.get("result").is_some() == msg.get("error").is_some()
                {
                    return Err(bad());
                }
                return Ok(msg);
            }
            Err(bad())
        })
        .await
        .map_err(|_| failed("Timed out waiting for a Codex protocol reply."))?
    }
    pub async fn handshake(&mut self, init: &Value, home: &Path) -> Result<Value> {
        let reply = self.exchange(init).await?;
        if reply.get("error").is_none()
            && (reply["result"]["codexHome"].as_str().map(Path::new) != Some(home)
                || !reply["result"]["userAgent"].is_string())
        {
            return Err(failed(
                "Native Codex initialized with an unexpected home or protocol.",
            ));
        }
        Ok(reply)
    }
}
