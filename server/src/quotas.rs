use crate::service::{Shared, blocking};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use ccpd::{
    native::executable,
    quota::{self, auth_stamp},
    store::{Config, config, configs},
    types::{Error, Result, now},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
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
    status: String,
    attempt: i64,
    success: Option<i64>,
    retry: i64,
    fails: i64,
    snapshot: Option<String>,
}
fn cached(db: &Connection, c: &Config) -> Result<Option<Cache>> {
    Ok(db.query_row("SELECT status,attempt_ms,success_ms,retry_ms,failures,snapshot FROM quota_cache WHERE config_id=? AND revision=? AND stamp=?",
        params![c.id,c.revision,auth_stamp(c)],|r|Ok(Cache{status:r.get(0)?,attempt:r.get(1)?,success:r.get(2)?,retry:r.get(3)?,fails:r.get(4)?,snapshot:r.get(5)?})).optional()?)
}
pub fn view(db: &Connection, c: &Config, j: &Jobs) -> Result<Value> {
    let busy = j.active.lock().unwrap().contains(&c.id);
    let Some(cache) = cached(db, c)? else {
        return Ok(json!({"status":"not_checked","loading":busy}));
    };
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
            let cache = cached(&db, &c)?;
            if cache.as_ref().is_some_and(|r| {
                now() < r.retry
                    || (stale_only
                        && r.status == "fresh"
                        && r.success.is_some_and(|t| now() - t < 300000))
            }) {
                continue;
            }
            if a.quotas.active.lock().unwrap().insert(c.id) {
                jobs.push((auth_stamp(&c), c, cache));
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
                Ok(current.revision == c.revision
                    && auth_stamp(&current) == stamp
                    && current.present())
            };
            if run().unwrap_or(false) {
                let result =
                    quota::fetch(&c, a.build(&c.tool), a.quotas.curl.as_deref(), &a.state_dir)
                        .await;
                let a2 = a.clone();
                let saved=blocking(move||a2.store.write(|db|{
                    let actual=config(db,c.id)?;if actual.revision!=c.revision||auth_stamp(&actual)!=stamp{return Ok(())}
                    let ms=now();let(status,failures,retry,success,snapshot)=match result{
                        Ok(v)=>("fresh",0,ms+30000,Some(ms),Some(v.to_string())),
                        Err(e)=>{let fails=prior.as_ref().map(|q|q.fails+1).unwrap_or(1);
                            let delay=(30000_i64 << (fails-1).min(6)).max(e.retry_after_ms.unwrap_or(0));
                            (e.status,fails,ms.saturating_add(delay),prior.as_ref().and_then(|c|c.success),prior.and_then(|q|q.snapshot))}
                    };
                    db.execute("INSERT INTO quota_cache(config_id,revision,attempt_ms,success_ms,retry_ms,failures,status,stamp,snapshot) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(config_id) DO UPDATE SET revision=excluded.revision,attempt_ms=excluded.attempt_ms,success_ms=excluded.success_ms,retry_ms=excluded.retry_ms,failures=excluded.failures,status=excluded.status,stamp=excluded.stamp,snapshot=excluded.snapshot",
                       params![id,c.revision,ms,success,retry,failures,status,stamp,snapshot])?;Ok(())
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
