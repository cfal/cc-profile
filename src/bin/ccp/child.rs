use std::{
    io,
    os::unix::process::ExitStatusExt,
    process::{ExitCode, ExitStatus},
    time::Duration,
};
use tokio::{
    process::Child,
    signal::unix::{Signal, SignalKind, signal},
    time::timeout,
};

pub(super) struct Signals {
    int: Signal,
    term: Signal,
    hup: Signal,
}
impl Signals {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
        })
    }
    pub(super) async fn recv(&mut self) -> i32 {
        tokio::select! { _=self.int.recv()=>libc::SIGINT, _=self.term.recv()=>libc::SIGTERM, _=self.hup.recv()=>libc::SIGHUP }
    }
}
pub(super) fn status(s: ExitStatus) -> ExitCode {
    ExitCode::from(s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(1)) as u8)
}
pub(super) async fn stop(child: &mut Child, sig: i32) {
    if let Some(id) = child.id() {
        unsafe {
            libc::kill(id as i32, sig);
        }
        if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
}
