use super::{
    NativeExec,
    child::{Signals, status, stop},
};
use ccpd::{
    invocation::native_id,
    protocol::{Plan, Request},
    socket,
    types::{Error, Result},
};
use serde::Deserialize;
use serde_json::json;
use std::{io, path::Path, process::ExitCode, time::Duration};
use tokio::{process::Command, time::timeout};

#[path = "codex/lazy.rs"]
pub(super) mod lazy;
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

pub(super) async fn run_stdio(binary: &Path, req: &Request, plan: &Plan) -> Result<ExitCode> {
    stdio::run(binary, req, plan, &mut Signals::new()?).await
}
