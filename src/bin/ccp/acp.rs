use super::child::{Signals, status, stop};
use ccpd::{
    agy,
    invocation::Invocation,
    protocol::{Plan, Request},
    socket,
    types::{Error, Result},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, BufRead, Read},
    path::Path,
    process::{ExitCode, Stdio},
    sync::Mutex,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};

const FRAME: usize = 8 << 20;
enum Reply {
    Initialize,
    NewSession(String),
    Forward,
}
type Pending = Mutex<HashMap<String, Reply>>;
type Out = tokio::sync::Mutex<tokio::io::Stdout>;

fn bad() -> Error {
    Error::bad(
        "acp_protocol",
        "Invalid or oversized ACP frame; closing connection.",
    )
}
fn parse(bytes: &[u8]) -> Result<Value> {
    if bytes.len() > FRAME || !bytes.ends_with(b"\n") {
        return Err(bad());
    }
    let v: Value = serde_json::from_slice(bytes).map_err(|_| bad())?;
    if !v.is_object() || v["jsonrpc"] != "2.0" {
        return Err(bad());
    }
    let result = v.get("result").is_some();
    let error = v.get("error").is_some();
    match v.get("method") {
        Some(m) if m.is_string() && !result && !error => {}
        None if !v["id"].is_null() && result != error => {}
        _ => return Err(bad()),
    }
    Ok(v)
}
fn key(id: &Value) -> Result<String> {
    if !(id.is_i64() || id.is_u64() || id.as_str().is_some_and(|s| s.len() <= 256)) {
        return Err(bad());
    }
    Ok(id.to_string())
}
async fn emit(out: &Out, bytes: &[u8]) -> Result<()> {
    let mut out = out.lock().await;
    out.write_all(bytes).await?;
    out.flush().await?;
    Ok(())
}
async fn reject(out: &Out, id: &Value, message: &str) -> Result<()> {
    let mut bytes = serde_json::to_vec(
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":message}}),
    )
    .map_err(|_| bad())?;
    bytes.push(b'\n');
    emit(out, &bytes).await
}
async fn record(req: &Request, event: &str, session: &str, workspace: Option<&str>) -> Result<()> {
    let data=json!({"request_id":req.request_id,"ticket":req.ticket,"source":"acp","event":event,"native_id":session,"workspace":workspace}).to_string();
    if matches!(tokio::time::timeout(Duration::from_secs(3),socket::post("/v2/observations",data.as_bytes())).await,Ok(Ok((status,_))) if status.is_success())
    {
        Ok(())
    } else {
        Err(Error::conflict(
            "acp_owner",
            "Cannot confirm ACP session ownership for this profile.",
        ))
    }
}

// A blocking terminal read must not keep the Tokio runtime alive after the agent exits.
fn incoming() -> Result<mpsc::Receiver<io::Result<Vec<u8>>>> {
    let (tx, rx) = mpsc::channel(1);
    std::thread::Builder::new()
        .name("acp-stdin".into())
        .spawn(move || {
            let mut reader = io::stdin().lock();
            loop {
                let mut line = Vec::new();
                match (&mut reader)
                    .take(FRAME as u64 + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.blocking_send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e));
                        break;
                    }
                }
            }
        })?;
    Ok(rx)
}
async fn input(
    req: &Request,
    pending: &Pending,
    out: &Out,
    mut pipe: tokio::process::ChildStdin,
) -> Result<()> {
    let mut rx = incoming()?;
    while let Some(bytes) = rx.recv().await {
        let bytes = bytes?;
        let v = parse(&bytes)?;
        if let Some(method) = v["method"].as_str() {
            let id = &v["id"];
            let mut reply = Reply::Forward;
            if !id.is_null() && pending.lock().unwrap().contains_key(&key(id)?) {
                return Err(bad());
            }
            match method {
                "initialize" => {
                    key(id)?;
                    reply = Reply::Initialize;
                }
                "session/new" => {
                    key(id)?;
                    let ws = v["params"]["cwd"]
                        .as_str()
                        .filter(|s| {
                            Path::new(s).is_absolute()
                                && s.len() <= 4096
                                && !s.chars().any(char::is_control)
                        })
                        .ok_or_else(bad)?;
                    if record(req, "admit_new", "", None).await.is_err() {
                        reject(out, id, "This profile is unavailable for new sessions.").await?;
                        continue;
                    }
                    reply = Reply::NewSession(ws.to_owned());
                }
                "session/load" | "session/resume" => {
                    key(id)?;
                    let session = v["params"]["sessionId"].as_str().ok_or_else(bad)?;
                    if agy::session_id(session).is_err()
                        || record(req, method, session, None).await.is_err()
                    {
                        reject(out,id,"Unknown, unavailable, or different profile owns this session. Start an owner-pinned connection.").await?;
                        continue;
                    }
                }
                "session/fork" => {
                    key(id)?;
                    reject(out, id, "Managed ACP does not support forks.").await?;
                    continue;
                }
                "authenticate" => {
                    key(id)?;
                    reject(
                        out,
                        id,
                        "Sign in with ccp login <profile-id> in your terminal.",
                    )
                    .await?;
                    continue;
                }
                _ => {}
            }
            if !id.is_null() {
                let mut p = pending.lock().unwrap();
                if p.len() >= 1024 {
                    return Err(bad());
                }
                p.insert(key(id)?, reply);
            }
        }
        pipe.write_all(&bytes).await?;
        pipe.flush().await?;
    }
    Ok(())
}
async fn output(
    req: &Request,
    pending: &Pending,
    out: &Out,
    pipe: tokio::process::ChildStdout,
) -> Result<()> {
    let mut reader = BufReader::new(pipe);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if (&mut reader)
            .take(FRAME as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .await?
            == 0
        {
            return Ok(());
        }
        let mut v = parse(&bytes)?;
        if v.get("method").is_none() {
            let reply = pending
                .lock()
                .unwrap()
                .remove(&key(&v["id"])?)
                .ok_or_else(bad)?;
            if v.get("error").is_none() {
                match reply {
                    Reply::NewSession(ws) => {
                        let id =
                            agy::session_id(v["result"]["sessionId"].as_str().ok_or_else(bad)?)?;
                        // The harness can reconnect to resume as soon as it receives this reply.
                        record(req, "session/new", id, Some(&ws)).await?;
                    }
                    Reply::Initialize => {
                        // A harness must not execute native login outside the selected profile.
                        let result = v["result"].as_object_mut().ok_or_else(bad)?;
                        result.insert("authMethods".into(), json!([]));
                        bytes = serde_json::to_vec(&v).map_err(|_| bad())?;
                        bytes.push(b'\n');
                    }
                    Reply::Forward => {}
                }
            }
        }
        emit(out, &bytes).await?;
    }
}

pub(super) async fn run(
    binary: &Path,
    home: &Path,
    req: &Request,
    plan: &Plan,
    managed: Option<&Invocation>,
) -> Result<ExitCode> {
    let saved = agy::has_login(home)?;
    if let Some(id) = managed.and_then(Invocation::agy_conversation)
        && !uuid::Uuid::parse_str(id)
            .ok()
            .is_some_and(|id| agy::has_conversation(home, id).unwrap_or(false))
    {
        return Err(Error::bad(
            "conversation_missing",
            "Saved conversation is no longer available in the selected profile.",
        ));
    }
    if req.login_profile.is_some() && req.argv == agy::STATUS {
        println!(
            "{}",
            if saved {
                "Local OAuth file present (validity not checked)."
            } else {
                "No local OAuth file."
            }
        );
        return Ok(if saved {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
    }
    let profile_usage = req.login_profile.is_some() && req.argv == agy::PROFILE_USAGE;
    if profile_usage && !saved {
        return Err(Error::bad(
            "login_required",
            "Sign in to this profile before checking quota.",
        ));
    }
    let tui = managed.is_some_and(|i| i.agy_tui);
    let bridge = managed.is_some() && !tui;
    let mut args = if profile_usage {
        agy::usage_args(false)
    } else {
        req.argv.clone()
    };
    if bridge && let Some(i) = args.iter().position(|s| s == "--for-session") {
        args.drain(i..i + 2);
    }
    let rt = agy::Runtime::new()?;
    let mut cmd = Command::new(binary);
    cmd.envs(&plan.env_set);
    for k in &plan.env_unset {
        cmd.env_remove(k);
    }
    rt.configure(&mut cmd, home, &args);
    if bridge {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    }
    if profile_usage {
        cmd.current_dir(home)
            .stdin(Stdio::null())
            .env("BROWSER", "/bin/false");
    }
    let mut signals = Signals::new()?;
    let mut child = cmd.spawn()?;
    let pump = async {
        if bridge {
            let pending = Pending::default();
            let out = Out::new(tokio::io::stdout());
            let input = input(req, &pending, &out, child.stdin.take().unwrap());
            let output = output(req, &pending, &out, child.stdout.take().unwrap());
            tokio::pin!(output);
            tokio::select! {
                r=input=>{r?;output.await?;}
                r=&mut output=>r?,
            }
        }
        Ok(status(child.wait().await?))
    };
    let shutdown = async {
        loop {
            let sig = signals.recv().await;
            // A foreground TUI already receives terminal Ctrl+C with its launcher.
            if !(tui
                && sig == libc::SIGINT
                && unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp() })
            {
                return sig;
            }
        }
    };
    tokio::select! {
        r=pump=>r,
        s=shutdown=>{stop(&mut child,s).await;Ok(ExitCode::from((128+s) as u8))}
    }
}
