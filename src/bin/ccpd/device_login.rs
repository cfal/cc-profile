use super::service::Shared;
use axum::{
    Json,
    extract::{Path, State},
};
use ccpd::{
    native::{Build, unsupported_build},
    rpc::Codex,
    store::{Config, config, revision},
    types::{Error, Result},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::oneshot, time::timeout};
use uuid::Uuid;

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Phase {
    Starting,
    Waiting { url: String, code: String },
    Cancelling,
    Cancelled,
    Completed,
    Expired,
    Failed,
}
struct Attempt {
    id: String,
    profile: i64,
    phase: Phase,
    cancel: Option<oneshot::Sender<()>>,
}
impl Attempt {
    fn active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Starting | Phase::Waiting { .. } | Phase::Cancelling
        )
    }
    fn view(&self) -> Value {
        let mut value = json!(self.phase);
        value["attempt"] = json!(self.id);
        value
    }
}
#[derive(Clone, Default)]
pub struct Logins(Arc<Mutex<Option<Attempt>>>);

pub async fn status(State(s): State<Shared>, Path(id): Path<i64>) -> Json<Value> {
    Json(
        s.logins
            .0
            .lock()
            .unwrap()
            .as_ref()
            .filter(|a| a.profile == id)
            .map_or_else(|| json!({"state":"idle"}), Attempt::view),
    )
}
pub async fn start(
    State(s): State<Shared>,
    Path(id): Path<i64>,
    Json(v): Json<Value>,
) -> Result<Json<Value>> {
    let build = s.build("codex").await;
    let c = config(&*s.store.db()?, id)?;
    revision(&v, &c)?;
    if c.tool != "codex" || !c.present() {
        return Err(Error::conflict(
            "profile_unavailable",
            "Choose a present Codex profile.",
        ));
    }
    if !build.supported || !build.unchanged() {
        return Err(unsupported_build());
    }
    let mut slot = s.logins.0.lock().unwrap();
    if let Some(a) = slot.as_ref().filter(|a| a.active()) {
        if a.profile == id {
            return Ok(Json(a.view()));
        }
        return Err(Error::conflict(
            "login_busy",
            "Finish or cancel the device sign-in on the other profile first.",
        ));
    }
    let (tx, rx) = oneshot::channel();
    let attempt = Uuid::new_v4().to_string();
    let a = Attempt {
        id: attempt.clone(),
        profile: id,
        phase: Phase::Starting,
        cancel: Some(tx),
    };
    let result = a.view();
    *slot = Some(a);
    tokio::spawn(s.logins.clone().run(build, c, attempt, rx));
    Ok(Json(result))
}
pub async fn cancel(
    State(s): State<Shared>,
    Path(id): Path<i64>,
    Json(v): Json<Value>,
) -> Result<Json<Value>> {
    let mut slot = s.logins.0.lock().unwrap();
    let a = slot
        .as_mut()
        .filter(|a| a.profile == id && Some(a.id.as_str()) == v["attempt"].as_str())
        .ok_or_else(|| Error::conflict("stale_login", "This sign-in attempt has changed."))?;
    if let Some(tx) = a.cancel.take() {
        a.phase = Phase::Cancelling;
        let _ = tx.send(());
    }
    Ok(Json(a.view()))
}
impl Logins {
    async fn run(self, build: Build, c: Config, id: String, mut cancelled: oneshot::Receiver<()>) {
        let begin = async {
            let mut rpc = Codex::open(&build, &c, std::path::Path::new("/")).await?;
            let v = rpc
                .call(
                    2,
                    "account/login/start",
                    json!({"type":"chatgptDeviceCode"}),
                )
                .await?;
            let url = v["verificationUrl"].as_str().unwrap_or("");
            let code = v["userCode"].as_str().unwrap_or("");
            let login = v["loginId"].as_str().unwrap_or("");
            if v["type"] != "chatgptDeviceCode"
                || url != "https://auth.openai.com/codex/device"
                || code.is_empty()
                || code.len() > 32
                || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || login.is_empty()
            {
                return Err(Error::unavailable(
                    "device_login",
                    "Device sign-in is unavailable.",
                ));
            }
            if let Some(a) = self
                .0
                .lock()
                .unwrap()
                .as_mut()
                .filter(|a| a.id == id && a.cancel.is_some())
            {
                a.phase = Phase::Waiting {
                    url: url.into(),
                    code: code.into(),
                };
            }
            Ok((rpc, login.to_owned()))
        };
        let started = tokio::select! {
            biased;
            _ = &mut cancelled => Err(Phase::Cancelled),
            r = timeout(Duration::from_secs(30), begin) => r.ok().and_then(Result::ok).ok_or(Phase::Failed),
        };
        let phase = match started {
            Err(phase) => phase,
            Ok((mut rpc, login)) => {
                let wait = async {
                    loop {
                        let Ok(v) = rpc.next().await else {
                            return Phase::Failed;
                        };
                        if v["method"] == "account/login/completed"
                            && v["params"]["loginId"] == login
                        {
                            return if v["params"]["success"] == true && c.present() {
                                Phase::Completed
                            } else {
                                Phase::Failed
                            };
                        }
                    }
                };
                let phase = tokio::select! {
                    biased;
                    _ = &mut cancelled => Phase::Cancelled,
                    r = timeout(Duration::from_secs(15 * 60), wait) => r.unwrap_or(Phase::Expired),
                };
                if matches!(phase, Phase::Cancelled | Phase::Expired) {
                    let _ = timeout(
                        Duration::from_secs(2),
                        rpc.call(3, "account/login/cancel", json!({"loginId":login})),
                    )
                    .await;
                }
                rpc.close().await;
                phase
            }
        };
        if let Some(a) = self.0.lock().unwrap().as_mut().filter(|a| a.id == id) {
            a.cancel = None;
            a.phase = phase;
        }
    }
}
