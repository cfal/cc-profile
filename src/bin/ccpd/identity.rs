use super::service::Shared;
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use ccpd::{
    agy,
    native::{Build, clean_command},
    quota::{auth_stamp, inputs},
    rpc,
    store::{Config, config},
    types::{Error, Result},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{path::Path as Dir, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

mod catalog;

#[derive(Serialize)]
pub struct Identity {
    state: &'static str,
    email: Option<String>,
}
fn status(state: &'static str) -> Identity {
    Identity { state, email: None }
}
fn unavailable() -> Error {
    Error::unavailable(
        "identity_unavailable",
        "Could not check this profile's sign-in.",
    )
}
fn reported(email: &Value) -> Identity {
    let email = email.as_str().filter(|s| {
        s.len() <= 254
            && !s.chars().any(|c| c.is_control() || c.is_whitespace())
            && s.split_once('@').is_some_and(|(user, domain)| {
                !user.is_empty() && !domain.is_empty() && !domain.contains('@')
            })
    });
    Identity {
        state: "signed_in",
        email: email.map(str::to_owned),
    }
}
async fn claude(c: &Config, build: &Build) -> Result<Identity> {
    let mut child = clean_command(&build.bin, Dir::new("/"), Some(c))
        .args(["auth", "status", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| unavailable())?;
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .unwrap()
            .take(16385)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > 16384 {
            return Err(unavailable());
        }
        let code = child.wait().await?.code();
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        match (code, v["loggedIn"].as_bool()) {
            (Some(0), Some(true)) => Ok(reported(&v["email"])),
            (Some(0 | 1), Some(false)) => Ok(status("signed_out")),
            _ => Err(unavailable()),
        }
    })
    .await
    .map_err(|_| unavailable())?
}

pub async fn read(State(s): State<Shared>, Path(id): Path<i64>) -> Result<Json<Identity>> {
    let c = config(&*s.store.db()?, id)?;
    let build = s.build(&c.tool).await;
    if !c.present() || !build.supported || !build.unchanged() {
        return Ok(Json(status("unavailable")));
    }
    if c.tool == "agy" {
        return Ok(Json(status(match agy::has_login(&c.home) {
            Ok(true) => "saved_login",
            Ok(false) => "no_saved_login",
            Err(_) => "unavailable",
        })));
    }
    let _slot = s
        .identity_slots
        .try_acquire()
        .map_err(|_| Error::conflict("busy", "Another sign-in check is in progress."))?;
    let stamp = auth_stamp(&c);
    let result = if c.tool == "claude" {
        claude(&c, &build).await
    } else {
        rpc::codex(
            &build,
            &c,
            Dir::new("/"),
            "account/read",
            json!({"refreshToken":false}),
        )
        .await
        .and_then(|r| match r.get("account") {
            Some(Value::Null) => Ok(status("signed_out")),
            Some(a) if a["type"] == "chatgpt" => Ok(reported(&a["email"])),
            Some(a) if a["type"] == "apiKey" => Ok(status("signed_in")),
            _ => Err(unavailable()),
        })
    };
    let current = config(&*s.store.db()?, id)?;
    if current.revision != c.revision || !current.present() || stamp != auth_stamp(&current) {
        return Ok(Json(status("unavailable")));
    }
    Ok(Json(result.unwrap_or_else(|_| status("unavailable"))))
}

pub async fn check_catalog(
    State(s): State<Shared>,
    Path(id): Path<i64>,
    body: std::result::Result<Json<Value>, JsonRejection>,
) -> Result<Json<Identity>> {
    if !body.is_ok_and(|v| v.as_object().is_some_and(|m| m.is_empty())) {
        return Err(Error::bad("bad_request", "Expected an empty JSON object."));
    }
    let c = config(&*s.store.db()?, id)?;
    if c.tool != "agy" {
        return Err(Error::bad(
            "wrong_tool",
            "Catalog checks require an AgY profile.",
        ));
    }
    let _slot = s
        .identity_slots
        .try_acquire()
        .map_err(|_| Error::conflict("busy", "Another sign-in check is in progress."))?;
    let build = s.build("agy").await;
    let unknown = Json(status("unavailable"));
    let Some(stamp) = inputs(&c) else {
        return Ok(unknown);
    };
    if !build.supported || !build.unchanged() {
        return Ok(unknown);
    }
    let Ok(state) = catalog::check(&c, &build).await else {
        return Ok(unknown);
    };
    let installed = s.build("agy").await;
    let current = config(&*s.store.db()?, id)?;
    if !installed.supported
        || installed.bin != build.bin
        || installed.fingerprint != build.fingerprint
        || !installed.unchanged()
        || inputs(&current) != Some(stamp)
    {
        return Ok(unknown);
    }
    Ok(Json(status(state)))
}
