use super::unavailable;
use ccpd::{
    agy,
    native::{Build, clean_command},
    store::Config,
    types::Result,
};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
    time::timeout,
};

struct Job(Child);
impl Drop for Job {
    fn drop(&mut self) {
        // Read both pipes before reaping: if a descendant holds one open, the
        // unreaped leader still reserves its process-group ID at cancellation.
        if let Some(pid) = self.0.id() {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
}

async fn bounded(stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    stream
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > LIMIT {
        return Err(unavailable());
    }
    Ok(bytes)
}

fn catalog(stdout: &[u8]) -> bool {
    let Ok(rows) = std::str::from_utf8(stdout) else {
        return false;
    };
    !rows.is_empty()
        && rows.lines().all(|row| {
            row.split_once('\t').is_some_and(|(id, label)| {
                !id.is_empty()
                    && !id.chars().any(|c| c.is_whitespace() || c.is_control())
                    && !label.trim().is_empty()
                    && !label.chars().any(char::is_control)
            })
        })
}

pub(super) async fn check(c: &Config, build: &Build) -> Result<&'static str> {
    let rt = agy::Runtime::new()?;
    let mut cmd = clean_command(&build.bin, &c.home, Some(c));
    rt.configure(&mut cmd, &c.home, &["models".into()]);
    let mut job = Job(cmd
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .env("NO_COLOR", "1")
        .env("BROWSER", "/bin/false")
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?);
    timeout(Duration::from_secs(6), async {
        let (out, err) = tokio::try_join!(
            bounded(job.0.stdout.take().unwrap()),
            bounded(job.0.stderr.take().unwrap())
        )?;
        let code = job.0.wait().await?.code();
        if code == Some(0) && catalog(&out) {
            return Ok("catalog_accessible");
        }
        const SIGNED_OUT: &[u8] = b"Error: Please sign in to view available models. Launch the CLI without arguments to sign in.";
        let diagnostic = err.strip_prefix(b"Fetching available models...\n").unwrap_or(&err);
        if code == Some(1) && out.is_empty() && diagnostic.trim_ascii_end() == SIGNED_OUT {
            return Ok("sign_in_required");
        }
        Err(unavailable())
    })
    .await.map_err(|_| unavailable())?
}
