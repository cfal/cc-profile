use crate::{
    native::{Build, clean_command},
    store::Config,
    types::{Error, Result},
};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines, Take},
    process::{Child, ChildStdin, ChildStdout},
};

fn failed() -> Error {
    Error::unavailable("native_rpc", "Native Codex service unavailable.")
}

pub struct Codex {
    child: Child,
    input: ChildStdin,
    lines: Lines<BufReader<Take<ChildStdout>>>,
}
impl Codex {
    pub async fn open(build: &Build, home: &Config, cwd: &Path) -> Result<Self> {
        if !build.supported || !build.unchanged() || !home.present() {
            return Err(failed());
        }
        let mut child = clean_command(&build.bin, cwd, Some(home))
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| failed())?;
        let mut rpc = Self {
            input: child.stdin.take().unwrap(),
            lines: BufReader::new(child.stdout.take().unwrap().take(1 << 20)).lines(),
            child,
        };
        rpc.call(1, "initialize", json!({"clientInfo":{"name":"cc-profile","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
        rpc.send(json!({"method":"initialized"})).await?;
        Ok(rpc)
    }
    async fn send(&mut self, msg: Value) -> Result<()> {
        self.input.write_all(format!("{msg}\n").as_bytes()).await?;
        self.input.flush().await?;
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Value> {
        let line = self.lines.next_line().await?.ok_or_else(failed)?;
        serde_json::from_str(&line).map_err(|_| failed())
    }
    pub async fn call(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let v = self.next().await?;
            if v["id"] != id {
                continue;
            }
            if !v["error"].is_null() {
                let msg = v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                return Err(
                    if msg.contains("authentication required")
                        || msg.contains("unauthorized")
                        || msg.contains("401")
                    {
                        Error::bad(
                            "sign_in_required",
                            "Sign in with a supported ChatGPT account.",
                        )
                    } else if msg.contains("429") || msg.contains("too many requests") {
                        Error::unavailable("rate_limited", "Rate limited.")
                    } else {
                        failed()
                    },
                );
            }
            return v.get("result").cloned().ok_or_else(failed);
        }
    }
    pub async fn close(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    // Read only the auth selectors, not the rest of the native config (which may contain secrets).
    pub(crate) async fn file_auth(&mut self, cwd: &Path) -> Result<bool> {
        #[derive(Deserialize)]
        struct Selectors {
            cli_auth_credentials_store: String,
            profile: Option<IgnoredAny>,
            project_root_markers: Vec<String>,
        }
        #[derive(Deserialize)]
        struct ConfigRead {
            config: Selectors,
        }
        #[derive(Deserialize)]
        struct Reply {
            id: Option<u64>,
            result: Option<ConfigRead>,
        }
        self.send(
            json!({"id":3,"method":"config/read","params":{"cwd":cwd,"includeLayers":false}}),
        )
        .await?;
        loop {
            let line = self.lines.next_line().await?.ok_or_else(failed)?;
            let r: Reply = serde_json::from_str(&line).map_err(|_| failed())?;
            if r.id == Some(3) {
                return Ok(r.result.is_some_and(|r| {
                    let c = r.config;
                    c.cli_auth_credentials_store == "file"
                        && c.profile.is_none()
                        && c.project_root_markers == [".git"]
                }));
            }
        }
    }
}
pub async fn codex(
    build: &Build,
    home: &Config,
    cwd: &Path,
    method: &str,
    params: Value,
) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(12), async {
        let mut rpc = Codex::open(build, home, cwd).await?;
        let result = rpc.call(2, method, params).await;
        rpc.close().await;
        result
    })
    .await
    .unwrap_or_else(|_| Err(failed()))
}
