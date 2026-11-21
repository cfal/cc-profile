use super::service::Shared;
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
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Mutex};

pub struct Pairing {
    key: [u8; 32],
    sessions: Mutex<HashMap<String, (String, i64)>>,
}
impl Pairing {
    pub fn new(key: &str) -> Self {
        Self {
            key: Sha256::digest(key.as_bytes()).into(),
            sessions: Mutex::default(),
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
    fn csrf(&self, h: &HeaderMap) -> Result<String> {
        self.sessions
            .lock()
            .map_err(|_| Error::denied())?
            .get(Self::cookie(h))
            .filter(|(_, until)| *until > now())
            .map(|(csrf, _)| csrf.clone())
            .ok_or_else(Error::denied)
    }
}
#[derive(Deserialize)]
pub struct Login {
    token: String,
}
pub async fn login(State(s): State<Shared>, Json(v): Json<Login>) -> Result<Response> {
    if Sha256::digest(v.token.as_bytes())[..] != s.pairing.key {
        return Err(Error::denied());
    }
    let sid = paths::secret()?;
    let csrf = paths::secret()?;
    let mut sessions = s.pairing.sessions.lock().map_err(|_| Error::denied())?;
    sessions.retain(|_, (_, until)| *until > now());
    if sessions.len() >= 64
        && let Some(k) = sessions
            .iter()
            .min_by_key(|(_, (_, until))| *until)
            .map(|(k, _)| k.clone())
    {
        sessions.remove(&k);
    }
    sessions.insert(sid.clone(), (csrf.clone(), now() + 8 * 3_600_000));
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
    Ok(Json(json!({"csrf":s.pairing.csrf(&h)?})))
}
pub async fn logout(State(s): State<Shared>, h: HeaderMap) -> Result<Response> {
    s.pairing
        .sessions
        .lock()
        .map_err(|_| Error::denied())?
        .remove(Pairing::cookie(&h));
    Ok((
        [(
            header::SET_COOKIE,
            "ccp_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        )],
        Json(json!({})),
    )
        .into_response())
}
pub async fn guard(State(s): State<Shared>, req: Request, next: Next) -> Response {
    let checked = (|| -> Result<()> {
        let h = req.headers();
        let host = h
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if host != format!("127.0.0.1:{}", s.port) && host != format!("localhost:{}", s.port) {
            return Err(Error::denied());
        }
        let path = req.uri().path();
        let mutation = !matches!(*req.method(), Method::GET | Method::HEAD);
        if mutation
            && (h.get(header::ORIGIN).and_then(|h| h.to_str().ok())
                != Some(&format!("http://{host}"))
                || !h
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.split(';').next() == Some("application/json")))
        {
            return Err(Error::denied());
        }
        if path.starts_with("/api/") && path != "/api/v1/login" {
            let csrf = s.pairing.csrf(h)?;
            if mutation && h.get("x-csrf-token").and_then(|h| h.to_str().ok()) != Some(&csrf) {
                return Err(Error::denied());
            }
        }
        Ok(())
    })();
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
