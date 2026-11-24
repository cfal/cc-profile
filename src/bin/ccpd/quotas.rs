use crate::service::{Shared, blocking};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use ccpd::{
    quota::{self, FRESH_MS, inputs},
    store::{Config, config, configs},
    types::{Error, Result, now},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Mutex, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;

pub struct Jobs {
    active: Mutex<HashMap<i64, Uuid>>,
    slots: Semaphore,
}
impl Jobs {
    // Called under the store transaction: a superseded fetch cannot restore the old observation.
    pub fn invalidate(&self, db: &Connection, id: i64) -> Result<()> {
        db.execute("DELETE FROM quota_cache WHERE config_id=?", [id])?;
        self.active.lock().unwrap().remove(&id);
        Ok(())
    }
    pub fn new() -> Self {
        Self {
            active: Mutex::default(),
            slots: Semaphore::new(2),
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
fn cached(db: &Connection, id: i64) -> Result<Option<Cache>> {
    let row = db.query_row("SELECT status,attempt_ms,success_ms,retry_ms,failures,snapshot,stamp FROM quota_cache WHERE config_id=?",
        [id],|r|Ok(Cache{status:r.get(0)?,attempt:r.get(1)?,success:r.get(2)?,retry:r.get(3)?,fails:r.get(4)?,snapshot:r.get(5)?,stamp:r.get(6)?})).optional();
    match row {
        Err(rusqlite::Error::InvalidColumnType(..)) => Ok(None),
        result => Ok(result?),
    }
}
pub fn view(db: &Connection, c: &Config, j: &Jobs) -> Result<Value> {
    let busy = j.active.lock().unwrap().contains_key(&c.id);
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
    let mut snapshot = cache
        .snapshot
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if let Some(obj) = snapshot.as_mut().and_then(Value::as_object_mut) {
        obj.remove("file_auth_verified");
    }
    Ok(
        json!({"status":cache.status,"loading":busy,"attempt_ms":cache.attempt,"success_ms":cache.success,
        "retry_ms":cache.retry,"stale":snapshot.is_some()&&(cache.status!="fresh"||cache.success.is_none_or(|t|!fresh(t))),"snapshot":snapshot}),
    )
}
fn fresh(success: i64) -> bool {
    (0..FRESH_MS).contains(&now().saturating_sub(success))
}
pub fn verified_alias(db: &Connection, id: i64, stamp: &str) -> Result<bool> {
    Ok(cached(db, id)?.is_some_and(|q| {
        q.stamp == stamp
            && q.status == "fresh"
            && q.success.is_some_and(fresh)
            && q.snapshot
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .is_some_and(|v| v["file_auth_verified"] == true)
    }))
}
enum Scope {
    Account {
        id: i64,
        enabled_only: bool,
        expected: Option<String>,
    },
    Provider(String),
    All,
}
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const ROSTER_CHECK_MS: i64 = 60_000;

#[derive(Clone, Copy)]
enum Refresh {
    Manual,
    Stale,
    Daily,
}
impl Refresh {
    fn due_at(self, cache: &Cache, stamp: &str, ms: i64) -> i64 {
        let age_limit = match self {
            Self::Stale if cache.status == "fresh" => FRESH_MS,
            Self::Daily => DAY_MS,
            _ => 0,
        };
        let deadline = cache
            .success
            .filter(|&t| cache.stamp == stamp && t <= ms)
            .map_or(0, |t| t.saturating_add(age_limit));
        deadline.max(cache.retry_for(stamp))
    }
}
struct Queued {
    count: usize,
    next_check_ms: i64,
}
async fn queue(s: Shared, scope: Scope, refresh: Refresh) -> Result<Queued> {
    let a = s.clone();
    let enabled_only = !matches!(
        scope,
        Scope::Account {
            enabled_only: false,
            ..
        }
    );
    let (candidates, next_check_ms) = blocking(move || {
        let db = a.store.db()?;
        let cs = match &scope {
            Scope::Account { id, .. } => vec![config(&db, *id)?],
            Scope::Provider(t) => configs(&db, t)?,
            Scope::All => configs(&db, "claude")?
                .into_iter()
                .chain(configs(&db, "codex")?)
                .chain(configs(&db, "agy")?)
                .collect(),
        };
        drop(db);
        // A slow profile directory must not hold the store lock during a roster sweep.
        let stamped: Vec<_> = cs.into_iter().filter(|c| !enabled_only || c.enabled)
            .filter_map(|c| inputs(&c).map(|stamp| (c, stamp))).collect();
        let db = a.store.db()?;
        let ms = now();
        let mut next = ms + ROSTER_CHECK_MS;
        let mut jobs = Vec::new();
        for (c, stamp) in stamped {
            if matches!(&scope, Scope::Account { expected: Some(expected), .. } if *expected != stamp) {
                continue;
            }
            let cache = cached(&db, c.id)?;
            let due = cache.as_ref().map_or(0, |r| refresh.due_at(r, &stamp, ms));
            if due > ms {
                next = next.min(due);
                continue;
            }
            jobs.push((stamp, c, cache, Uuid::new_v4()));
        }
        // Do not mark any jobs active until candidate enumeration succeeds.
        let mut active = a.quotas.active.lock().unwrap();
        jobs.retain(|(_, c, _, ticket)| {
            if let std::collections::hash_map::Entry::Vacant(slot) = active.entry(c.id) {
                slot.insert(*ticket);
                true
            } else {
                false
            }
        });
        Ok((jobs, next))
    })
    .await?;
    let n = candidates.len();
    for (stamp, c, prior, ticket) in candidates {
        let a = s.clone();
        tokio::spawn(async move {
            let id = c.id;
            let _slot = a.quotas.slots.acquire().await.unwrap();
            let run = || -> Result<bool> {
                let current = config(&*a.store.db()?, id)?;
                Ok((!enabled_only || current.enabled)
                    && inputs(&current).as_ref() == Some(&stamp)
                    && a.quotas.active.lock().unwrap().get(&id) == Some(&ticket))
            };
            if run().unwrap_or(false) {
                let result = quota::fetch(&c, a.build(&c.tool), &a.state_dir).await;
                let a2 = a.clone();
                let saved = blocking(move || a2.store.write(|db| {
                    if a2.quotas.active.lock().unwrap().get(&id) != Some(&ticket) {
                        return Ok(());
                    }
                    let ms = now();
                    let (status, failures, retry, success, snapshot) = match result {
                        Ok(v) => ("fresh", 0, ms + 30000, Some(ms), Some(v.to_string())),
                        Err(e) => {
                            let fails = prior.as_ref().filter(|q| q.stamp==stamp || q.status=="rate_limited").map_or(1, |q| q.fails.max(0).saturating_add(1));
                            let delay = 30000_i64 << (fails-1).min(6);
                            let status = match e.1 {
                                "sign_in_required" | "rate_limited" => e.1,
                                _ => "unavailable",
                            };
                            let last = prior.filter(|q| q.stamp == stamp);
                            (status, fails, ms.saturating_add(delay), last.as_ref().and_then(|q| q.success), last.and_then(|q| q.snapshot))
                        }
                    };
                    // Never publish an observation across a profile or credential change.
                    let (status, success, snapshot) = if inputs(&config(db, id)?).as_ref() == Some(&stamp) {
                        (status, success, snapshot)
                    } else if c.tool != "codex" {
                        return Ok(());
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
            let mut active = a.quotas.active.lock().unwrap();
            if active.get(&id) == Some(&ticket) {
                active.remove(&id);
            }
        });
    }
    Ok(Queued {
        count: n,
        next_check_ms,
    })
}
pub async fn run_daily(s: Shared) {
    loop {
        let delay = match queue(s.clone(), Scope::All, Refresh::Daily).await {
            Ok(q) => q
                .next_check_ms
                .saturating_sub(now())
                .clamp(0, ROSTER_CHECK_MS),
            Err(_) => {
                eprintln!("ccpd: could not schedule daily quota checks");
                ROSTER_CHECK_MS
            }
        };
        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
    }
}
pub async fn after_reset(s: Shared, selected: i64, refresh: Vec<(i64, String)>) {
    for (id, stamp) in refresh {
        let scope = Scope::Account {
            id,
            enabled_only: id != selected,
            expected: Some(stamp),
        };
        if queue(s.clone(), scope, Refresh::Manual).await.is_err() {
            eprintln!("ccpd: could not queue post-reset quota check for config {id}");
        }
    }
}
pub async fn refresh_provider(s: Shared, tool: String) {
    if queue(s, Scope::Provider(tool), Refresh::Stale)
        .await
        .is_err()
    {
        eprintln!("ccpd: could not queue stale routing quota checks");
    }
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
    let refresh = if v.stale_only {
        Refresh::Stale
    } else {
        Refresh::Manual
    };
    let q = queue(s, Scope::All, refresh).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"queued": q.count}))))
}
pub async fn one(
    State(s): State<Shared>,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<Value>)> {
    if id <= 0 {
        return Err(Error::bad("config", "Expected config ID."));
    }
    let q = queue(
        s,
        Scope::Account {
            id,
            enabled_only: false,
            expected: None,
        },
        Refresh::Manual,
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"queued": q.count}))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_deadline_uses_the_last_matching_success_not_the_last_attempt() {
        let mut q = Cache {
            stamp: "login".into(),
            status: "fresh".into(),
            attempt: DAY_MS,
            success: Some(DAY_MS),
            retry: DAY_MS + 30_000,
            fails: 0,
            snapshot: None,
        };
        let ms = DAY_MS + FRESH_MS;
        assert_eq!(Refresh::Stale.due_at(&q, "login", ms), ms);
        assert_eq!(Refresh::Daily.due_at(&q, "login", ms), 2 * DAY_MS);
        assert!(Refresh::Daily.due_at(&q, "new login", ms) <= ms);
        q.status = "unavailable".into();
        q.attempt = ms;
        q.retry = ms + 30_000;
        q.fails = 1;
        assert_eq!(Refresh::Daily.due_at(&q, "login", ms), 2 * DAY_MS);
        q.success = None;
        assert_eq!(Refresh::Daily.due_at(&q, "login", ms), q.retry);
        assert_eq!(Refresh::Daily.due_at(&q, "new login", ms), 0);
        q.status = "rate_limited".into();
        assert_eq!(Refresh::Daily.due_at(&q, "new login", ms), q.retry);
    }
}
