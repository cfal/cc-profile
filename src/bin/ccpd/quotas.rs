use crate::service::{Shared, blocking};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use ccpd::{
    native::{PROXIES, executable},
    quota::{self, auth_stamp},
    store::{Config, config, configs},
    types::{Error, Result, now},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::PathBuf, sync::Mutex};
use tokio::sync::Semaphore;

pub struct Jobs {
    active: Mutex<HashSet<i64>>,
    slots: Semaphore,
    curl: Option<PathBuf>,
}
impl Jobs {
    pub fn new() -> Self {
        Self {
            active: Mutex::default(),
            slots: Semaphore::new(2),
            curl: executable("curl"),
        }
    }
}
struct Cache {
    stamp: String,
    status: String,
    attempt: i64,
    success: Option<i64>,
    retry: i64,
    fails: i64,
    snapshot: Option<String>,
}
impl Cache {
    fn retry_for(&self, stamp: &str) -> i64 {
        if self.stamp == stamp || self.fails == 0 || self.status == "rate_limited" {
            self.retry
        } else {
            // A new login or proxy can resolve the previous input's error.
            0
        }
    }
}
// Bind numbers to the inputs used to fetch them, not to label, Current or Enabled.
fn inputs(c: &Config) -> Option<String> {
    if !c.present() {
        return None;
    }
    let inherited = (c.mode == "inherit").then(|| PROXIES.map(|k| std::env::var(k).ok()));
    let fields = json!([c.dev, c.ino, auth_stamp(c), c.mode, c.proxy_url, inherited]);
    Some(format!("{:x}", Sha256::digest(fields.to_string())))
}
fn cached(db: &Connection, id: i64) -> Result<Option<Cache>> {
    Ok(db.query_row("SELECT status,attempt_ms,success_ms,retry_ms,failures,snapshot,stamp FROM quota_cache WHERE config_id=?",
        [id],|r|Ok(Cache{status:r.get(0)?,attempt:r.get(1)?,success:r.get(2)?,retry:r.get(3)?,fails:r.get(4)?,snapshot:r.get(5)?,stamp:r.get(6)?})).optional()?)
}
pub fn view(db: &Connection, c: &Config, j: &Jobs) -> Result<Value> {
    if c.tool == "agy" {
        return Ok(json!({"status":"native_report","loading":false}));
    }
    let busy = j.active.lock().unwrap().contains(&c.id);
    let Some(stamp) = inputs(c) else {
        return Ok(json!({"status":"unavailable","loading":busy}));
    };
    let Some(cache) = cached(db, c.id)? else {
        return Ok(json!({"status":"not_checked","loading":busy}));
    };
    if cache.stamp != stamp {
        let status = if cache.status == "rate_limited" && now() < cache.retry {
            "rate_limited"
        } else {
            "not_checked"
        };
        return Ok(json!({"status":status,"loading":busy,"retry_ms":cache.retry_for(&stamp)}));
    }
    let snapshot = cache
        .snapshot
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    Ok(
        json!({"status":cache.status,"loading":busy,"attempt_ms":cache.attempt,"success_ms":cache.success,
        "retry_ms":cache.retry,"stale":snapshot.is_some()&&(cache.status!="fresh"||cache.success.is_none_or(|t|now()-t>=300000)),"snapshot":snapshot}),
    )
}
pub async fn queue(s: Shared, id: Option<i64>, stale_only: bool) -> Result<Json<Value>> {
    let a = s.clone();
    let candidates = blocking(move || {
        let db = a.store.db()?;
        let cs = match id {
            Some(id) => vec![config(&db, id)?],
            None => configs(&db, "claude")?
                .into_iter()
                .chain(configs(&db, "codex")?)
                .filter(|c| c.enabled)
                .collect(),
        };
        let mut jobs = Vec::new();
        for c in cs {
            if c.tool == "agy" {
                if id.is_some() {
                    return Err(Error::bad(
                        "native_report",
                        "Request the native quota report instead.",
                    ));
                }
                continue;
            }
            let Some(stamp) = inputs(&c) else { continue };
            let cache = cached(&db, c.id)?;
            if cache.as_ref().is_some_and(|r| {
                now() < r.retry_for(&stamp)
                    || (stale_only
                        && r.stamp == stamp
                        && r.status == "fresh"
                        && r.success.is_some_and(|t| now() - t < 300000))
            }) {
                continue;
            }
            if a.quotas.active.lock().unwrap().insert(c.id) {
                jobs.push((stamp, c, cache));
            }
        }
        Ok(jobs)
    })
    .await?;
    let n = candidates.len();
    for (stamp, c, prior) in candidates {
        let a = s.clone();
        tokio::spawn(async move {
            let id = c.id;
            let _slot = a.quotas.slots.acquire().await.unwrap();
            let run = || -> Result<bool> {
                let current = config(&*a.store.db()?, id)?;
                Ok(inputs(&current).as_ref() == Some(&stamp))
            };
            if run().unwrap_or(false) {
                let result =
                    quota::fetch(&c, a.build(&c.tool), a.quotas.curl.as_deref(), &a.state_dir)
                        .await;
                let a2 = a.clone();
                let saved = blocking(move || a2.store.write(|db| {
                    let ms = now();
                    let (status, failures, retry, success, snapshot) = match result {
                        Ok(v) => ("fresh", 0, ms + 30000, Some(ms), Some(v.to_string())),
                        Err(e) => {
                            let fails = prior.as_ref().filter(|q| q.stamp==stamp || q.status=="rate_limited").map_or(1, |q| q.fails + 1);
                            let delay = (30000_i64 << (fails-1).min(6)).max(e.retry_after_ms.unwrap_or(0));
                            let last = prior.filter(|q| q.stamp == stamp);
                            (e.status, fails, ms.saturating_add(delay), last.as_ref().and_then(|q| q.success), last.and_then(|q| q.snapshot))
                        }
                    };
                    // Discard numbers if inputs changed in flight, not a provider's Retry-After.
                    let (status, success, snapshot) = if inputs(&config(db, id)?).as_ref() == Some(&stamp) {
                        (status, success, snapshot)
                    } else {
                        (if status=="fresh" { "unavailable" } else { status }, None, None)
                    };
                    db.execute("INSERT INTO quota_cache(config_id,revision,attempt_ms,success_ms,retry_ms,failures,status,stamp,snapshot) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(config_id) DO UPDATE SET revision=excluded.revision,attempt_ms=excluded.attempt_ms,success_ms=excluded.success_ms,retry_ms=excluded.retry_ms,failures=excluded.failures,status=excluded.status,stamp=excluded.stamp,snapshot=excluded.snapshot",
                       params![id,c.revision,ms,success,retry,failures,status,stamp,snapshot])?;
                    Ok(())
                })).await;
                if saved.is_err() {
                    eprintln!("ccpd: could not persist quota observation for config {id}");
                }
            }
            a.quotas.active.lock().unwrap().remove(&id);
        });
    }
    Ok(Json(json!({"queued":n})))
}
#[derive(Deserialize)]
pub struct Options {
    #[serde(default)]
    stale_only: bool,
}
pub async fn bulk(
    State(s): State<Shared>,
    Json(v): Json<Options>,
) -> Result<(StatusCode, Json<Value>)> {
    Ok((StatusCode::ACCEPTED, queue(s, None, v.stale_only).await?))
}
pub async fn one(
    State(s): State<Shared>,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<Value>)> {
    if id <= 0 {
        return Err(Error::bad("config", "Expected config ID."));
    }
    Ok((StatusCode::ACCEPTED, queue(s, Some(id), false).await?))
}

// The pinned CLI handles /usage as a native text report, not an ACP method.
// Only run on explicit demand; never store the free-form report in the quota cache.
pub async fn report(State(s): State<Shared>, Path(id): Path<i64>) -> Result<Json<Value>> {
    use ccpd::{
        agy,
        native::{clean_command, unsupported_build},
    };
    use std::{process::Stdio, time::Duration};
    use tokio::io::AsyncReadExt;
    let c = config(&*s.store.db()?, id)?;
    if c.tool != "agy" {
        return Err(Error::bad(
            "wrong_tool",
            "Use this provider's quota meters.",
        ));
    }
    if !c.present() || !s.agy.supported || !s.agy.unchanged() {
        return Err(unsupported_build());
    }
    if !agy::has_login(&c.home)? {
        return Err(Error::bad(
            "sign_in_required",
            "Sign in to this profile first.",
        ));
    }
    let _slot = s.quotas.slots.try_acquire().map_err(|_| {
        Error::conflict("busy", "Another quota check is running. Try again shortly.")
    })?;
    let unavailable = || {
        Error::unavailable(
            "report_unavailable",
            "Native quota report unavailable. Run ccp usage for this profile in your terminal.",
        )
    };
    let rt = agy::Runtime::new()?;
    let mut cmd = clean_command(&s.agy.bin, &c.home, Some(&c));
    rt.configure(
        &mut cmd,
        &c.home,
        &agy::USAGE.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
    );
    cmd.env("NO_COLOR", "1")
        .env("BROWSER", "/bin/false")
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().map_err(|_| unavailable())?;
    let output = tokio::time::timeout(Duration::from_secs(15), async {
        let mut text = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .take(16385)
            .read_to_string(&mut text)
            .await?;
        if text.len() > 16384 || !child.wait().await?.success() {
            return Err(unavailable());
        }
        let lower = text.to_ascii_lowercase();
        if text.trim().is_empty()
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            || !lower.contains("quota")
            || [
                "https://",
                "http://",
                "authentication required",
                "authorization code",
                "bearer ",
                "ya29.",
                "1//",
                "refresh_token",
                "access_token",
            ]
            .iter()
            .any(|k| lower.contains(k))
        {
            return Err(unavailable());
        }
        Ok(text)
    })
    .await
    .map_err(|_| unavailable())??;
    let current = config(&*s.store.db()?, id)?;
    if current.revision != c.revision || !current.present() {
        return Err(Error::conflict(
            "stale_config",
            "Profile changed during quota lookup.",
        ));
    }
    Ok(Json(json!({"report":output})))
}
