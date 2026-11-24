use super::{Signals, Thread, command, failed, report, status, stop};
use ccpd::{
    invocation::codex_stdio_command,
    protocol::{Plan, Request},
    types::Result,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::{self, PipeWriter, Read, Write},
    path::Path,
    process::{ExitCode, Stdio},
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Inspection is best effort for very large frames; transport is byte-for-byte
// even when a prompt or an unrelated response exceeds this budget.
const LIMIT: usize = 2 << 20;
#[derive(Default)]
struct Frame {
    buf: Vec<u8>,
    oversized: bool,
}
impl Frame {
    fn feed(&mut self, part: &[u8], starts: &Starts) -> Option<&[u8]> {
        if !self.oversized {
            if self.buf.len() + part.len() <= LIMIT {
                self.buf.extend_from_slice(part);
            } else {
                self.buf.clear();
                self.oversized = true;
                // Its id is unknown. Forget outstanding claims rather than
                // misattribute a later reply if the client reuses an id.
                starts.lock().unwrap().clear();
            }
        }
        if !part.ends_with(b"\n") {
            return None;
        }
        if self.oversized {
            self.oversized = false;
            return None;
        }
        Some(&self.buf)
    }
    fn done(&mut self, part: &[u8]) {
        if part.ends_with(b"\n") {
            self.buf.clear();
        }
    }
}
#[derive(Deserialize, Eq, PartialEq, Hash)]
#[serde(untagged)]
pub(super) enum Id {
    Number(i64),
    Text(String),
}
#[derive(Deserialize)]
struct Call {
    id: Id,
    method: String,
}
#[derive(Deserialize)]
struct Fork {
    params: Selector,
}
#[derive(Deserialize)]
struct Selector {
    #[serde(rename = "threadId")]
    thread_id: String,
}
#[derive(Deserialize)]
struct Reply {
    id: Id,
    method: Option<String>,
}
#[derive(Deserialize)]
struct Started {
    result: StartedResult,
}
#[derive(Deserialize)]
struct StartedResult {
    thread: Thread,
}
pub(super) type Starts = Arc<Mutex<HashMap<Id, Option<String>>>>;

pub(super) fn track(line: &[u8], starts: &Starts, managed_fork: bool) {
    if let Ok(call) = serde_json::from_slice::<Call>(line) {
        let mut ids = starts.lock().unwrap();
        ids.remove(&call.id);
        if ids.len() < 1024 && !matches!(&call.id, Id::Text(s) if s.len()>256) {
            if call.method == "thread/start" {
                ids.insert(call.id, None);
            } else if managed_fork
                && call.method == "thread/fork"
                && let Ok(fork) = serde_json::from_slice::<Fork>(line)
            {
                ids.insert(call.id, Some(fork.params.thread_id));
            }
        }
    }
}

fn input(mut pipe: PipeWriter, starts: Starts) -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut chunk = [0; 16384];
    let mut frame = Frame::default();
    loop {
        let n = input.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        for part in chunk[..n].split_inclusive(|b| *b == b'\n') {
            if let Some(line) = frame.feed(part, &starts) {
                track(line, &starts, false);
            }
            frame.done(part);
        }
        pipe.write_all(&chunk[..n])?;
    }
}
pub(super) fn started(line: &[u8], starts: &Starts) -> Option<(&'static str, Thread)> {
    let reply = serde_json::from_slice::<Reply>(line).ok()?;
    if reply.method.is_some() {
        return None;
    }
    let source = starts.lock().unwrap().remove(&reply.id)?;
    started_claim(line, source)
}
pub(super) fn started_claim(line: &[u8], source: Option<String>) -> Option<(&'static str, Thread)> {
    let thread = serde_json::from_slice::<Started>(line).ok()?.result.thread;
    if let Some(source) = source {
        return (thread.id == thread.session_id
            && thread.parent_thread_id.is_none()
            && thread.id != source
            && super::native_id(&thread.id).is_ok()
            && thread.forked_from_id.as_ref() == Some(&source))
        .then_some(("thread/fork", thread));
    }
    thread.root().then_some(("thread/start", thread))
}
async fn output(
    mut pipe: tokio::process::ChildStdout,
    req: &Request,
    starts: &Starts,
) -> io::Result<()> {
    let mut out = tokio::io::stdout();
    let mut frame = Frame::default();
    let mut chunk = [0; 16384];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return out.flush().await;
        }
        for part in chunk[..n].split_inclusive(|b| *b == b'\n') {
            if let Some((event, thread)) = frame
                .feed(part, starts)
                .and_then(|line| started(line, starts))
                && !thread.ephemeral
                && let Err(e) = report(req, event, &thread).await
            {
                eprintln!("ccp: {e}");
            }
            frame.done(part);
        }
        // Record ownership before releasing the complete reply to a caller
        // that may immediately start a second server to resume this thread.
        out.write_all(&chunk[..n]).await?;
        out.flush().await?;
    }
}

pub(super) async fn run(
    binary: &Path,
    req: &Request,
    plan: &Plan,
    sig: &mut Signals,
) -> Result<ExitCode> {
    let (read, stdin) = io::pipe()?;
    let (_, argv) = codex_stdio_command(&plan.argv)?;
    let mut child = command(binary, &argv, plan)
        .stdin(Stdio::from(read))
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| failed("Could not start the native stdio app-server."))?;
    let stdout = child.stdout.take().unwrap();
    let starts = Starts::default();
    let track = starts.clone();
    // An idle client's stdin can stay open after the child dies. Do not tie
    // runtime shutdown to joining a blocking stdin read.
    std::thread::Builder::new()
        .name("codex-stdin".into())
        .spawn(move || {
            let _ = input(stdin, track);
        })?;
    tokio::select! {
        s=sig.recv()=>{ stop(&mut child,s).await; Ok(ExitCode::from((128+s) as u8)) }
        r=async { output(stdout,req,&starts).await?; child.wait().await }=> match r {
            Ok(s)=>Ok(status(s)),
            Err(_)=>{stop(&mut child,libc::SIGTERM).await; Err(failed("Codex stdio connection closed unexpectedly."))}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_frame_invalidates_unknown_ids_and_next_frame_is_read_normally() {
        let starts = Starts::default();
        starts.lock().unwrap().insert(Id::Number(1), None);
        let mut frame = Frame::default();
        assert!(frame.feed(&vec![b'x'; LIMIT], &starts).is_none());
        assert!(frame.feed(b"x\n", &starts).is_none());
        assert!(starts.lock().unwrap().is_empty());
        frame.done(b"x\n");
        assert_eq!(frame.feed(b"{}\n", &starts), Some(&b"{}\n"[..]));
    }
}
