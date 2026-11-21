use crate::{
    native::{Build, clean_command},
    store::Config,
    types::{Error, Result},
};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub async fn codex(
    build: &Build,
    home: &Config,
    cwd: &Path,
    method: &str,
    params: Value,
) -> Result<Value> {
    let failed = || Error::unavailable("native_rpc", "Native Codex metadata service unavailable.");
    if !build.unchanged() || !home.present() {
        return Err(failed());
    }
    let mut child = clean_command(&build.bin, cwd, Some(home))
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| failed())?;
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap().take(1 << 20)).lines();
    let result=tokio::time::timeout(Duration::from_secs(12),async {
        for(id,msg) in [(1,json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"cc-profile","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}})),
            (2,json!({"id":2,"method":method,"params":params}))]{
            if id==2{input.write_all(b"{\"method\":\"initialized\"}\n").await?;}
            input.write_all(format!("{msg}\n").as_bytes()).await?;input.flush().await?;
            loop{
                let line=lines.next_line().await?.ok_or_else(failed)?;
                let v:Value=serde_json::from_str(&line).map_err(|_|failed())?;
                if v["id"]!=id{continue}
                if !v["error"].is_null(){
                    let message=v["error"]["message"].as_str().unwrap_or("").to_ascii_lowercase();
                    return Err(if message.contains("authentication required")||message.contains("unauthorized")||message.contains("401"){
                        Error::bad("sign_in_required","Sign in with a supported ChatGPT account.")
                    }else if message.contains("429")||message.contains("too many requests"){Error::unavailable("rate_limited","Rate limited.")}
                    else{failed()})
                }
                let result=v.get("result").ok_or_else(failed)?;
                if id==2{return Ok(result.clone())}break
            }
        }
        Err(failed())
    }).await.unwrap_or_else(|_|Err(failed()));
    drop(input);
    let _ = child.start_kill();
    let _ = child.wait().await;
    result
}
