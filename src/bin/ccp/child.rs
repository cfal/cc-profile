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

pub(super) type Incoming = tokio::sync::mpsc::Receiver<io::Result<Vec<u8>>>;
pub(super) fn incoming(name: &str, limit: usize) -> io::Result<Incoming> {
    use std::io::{BufRead, Read};
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut reader = io::stdin().lock();
            loop {
                let mut line = Vec::new();
                match (&mut reader)
                    .take(limit as u64 + 1)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) => break,
                    Ok(_) => {
                        let drain = line.len() > limit && !line.ends_with(b"\n");
                        if tx.blocking_send(Ok(line)).is_err() {
                            break;
                        }
                        // Report overflow immediately, but never deliver its tail as another frame.
                        if drain && let Err(e) = reader.skip_until(b'\n') {
                            let _ = tx.blocking_send(Err(e));
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
