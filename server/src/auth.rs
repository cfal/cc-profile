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
    sessions: Mutex<HashMap<String, (String, i64, i64)>>,
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
    fn cookie(headers: &HeaderMap) -> &str {
        headers
            .get(header::COOKIE)
            .and_then(|s| s.to_str().ok())
            .unwrap_or("")
            .split(';')
            .find_map(|kv| kv.trim().strip_prefix("ccp_session="))
            .unwrap_or("")
    }
    fn csrf(&self, h: &HeaderMap, revision: i64) -> Result<String> {
        self.sessions
            .lock()
            .map_err(|_| Error::denied())?
            .get(Self::cookie(h))
            .filter(|(_, until, rev)| *until > now() && *rev == revision)
            .map(|(csrf, _, _)| csrf.clone())
            .ok_or_else(Error::denied)
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
            return password::save(&a.store, password::hash(&v.password)?, true);
        }
        let Some(verifier) = verifier else {
            return Err(Error::conflict(
                "setup_required",
                "Set the panel password first.",
            ));
        };
        let recent = |r: i64, time: Instant| r == rev && time.elapsed() < Duration::from_secs(30);
        if a.auth
            .failures
            .lock()
            .unwrap()
            .is_some_and(|(r, n, t)| recent(r, t) && n >= 5)
        {
            return Err(limited());
        }
        if !password::matches(&v.password, &verifier)? {
            let mut failures = a.auth.failures.lock().unwrap();
            let count = failures
                .filter(|(r, _, t)| recent(*r, *t))
                .map_or(1, |(_, n, _)| n.saturating_add(1));
            *failures = Some((rev, count, Instant::now()));
            return Err(Error(
                axum::http::StatusCode::UNAUTHORIZED,
                "wrong_password",
                "Incorrect panel password.".into(),
            ));
        }
        if password::revision(&a.store)? != rev {
            return Err(Error::denied());
        }
        *a.auth.failures.lock().unwrap() = None;
        Ok(rev)
    })
    .await?;
    let sid = paths::secret()?;
    let csrf = paths::secret()?;
    let mut sessions = s.auth.sessions.lock().map_err(|_| Error::denied())?;
    sessions.retain(|_, (_, until, rev)| *until > now() && *rev >= revision);
    if sessions.len() >= 64
        && let Some(k) = sessions
            .iter()
            .min_by_key(|(_, (_, until, _))| *until)
            .map(|(k, _)| k.clone())
    {
        sessions.remove(&k);
    }
    sessions.insert(sid.clone(), (csrf.clone(), now() + 8 * 3_600_000, revision));
    Ok((
        [(
            header::SET_COOKIE,
            format!("ccp_session={sid}; Path=/; HttpOnly; SameSite=Strict; Max-Age=28800"),
        )],
        Json(json!({"csrf":csrf})),
    )
        .into_response())
}
pub async fn session(State(s): State<Shared>, h: HeaderMap) -> Result<Json<Value>> {
    let a = s.clone();
    let rev = blocking(move || password::revision(&a.store)).await?;
    Ok(Json(
        json!({"setup_required":rev==0,"csrf":s.auth.csrf(&h,rev).ok()}),
    ))
}
pub async fn logout(State(s): State<Shared>, h: HeaderMap) -> Result<Response> {
    s.auth
        .sessions
        .lock()
        .map_err(|_| Error::denied())?
        .remove(Auth::cookie(&h));
    Ok((
        [(
            header::SET_COOKIE,
            "ccp_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        )],
        Json(json!({})),
    )
        .into_response())
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
        let csrf = s.auth.csrf(h, rev)?;
        if mutation && h.get("x-csrf-token").and_then(|h| h.to_str().ok()) != Some(&csrf) {
            return Err(Error::denied());
        }
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
