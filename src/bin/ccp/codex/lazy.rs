use super::{child_command, command, failed, report, status, stdio};
use crate::{
    bind_session,
    child::{self, Incoming, Signals},
    record_failure, request, validate, validate_patch,
};
use ccpd::{
    invocation::{codex_stdio_command, native_id},
    paths,
    protocol::{CODEX_BROWSE, CODEX_CHECK, CODEX_LISTED, CODEX_OWNER, CodexListed, Plan, Request},
    types::{Error, Result},
};
use serde_json::{Value, json};
use std::{
    env, fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    time::Duration,
};
use tokio::{io::Stdout, sync::Mutex, time::Instant};
use uuid::Uuid;

#[path = "browse.rs"]
mod browse;
#[path = "frames.rs"]
mod frames;
#[path = "pending.rs"]
mod pending;
#[path = "wire.rs"]
mod wire;
use wire::{Server, bad, bytes, error, value, write};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn reject(msg: &'static str) -> Error {
    Error::bad("codex_routing", msg)
}
type Out = Mutex<Stdout>;
async fn send(out: &Out, v: &Value) -> Result<()> {
    write(&mut *out.lock().await, &bytes(v)?).await
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
async fn neutral(req: &Request, argv: &[String], init: &Value) -> Result<Value> {
    let runtime = paths::runtime_dir()?;
    paths::private_dir(&runtime)?;
    let public_home = runtime.join("codex-router-home");
    paths::private_dir(&public_home)?;
    let path = runtime.join(format!("codex-init-{}", Uuid::new_v4()));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    let scratch = Scratch(path);
    let binary = req.binary.as_ref().ok_or_else(bad)?;
    request::<()>(CODEX_CHECK, &bytes(&json!(binary))?, deadline()).await?;
    let mut cmd = child_command(binary);
    cmd.args(argv)
        .current_dir(&scratch.0)
        .env_clear()
        .env("HOME", &scratch.0)
        .env("CODEX_HOME", &scratch.0)
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .stderr(Stdio::null());
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env(key, "http://127.0.0.1:1");
    }
    let mut server = Server::spawn(cmd)?;
    let reply = server.handshake(init, &scratch.0).await;
    let _ = server.child.kill().await;
    let mut reply = reply?;
    if let Some(result) = reply.get_mut("result") {
        result["codexHome"] = json!(public_home);
    }
    Ok(reply)
}

fn pinned(base: &Request, id: &str) -> Request {
    let mut req = base.clone();
    req.argv.extend(["--for-session".into(), id.into()]);
    req
}
fn source(call: &mut Value) -> Result<Option<String>> {
    let method = call["method"].as_str().unwrap_or("");
    if !matches!(
        method,
        "thread/resume"
            | "thread/fork"
            | "thread/read"
            | "thread/turns/list"
            | "thread/items/list"
            | "thread/turns/items/list"
    ) {
        return Ok(None);
    }
    let params = call["params"].as_object_mut().ok_or_else(bad)?;
    let id = native_id(params.get("threadId").and_then(Value::as_str).unwrap_or(""))?;
    if params.get("history").is_some_and(|v| !v.is_null()) {
        return Err(reject(
            "Managed Codex restore requires a thread ID, not inline history.",
        ));
    }
    // In native Codex a path takes precedence over the ID. A client-supplied
    // path must never turn a sticky admission into a read from a different home.
    params.remove("path");
    params.insert("threadId".into(), Value::String(id.clone()));
    Ok(Some(id))
}

struct Connection {
    base: Request,
    argv: Vec<String>,
    init: Value,
    ready: Option<Value>,
    hello: Value,
}
impl Connection {
    async fn confirm_page(&self, req: &Request, owner: i64, reply: &Value) -> Result<()> {
        let rows = reply["result"]["data"]
            .as_array()
            .filter(|a| a.len() <= 2048)
            .ok_or_else(bad)?;
        let threads = rows
            .iter()
            .map(|r| {
                Ok((
                    owner,
                    r["id"].as_str().ok_or_else(bad)?.into(),
                    r["cwd"].as_str().ok_or_else(bad)?.into(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let body = CodexListed {
            bound: Some((req.request_id.clone(), req.ticket.clone())),
            binary: req.binary.clone().ok_or_else(bad)?,
            profiles: Vec::new(),
            threads,
        };
        let count: usize = request(
            CODEX_LISTED,
            &serde_json::to_vec(&body).map_err(|_| bad())?,
            deadline(),
        )
        .await?;
        if count != rows.len() {
            return Err(bad());
        }
        Ok(())
    }
    async fn connected(&self, plan: &Plan) -> Result<Server> {
        let bin = self.base.binary.as_ref().ok_or_else(bad)?;
        let home = Path::new(plan.env_set.get("CODEX_HOME").ok_or_else(bad)?);
        let mut server = Server::spawn(command(bin, &self.argv, plan))?;
        let mut reply = server.handshake(&self.init, home).await?;
        if let Some(result) = reply.get_mut("result") {
            result["codexHome"] = self.hello["result"]["codexHome"].clone();
        }
        if reply.get("result") != self.hello.get("result") {
            return Err(failed(
                "The selected Codex server could not replay the client handshake.",
            ));
        }
        if let Some(ready) = &self.ready {
            write(&mut server.input, &bytes(ready)?).await?;
        }
        Ok(server)
    }
    async fn plans(&self, all: bool) -> Result<Vec<(Plan, i64)>> {
        let plans: Vec<(Plan, i64)> =
            request(CODEX_BROWSE, &bytes(&json!([self.base, all]))?, deadline()).await?;
        if plans.len() > 32 || (!all && plans.len() != 1) {
            return Err(bad());
        }
        for (plan, revision) in &plans {
            validate_patch(plan, &self.base)?;
            if plan.config_id <= 0
                || *revision <= 0
                || plan.decision != "browse"
                || plan.argv != self.base.argv
            {
                return Err(bad());
            }
        }
        Ok(plans)
    }
    async fn utility(&self, call: &Value) -> Result<Value> {
        let plans = self.plans(false).await?;
        let reply = self.connected(&plans[0].0).await?.exchange(call).await?;
        if self.plans(false).await? != plans {
            return Err(reject(
                "The default Codex profile changed. Retry the request.",
            ));
        }
        Ok(reply)
    }
    async fn owner(&self, id: &str) -> Result<i64> {
        request(
            CODEX_OWNER,
            &serde_json::to_vec(&pinned(&self.base, id)).map_err(|_| bad())?,
            deadline(),
        )
        .await
    }
    async fn bind(&self, call: &Value, id: Option<&str>) -> Result<(Request, Plan, Server)> {
        let cwd = if let Some(cwd) = call["params"].get("cwd").filter(|v| !v.is_null()) {
            let cwd = cwd
                .as_str()
                .filter(|p| paths::workspace(p))
                .ok_or_else(bad)?;
            Some(Path::new(cwd))
        } else {
            None
        };
        let (req, plan) = bind_session(&self.base, id, cwd).await?;
        let (home, managed) = validate(&plan, &req)?;
        if managed.is_none() {
            return Err(bad());
        }
        if env::var_os("CC_PROFILE_DEBUG").as_deref() == Some("1".as_ref()) {
            eprintln!("ccp: codex/{} -> {home}", plan.label);
        }
        match self.connected(&plan).await {
            Ok(server) => Ok((req, plan, server)),
            Err(e) => {
                record_failure(&req, libc::EIO).await;
                Err(e)
            }
        }
    }
    async fn bound(
        &self,
        mut rx: Incoming,
        out: &Out,
        bound: Bound,
        sig: &mut Signals,
    ) -> Result<ExitCode> {
        let Bound {
            first,
            req,
            plan,
            server,
        } = bound;
        let Server {
            mut child,
            mut input,
            mut output,
        } = server;
        let pending = std::cell::RefCell::new(pending::Requests::default());
        // Ephemeral threads have no durable owner. They can be read only on
        // the connection on which native Codex actually started them.
        let ephemeral = std::cell::RefCell::new(std::collections::HashSet::new());
        let reader = async {
            while let Some(line) = frames::bound_output(&mut output, out, &pending).await? {
                let msg: pending::Envelope = serde_json::from_slice(&line).map_err(|_| bad())?;
                if msg.method.as_deref() == Some("thread/closed")
                    && msg.id.is_null()
                    && let Some(id) = value(&line)?["params"]["threadId"].as_str()
                {
                    ephemeral.borrow_mut().remove(id);
                }
                let kind = if msg.method.is_none() {
                    pending.borrow_mut().remove(&msg.id)
                } else {
                    None
                };
                match kind {
                    Some(pending::Kind::List) => {
                        let reply = value(&line)?;
                        if reply.get("error").is_some() == reply.get("result").is_some() {
                            return Err(bad());
                        }
                        if reply.get("error").is_none()
                            && let Err(e) = self.confirm_page(&req, plan.config_id, &reply).await
                        {
                            send(out, &error(&msg.id, &e.2)).await?;
                            continue;
                        }
                    }
                    Some(pending::Kind::Claim { source, .. }) => {
                        if let Some((event, thread)) = stdio::started_claim(&line, source) {
                            if thread.ephemeral {
                                if ephemeral.borrow().len() >= pending::CAP {
                                    send(out, &error(&msg.id, pending::EPHEMERAL_FULL)).await?;
                                    continue;
                                }
                                ephemeral.borrow_mut().insert(thread.id);
                            } else if report(&req, event, &thread).await.is_err() {
                                send(out, &error(&msg.id, "Native Codex created a thread but CCP could not confirm its owner. Do not automatically retry.")).await?;
                                continue;
                            }
                        }
                    }
                    _ => {}
                }
                write(&mut *out.lock().await, &line).await?;
            }
            Ok::<_, Error>(())
        };
        let writer = async {
            pending.borrow_mut().admit(&value(&first)?, 0)?;
            write(&mut input, &first).await?;
            while let Some(mut line) = frames::client(&mut rx, out).await? {
                let mut call = value(&line)?;
                if matches!(
                    call["method"].as_str(),
                    Some("thread/start" | "thread/fork" | "thread/resume")
                ) {
                    let check = json!({"request_id":req.request_id,"ticket":req.ticket,"event":"admit_new",
                        "source":"app-server","native_id":""});
                    if request::<Value>("/v2/observations", &bytes(&check)?, deadline())
                        .await
                        .is_err()
                    {
                        send(out,&error(pending::error_id(&call),"This Codex profile changed or is unavailable. Open a new connection.")).await?;
                        continue;
                    }
                }
                match source(&mut call) {
                    Ok(Some(id))
                        if ephemeral.borrow().contains(&id)
                            && !matches!(
                                call["method"].as_str(),
                                Some("thread/resume" | "thread/fork")
                            ) =>
                    {
                        line = bytes(&call)?
                    }
                    Ok(Some(id)) => match self.owner(&id).await {
                        Ok(owner) if owner == plan.config_id => line = bytes(&call)?,
                        _ => {
                            send(out,&error(pending::error_id(&call),"Thread is not available on this connection. Open a new connection for its owner.")).await?;
                            continue;
                        }
                    },
                    Err(e) => {
                        send(out, &error(pending::error_id(&call), &e.2)).await?;
                        continue;
                    }
                    _ => {}
                }
                let admission = pending.borrow_mut().admit(&call, ephemeral.borrow().len());
                if let Err(e) = admission {
                    send(out, &error(pending::error_id(&call), &e.2)).await?;
                    continue;
                }
                write(&mut input, &line).await?;
            }
            drop(input);
            Ok::<_, Error>(())
        };
        let pump = async {
            tokio::pin!(reader, writer);
            tokio::select! {
                r = &mut reader => r,
                w = &mut writer => { w?; reader.await }
            }
        };
        tokio::select! {
            r = pump => r?,
            s = sig.recv() => {
                child::stop(&mut child,s).await;
                return Ok(ExitCode::from((128+s) as u8));
            }
        }
        tokio::select! {
            r=child.wait()=>Ok(status(r?)),
            s=sig.recv()=>{child::stop(&mut child,s).await; Ok(ExitCode::from((128+s) as u8))}
        }
    }
}

pub(crate) async fn run(req: Request) -> Result<ExitCode> {
    let mut sig = Signals::new()?;
    let out = Mutex::new(tokio::io::stdout());
    let result = tokio::select! {
        s=sig.recv()=>return Ok(ExitCode::from((128+s) as u8)),
        result=route(req,&out)=>result?,
    };
    match result {
        Some((conn, rx, bound)) => conn.bound(rx, &out, bound, &mut sig).await,
        None => Ok(ExitCode::SUCCESS),
    }
}
struct Bound {
    first: Vec<u8>,
    req: Request,
    plan: Plan,
    server: Server,
}
async fn route(req: Request, out: &Out) -> Result<Option<(Connection, Incoming, Bound)>> {
    let bin = req
        .binary
        .as_ref()
        .ok_or_else(ccpd::native::unsupported_build)?;
    request::<()>(CODEX_CHECK, &bytes(&json!(bin))?, deadline()).await?;
    let (_, argv) = codex_stdio_command(&req.argv)?;
    let mut rx = child::incoming("codex-stdin", wire::LIMIT)?;
    let mut conn: Option<Connection> = None;
    let mut browse = browse::Browse::default();
    while let Some(line) = frames::client(&mut rx, out).await? {
        let mut call = value(&line)?;
        let id = call["id"].clone();
        let method = call["method"].as_str().ok_or_else(bad)?;
        if id.is_null() {
            if method == "initialized"
                && let Some(c) = &mut conn
                && c.ready.is_none()
            {
                c.ready = Some(call);
            }
            continue;
        }
        if !pending::valid_id(&id) {
            return Err(bad());
        }
        if method == "initialize" && conn.is_none() {
            let reply = match neutral(&req, &argv, &call).await {
                Ok(reply) => reply,
                Err(e) => {
                    send(out, &error(&id, &e.2)).await?;
                    return Err(e);
                }
            };
            send(out, &reply).await?;
            if reply.get("result").is_some() {
                conn = Some(Connection {
                    base: req.clone(),
                    argv: argv.clone(),
                    init: call,
                    ready: None,
                    hello: reply,
                });
            }
            continue;
        }
        let Some(c) = conn.as_ref() else {
            send(out, &error(&id, "Initialize Codex first.")).await?;
            continue;
        };
        let is_new = method == "thread/start";
        if method == "thread/loaded/list" {
            let reply = if call["params"]["cursor"].is_null() {
                json!({"id":id,"result":{"data":[],"nextCursor":null}})
            } else {
                error(
                    &id,
                    "This unbound connection has no loaded threads or cursor.",
                )
            };
            send(out, &reply).await?;
            continue;
        }
        if method == "thread/list" {
            let reply = match browse.list(c, &call).await {
                Ok(result) => json!({"id": id, "result": result}),
                Err(e) => error(&id, &e.2),
            };
            send(out, &reply).await?;
            continue;
        }
        if matches!(
            method,
            "model/list"
                | "skills/list"
                | "account/read"
                | "account/rateLimits/read"
                | "config/read"
        ) {
            let reply = match c.utility(&call).await {
                Ok(v) => v,
                Err(e) => error(&id, &e.2),
            };
            send(out, &reply).await?;
            continue;
        }
        let sid = match source(&mut call) {
            Ok(s) if s.is_some() || is_new => s,
            Err(e) => {
                send(out, &error(&id, &e.2)).await?;
                continue;
            }
            _ => {
                send(
                    out,
                    &error(&id, "Start or select a Codex thread before this operation."),
                )
                .await?;
                continue;
            }
        };
        match c.bind(&call, sid.as_deref()).await {
            Ok((req, plan, server)) => {
                return Ok(Some((
                    conn.take().unwrap(),
                    rx,
                    Bound {
                        first: bytes(&call)?,
                        req,
                        plan,
                        server,
                    },
                )));
            }
            Err(e) => send(out, &error(&id, &e.2)).await?,
        }
    }
    Ok(None)
}
