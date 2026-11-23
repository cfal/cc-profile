use super::{Incoming, Out, bad, incoming, key, parse, reject, send};
use crate::{launch, record_failure, request, validate};
use ccpd::{
    agy::{self, acp as wire},
    protocol::Request,
    types::Result,
};
use serde_json::{Value, json};
use std::{path::Path, process::ExitCode, time::Duration};
use tokio::time::Instant;

pub(crate) struct Binding {
    pub(super) init: Value,
    pub(super) notification: Option<Value>,
    pub(super) rx: Option<Incoming>,
    pub(super) first: Option<Vec<u8>>,
}

pub(crate) async fn run(mut req: Request) -> Result<ExitCode> {
    let deadline = || Instant::now() + Duration::from_secs(5);
    let binary = req
        .binary
        .clone()
        .ok_or_else(ccpd::native::unsupported_build)?;
    let version: String = request(
        "/v3/agy-acp/check",
        json!(binary).to_string().as_bytes(),
        deadline(),
    )
    .await?;
    if version != agy::VERSION {
        return Err(ccpd::native::unsupported_build());
    }
    let mut rx = incoming()?;
    let out = Out::new(tokio::io::stdout());
    let mut init: Option<Value> = None;
    let mut notification = None;
    let argc = req.argv.len();
    while let Some(bytes) = rx.recv().await {
        let bytes = bytes?;
        let v = parse(&bytes)?;
        let id = &v["id"];
        let method = v["method"].as_str().ok_or_else(bad)?;
        if v.get("id").is_none() {
            if method == "notifications/initialized" && init.is_some() && notification.is_none() {
                notification = Some(v);
            }
            continue;
        }
        key(id)?;
        let params = &v["params"];
        match method {
            "initialize" if init.is_none() => {
                if !(params.is_object() || params.is_null())
                    || params
                        .get("protocolVersion")
                        .is_some_and(|v| v.as_u64().is_none_or(|n| n < 1))
                    || params
                        .get("clientCapabilities")
                        .is_some_and(|v| !v.is_object())
                {
                    reject(
                        &out,
                        id,
                        "Invalid ACP protocolVersion or clientCapabilities.",
                    )
                    .await?;
                    continue;
                }
                send(
                    &out,
                    json!({"jsonrpc":"2.0","id":id,"result":wire::initialize_result()}),
                )
                .await?;
                init = Some(v);
            }
            _ if init.is_none() => reject(&out, id, "Initialize the ACP connection first.").await?,
            "initialize" => reject(&out, id, "This ACP connection is already initialized.").await?,
            "authenticate" | "auth/logout" | "logout" => {
                reject(
                    &out,
                    id,
                    "Sign in with ccp login <profile-id> in your terminal.",
                )
                .await?
            }
            "session/list" => {
                if !params.is_null() && !params.is_object()
                    || !params["cursor"].is_null()
                    || !(params["cwd"].is_null()
                        || params["cwd"].as_str().is_some_and(wire::workspace))
                {
                    reject(&out,id,"Expected an optional absolute cwd and no cursor; managed browse returns one complete list.").await?;
                    continue;
                }
                let q = json!({"binary":binary,"cwd":params["cwd"]});
                match request::<Value>(
                    "/v3/agy-acp/list",
                    q.to_string().as_bytes(),
                    Instant::now() + Duration::from_secs(35),
                )
                .await
                {
                    Ok(r) if r["sessions"].is_array() => {
                        send(&out, json!({"jsonrpc":"2.0","id":id,"result":r})).await?
                    }
                    Ok(_) => reject(&out, id, "Invalid managed list reply.").await?,
                    Err(e) => reject(&out, id, &e.2).await?,
                }
            }
            "session/new" | "session/load" | "session/resume" => {
                if (method == "session/new" || params.get("cwd").is_some())
                    && !params["cwd"].as_str().is_some_and(wire::workspace)
                {
                    reject(&out, id, "Expected an absolute ACP workspace.").await?;
                    continue;
                }
                if method != "session/new" {
                    let Some(sid) = params["sessionId"]
                        .as_str()
                        .and_then(|s| agy::session_id(s).ok())
                    else {
                        reject(&out, id, "Expected an exact ACP session ID.").await?;
                        continue;
                    };
                    req.argv.extend(["--for-session".into(), sid.into()]);
                }
                let plan = match launch(&req, deadline()).await {
                    Ok(p) => p,
                    Err(e) => {
                        reject(&out, id, &e.2).await?;
                        req.argv.truncate(argc);
                        req.request_id = uuid::Uuid::new_v4().to_string();
                        req.ticket = ccpd::paths::secret()?;
                        continue;
                    }
                };
                let (home, managed) = validate(&plan, &req)?;
                if std::env::var_os("CC_PROFILE_DEBUG").as_deref() == Some("1".as_ref()) {
                    eprintln!("ccp: agy/{} -> {home}", plan.label);
                }
                let mut binding = Binding {
                    init: init.unwrap(),
                    notification,
                    rx: Some(rx),
                    first: Some(bytes),
                };
                let result = super::run(
                    &binary,
                    Path::new(home),
                    &req,
                    &plan,
                    managed.as_ref(),
                    Some(&mut binding),
                )
                .await;
                if let Err(e) = &result {
                    record_failure(&req, libc::EIO).await;
                    if binding.first.is_some() {
                        reject(&out, id, &e.2).await?;
                    }
                }
                return result;
            }
            _ => reject(&out, id, "Open or resume a session before this operation.").await?,
        }
    }
    Ok(ExitCode::SUCCESS)
}
