use super::{
    NativeExec,
    child::{Signals, status, stop},
};
use ccpd::{
    invocation::{Intent, Invocation, native_id},
    paths,
    protocol::{Plan, Request},
    socket,
    types::{Error, Result},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs, io,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    time::Duration,
};
use tokio::{
    net::UnixStream,
    process::Command,
    time::{sleep, timeout},
};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};

#[path = "stdio.rs"]
mod stdio;

const SECOND: Duration = Duration::from_secs(1);
fn failed(msg: &'static str) -> Error {
    Error::unavailable("codex_app_server", msg)
}

fn command(binary: &Path, argv: &[String], plan: &Plan) -> Command {
    let exec = NativeExec::new(binary, "codex", argv, plan);
    let parent = std::process::id();
    let mut cmd = Command::new(binary);
    cmd.kill_on_drop(true);
    // A killed launcher must not leave a credentialed app-server running.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                return Err(io::Error::from_raw_os_error(libc::EINTR));
            }
            Err(exec.exec())
        });
    }
    cmd
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thread {
    id: String,
    session_id: String,
    ephemeral: bool,
    cwd: String,
    forked_from_id: Option<String>,
    parent_thread_id: Option<String>,
}
impl Thread {
    fn root(&self) -> bool {
        self.id == self.session_id
            && !self.ephemeral
            && self.forked_from_id.is_none()
            && self.parent_thread_id.is_none()
            && native_id(&self.id).is_ok()
    }
}
async fn report(req: &Request, event: &str, thread: &Thread) -> Result<()> {
    let body = json!({"request_id":req.request_id,"ticket":req.ticket,"event":event,
        "source":"app-server","native_id":thread.id,"workspace":thread.cwd})
    .to_string();
    for _ in 0..2 {
        if let Ok(Ok((status, _))) = timeout(
            2 * SECOND,
            socket::post("/v2/observations", body.as_bytes()),
        )
        .await
        {
            if status.is_success() {
                return Ok(());
            }
            if status.is_client_error() {
                break;
            }
        }
    }
    Err(failed(
        "Could not record the Codex thread owner; managed resume is not guaranteed.",
    ))
}

struct SocketDir(PathBuf);
impl SocketDir {
    fn new() -> Result<Self> {
        let base = paths::runtime_dir()?;
        paths::private_dir(&base)?;
        let path = base.join(format!(
            "c-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        ));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("s"));
        let _ = fs::remove_dir(&self.0);
    }
}
type Socket = WebSocketStream<UnixStream>;
async fn message(ws: &mut Socket) -> Result<Value> {
    while let Some(Ok(m)) = ws.next().await {
        match m {
            Message::Text(text) => {
                return serde_json::from_str(&text)
                    .map_err(|_| failed("Invalid Codex notification."));
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => break,
        }
    }
    Err(failed("Lost the private Codex observation connection."))
}
async fn connect(path: &Path) -> Result<Socket> {
    let stream = loop {
        // Codex can publish a symlink to a shorter protected socket path.
        match UnixStream::connect(path.canonicalize().unwrap_or_else(|_| path.into())).await {
            Ok(s) => break s,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                sleep(Duration::from_millis(25)).await
            }
            Err(_) => return Err(failed("Could not connect to the private Codex socket.")),
        }
    };
    if stream.peer_cred()?.uid() != paths::uid() {
        return Err(Error::denied());
    }
    let (mut ws, _) = client_async("ws://localhost/", stream)
        .await
        .map_err(|_| failed("Codex socket handshake failed."))?;
    let init = json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"cc-profile","version":"1"}}});
    ws.send(Message::Text(init.to_string().into()))
        .await
        .map_err(|_| failed("Codex initialization failed."))?;
    loop {
        let m = message(&mut ws).await?;
        if m["id"] == 1 {
            if !m["result"].is_object() {
                return Err(failed("Codex refused observation initialization."));
            }
            break;
        }
    }
    ws.send(Message::Text(r#"{"method":"initialized"}"#.into()))
        .await
        .map_err(|_| failed("Codex initialization failed."))?;
    Ok(ws)
}
async fn observe(mut ws: Socket, req: &Request) -> Result<()> {
    loop {
        let m = message(&mut ws).await?;
        if m["method"] == "thread/started"
            && let Some(raw) = m.pointer("/params/thread")
            && let Ok(thread) = Thread::deserialize(raw)
            && thread.root()
        {
            return report(req, "thread/started", &thread).await;
        }
    }
}
fn client_args(argv: &[String], invocation: &Invocation, endpoint: String) -> Vec<String> {
    let parsed = &invocation.codex;
    let mut args = Vec::new();
    if parsed.exec.is_none() {
        // The remote TUI asks for folder consent only with an explicit cwd.
        args.extend([
            "--remote".into(),
            endpoint.clone(),
            "--cd".into(),
            invocation.workspace.to_string_lossy().into_owned(),
        ]);
    }
    for (index, arg) in argv.iter().enumerate() {
        if parsed.cd.as_ref().is_some_and(|r| r.contains(&index))
            || parsed
                .model
                .as_ref()
                .is_some_and(|(r, _)| r.contains(&index))
        {
            continue;
        }
        args.push(arg.clone());
        if parsed.exec == Some(index) {
            args.extend(["--remote".into(), endpoint.clone()]);
        }
    }
    args
}

pub(super) async fn run(
    binary: &Path,
    req: &Request,
    plan: &Plan,
    invocation: &Invocation,
) -> Result<ExitCode> {
    if req.argv.first().is_some_and(|s| s == "app-server") {
        return stdio::run(binary, req, plan, &mut Signals::new()?).await;
    }
    if matches!(invocation.intent, Intent::Resume(_)) {
        // The owner is already known. Keep Codex's native resume and trust UX.
        let mut args = req.argv.clone();
        if invocation.codex.exec.is_none()
            && !args
                .iter()
                .take_while(|s| *s != "--")
                .any(|s| s == "--no-daemon")
        {
            args.insert(0, "--no-daemon".into());
        }
        return Err(Error::unavailable(
            "exec_failed",
            format!(
                "Cannot execute Codex: {}",
                NativeExec::new(binary, "codex", &args, plan).exec()
            ),
        ));
    }
    let mut sig = Signals::new()?;
    let dir = SocketDir::new()?;
    let path = dir.0.join("s");
    let endpoint = format!("unix://{}", path.display());
    let mut server_args = vec!["app-server".into(), "--listen".into(), endpoint.clone()];
    if let Some((_, model)) = &invocation.codex.model {
        server_args.extend(["-c".into(), format!("model={}", json!(model))]);
    }
    let mut server = command(binary, &server_args, plan)
        .current_dir(&invocation.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| failed("Could not start the native Codex app-server."))?;
    let result = async {
        let ws = tokio::select! {
            s = sig.recv() => return Ok(ExitCode::from((128 + s) as u8)),
            _ = server.wait() => return Err(failed("Codex app-server exited before the client started.")),
            r = timeout(10 * SECOND, connect(&path)) => {
                r.map_err(|_| failed("Timed out initializing the private Codex socket."))??
            }
        };
        let args = client_args(&req.argv, invocation, endpoint);
        let mut client = command(binary, &args, plan).spawn()
            .map_err(|_| failed("Could not start the native Codex client."))?;
        let mut pending = true;
        let watch = observe(ws, req);
        tokio::pin!(watch);
        loop {
            tokio::select! {
                // Prefer an already queued UUID when a fast exec client exits.
                biased;
                r = &mut watch, if pending => {
                    pending = false;
                    if let Err(e) = r {
                        eprintln!("ccp: {e}");
                    }
                }
                res = client.wait() => {
                    let code = status(res?);
                    if pending && !matches!(timeout(2 * SECOND, &mut watch).await, Ok(Ok(()))) {
                        eprintln!("ccp: No root thread recorded for this launch.");
                    }
                    return Ok(code);
                }
                s = sig.recv() => {
                    stop(&mut client, s).await;
                    return Ok(ExitCode::from((128 + s) as u8));
                }
                _ = server.wait() => {
                    stop(&mut client, libc::SIGTERM).await;
                    return Err(failed("Private Codex app-server exited unexpectedly."));
                }
            }
        }
    }.await;
    stop(&mut server, libc::SIGTERM).await;
    result
}
