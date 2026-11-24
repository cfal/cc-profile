use super::{command, failed, report, status, stdio};
use crate::{
    child::{self, Incoming, Signals},
    launch, record_failure, request, validate, validate_patch,
};
use ccpd::{
    invocation::{codex_stdio_command, native_id},
    paths,
    protocol::{CODEX_BROWSE, CODEX_CHECK, CODEX_OWNER, Plan, Request},
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
use tokio::{io::Stdout, process::Command, sync::Mutex, time::Instant};
use uuid::Uuid;

#[path = "wire.rs"]
mod wire;
use wire::{Server, bad, bytes, error, read, value, write};

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
    let mut cmd = Command::new(req.binary.as_ref().ok_or_else(bad)?);
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
    async fn connected(&self, plan: &Plan) -> Result<Server> {
        let bin = self.base.binary.as_ref().ok_or_else(bad)?;
        let home = Path::new(plan.env_set.get("CODEX_HOME").ok_or_else(bad)?);
        let mut server = Server::spawn(command(bin, &self.argv, plan))?;
        let reply = server.handshake(&self.init, home).await?;
        if reply.get("error").is_some()
            || reply["result"]["userAgent"] != self.hello["result"]["userAgent"]
        {
            return Err(failed(
                "The selected Codex server could not replay the client handshake.",
            ));
        }
        if let Some(ready) = &self.ready {
            write(&mut server.input, &bytes(ready)?).await?;
        }
        Ok(server)
    }
    async fn plans(&self, all: bool) -> Result<Vec<Plan>> {
        let plans: Vec<Plan> =
            request(CODEX_BROWSE, &bytes(&json!([self.base, all]))?, deadline()).await?;
        if plans.len() > 32 || (!all && plans.len() != 1) {
            return Err(bad());
        }
        for plan in &plans {
            validate_patch(plan, &self.base)?;
            if plan.config_id <= 0 || plan.decision != "browse" || plan.argv != self.base.argv {
                return Err(bad());
            }
        }
        Ok(plans)
    }
    async fn utility(&self, call: &Value) -> Result<Value> {
        let plans = self.plans(false).await?;
        self.connected(&plans[0]).await?.exchange(call).await
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
        let mut req = id.map_or_else(|| self.base.clone(), |id| pinned(&self.base, id));
        req.request_id = Uuid::new_v4().to_string();
        req.ticket = paths::secret()?;
        if let Some(cwd) = call["params"].get("cwd").filter(|v| !v.is_null()) {
            let cwd = cwd
                .as_str()
                .filter(|p| {
                    Path::new(p).is_absolute()
                        && p.len() <= 4096
                        && !p.chars().any(char::is_control)
                })
                .ok_or_else(bad)?;
            req.cwd = cwd.into();
        }
        let plan = launch(&req, deadline()).await?;
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
        first: Vec<u8>,
        req: &Request,
        plan: &Plan,
        server: Server,
    ) -> Result<ExitCode> {
        let Server {
            mut child,
            mut input,
            mut output,
        } = server;
        let starts = stdio::Starts::default();
        let reader = async {
            while let Some(line) = read(&mut output).await? {
                if let Some(thread) = stdio::started(&line, &starts)
                    && let Err(e) = report(req, "thread/start", &thread).await
                {
                    eprintln!("ccp: {e}");
                }
                write(&mut *out.lock().await, &line).await?;
            }
            Ok::<_, Error>(())
        };
        let writer = async {
            stdio::track(&first, &starts);
            write(&mut input, &first).await?;
            while let Some(line) = rx.recv().await {
                let mut line = line?;
                let mut call = value(&line)?;
                match source(&mut call) {
                    Ok(Some(id)) => match self.owner(&id).await {
                        Ok(owner) if owner == plan.config_id => line = bytes(&call)?,
                        _ => {
                            send(out,&error(&call["id"],"Thread is not available on this connection. Open a new connection for its owner.")).await?;
                            continue;
                        }
                    },
                    Err(e) => {
                        send(out, &error(&call["id"], &e.2)).await?;
                        continue;
                    }
                    _ => {}
                }
                stdio::track(&line, &starts);
                write(&mut input, &line).await?;
            }
            drop(input);
            Ok::<_, Error>(())
        };
        tokio::pin!(reader, writer);
        tokio::select! {
            r = &mut reader => r?,
            w = &mut writer => {w?; reader.await?;}
        }
        Ok(status(child.wait().await?))
    }
}

pub(crate) async fn run(req: Request) -> Result<ExitCode> {
    let mut sig = Signals::new()?;
    tokio::select! {
        s=sig.recv()=>Ok(ExitCode::from((128+s) as u8)),
        result=route(req)=>result,
    }
}
async fn route(req: Request) -> Result<ExitCode> {
    let bin = req
        .binary
        .as_ref()
        .ok_or_else(ccpd::native::unsupported_build)?;
    request::<()>(CODEX_CHECK, &bytes(&json!(bin))?, deadline()).await?;
    let (_, argv) = codex_stdio_command(&req.argv)?;
    let mut rx = child::incoming("codex-stdin", wire::LIMIT)?;
    let out = Mutex::new(tokio::io::stdout());
    let mut conn: Option<Connection> = None;
    while let Some(line) = rx.recv().await {
        let mut call = value(&line?)?;
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
        if !(id.is_i64() || id.as_str().is_some_and(|s| s.len() <= 256)) {
            return Err(bad());
        }
        if method == "initialize" && conn.is_none() {
            let reply = neutral(&req, &argv, &call).await?;
            send(&out, &reply).await?;
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
            send(&out, &error(&id, "Initialize Codex first.")).await?;
            continue;
        };
        let is_new = method == "thread/start";
        if matches!(
            method,
            "model/list"
                | "skills/list"
                | "thread/list"
                | "account/read"
                | "account/rateLimits/read"
                | "config/read"
        ) {
            let reply = match c.utility(&call).await {
                Ok(v) => v,
                Err(e) => error(&id, &e.2),
            };
            send(&out, &reply).await?;
            continue;
        }
        let sid = match source(&mut call) {
            Ok(s) if s.is_some() || is_new => s,
            Err(e) => {
                send(&out, &error(&id, &e.2)).await?;
                continue;
            }
            _ => {
                send(
                    &out,
                    &error(&id, "Start or select a Codex thread before this operation."),
                )
                .await?;
                continue;
            }
        };
        match c.bind(&call, sid.as_deref()).await {
            Ok((req, plan, server)) => {
                return c.bound(rx, &out, bytes(&call)?, &req, &plan, server).await;
            }
            Err(e) => send(&out, &error(&id, &e.2)).await?,
        }
    }
    Ok(ExitCode::SUCCESS)
}
