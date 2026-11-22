use super::{
    password,
    service::{Shared, blocking},
};
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use ccpd::{
    paths,
    types::{Error, Result, now},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

pub struct Auth {
    sessions: Mutex<HashMap<String, (i64, i64)>>,
    slots: Arc<Semaphore>,
    failures: Mutex<Option<(i64, u8, Instant)>>,
}
impl Auth {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::default(),
            slots: Arc::new(Semaphore::new(2)),
            failures: Mutex::default(),
        }
    }
    fn token(headers: &HeaderMap) -> &str {
        headers
            .get("x-ccp-session")
            .and_then(|s| s.to_str().ok())
            .unwrap_or("")
    }
    fn signed_in(&self, h: &HeaderMap, revision: i64) -> Result<()> {
        self.sessions
            .lock()
            .map_err(|_| Error::denied())?
            .get(Self::token(h))
            .filter(|(until, rev)| *until > now() && *rev == revision)
            .map(|_| ())
            .ok_or_else(Error::denied)
    }
    fn verify(&self, password: &str, verifier: &str, rev: i64) -> Result<bool> {
        let recent = |r: i64, time: Instant| r == rev && time.elapsed() < Duration::from_secs(30);
        if self
            .failures
            .lock()
            .unwrap()
            .is_some_and(|(r, n, t)| recent(r, t) && n >= 5)
        {
            return Err(limited());
        }
        let valid = password::matches(password, verifier)?;
        let mut failures = self.failures.lock().unwrap();
        if failures.is_none_or(|(r, _, _)| r <= rev) {
            if valid {
                *failures = None;
            } else {
                let count = failures
                    .filter(|(r, _, t)| recent(*r, *t))
                    .map_or(1, |(_, n, _)| n.saturating_add(1));
                *failures = Some((rev, count, Instant::now()));
            }
        }
        Ok(valid)
    }
}
#[derive(Deserialize)]
pub struct Login {
    password: String,
}
fn limited() -> Error {
    Error(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "try_later",
        "Too many password attempts. Try again shortly.".into(),
    )
}
pub async fn setup(State(s): State<Shared>, Json(v): Json<Login>) -> Result<Response> {
    authenticate(s, v, true).await
}
pub async fn login(State(s): State<Shared>, Json(v): Json<Login>) -> Result<Response> {
    authenticate(s, v, false).await
}
async fn authenticate(s: Shared, v: Login, setup: bool) -> Result<Response> {
    let permit = s
        .auth
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| limited())?;
    let a = s.clone();
    let revision = blocking(move || {
        let _permit = permit;
        let (rev, verifier) = password::state(&a.store)?;
        if setup {
            if verifier.is_some() {
                return Err(password::already_set());
            }
            return password::save(&a.store, password::hash(&v.password)?, Some(0));
        }
        let Some(verifier) = verifier else {
            return Err(Error::conflict(
                "setup_required",
                "Set the panel password first.",
            ));
        };
        if !a.auth.verify(&v.password, &verifier, rev)? {
            return Err(Error(
                axum::http::StatusCode::UNAUTHORIZED,
                "wrong_password",
                "Incorrect panel password.".into(),
            ));
        }
        if password::revision(&a.store)? != rev {
            return Err(Error::denied());
        }
        Ok(rev)
    })
    .await?;
    let sid = paths::secret()?;
    let mut sessions = s.auth.sessions.lock().map_err(|_| Error::denied())?;
    sessions.retain(|_, (until, rev)| *until > now() && *rev >= revision);
    if sessions.len() >= 64
        && let Some(k) = sessions
            .iter()
            .min_by_key(|(_, (until, _))| *until)
            .map(|(k, _)| k.clone())
    {
        sessions.remove(&k);
    }
    sessions.insert(sid.clone(), (now() + 8 * 3_600_000, revision));
    Ok(Json(json!({"session":sid})).into_response())
}
#[derive(Deserialize)]
pub struct ChangePassword {
    current_password: String,
    new_password: String,
}
pub async fn change_password(
    State(s): State<Shared>,
    h: HeaderMap,
    Json(v): Json<ChangePassword>,
) -> Result<Json<Value>> {
    let permit = s
        .auth
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| limited())?;
    blocking(move || {
        let _permit = permit;
        let (rev, verifier) = password::state(&s.store)?;
        s.auth.signed_in(&h, rev)?;
        if !s.auth.verify(
            &v.current_password,
            &verifier.ok_or_else(Error::denied)?,
            rev,
        )? {
            return Err(Error::bad("wrong_password", "Incorrect current password."));
        }
        password::save(&s.store, password::hash(&v.new_password)?, Some(rev))?;
        Ok(Json(json!({})))
    })
    .await
}
pub async fn session(State(s): State<Shared>, h: HeaderMap) -> Result<Json<Value>> {
    let a = s.clone();
    let rev = blocking(move || password::revision(&a.store)).await?;
    Ok(Json(
        json!({"setup_required":rev==0,"signed_in":s.auth.signed_in(&h,rev).is_ok()}),
    ))
}
pub async fn logout(State(s): State<Shared>, h: HeaderMap) -> Result<Response> {
    s.auth
        .sessions
        .lock()
        .map_err(|_| Error::denied())?
        .remove(Auth::token(&h));
    Ok(Json(json!({})).into_response())
}
async fn check(s: &Shared, method: &Method, path: &str, h: &HeaderMap) -> Result<()> {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if host != format!("127.0.0.1:{}", s.port) && host != format!("localhost:{}", s.port) {
        return Err(Error::denied());
    }
    let mutation = !matches!(*method, Method::GET | Method::HEAD);
    if mutation
        && (h.get(header::ORIGIN).and_then(|h| h.to_str().ok()) != Some(&format!("http://{host}"))
            || !h
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(';').next() == Some("application/json")))
    {
        return Err(Error::denied());
    }
    if path.starts_with("/api/")
        && !matches!(path, "/api/v1/login" | "/api/v1/setup" | "/api/v1/session")
    {
        let a = s.clone();
        let rev = blocking(move || password::revision(&a.store)).await?;
        s.auth.signed_in(h, rev)?;
    }
    Ok(())
}
pub async fn guard(State(s): State<Shared>, req: Request, next: Next) -> Response {
    let checked = check(&s, req.method(), req.uri().path(), req.headers()).await;
    let mut res = match checked {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    };
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("content-security-policy",HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    res
}
