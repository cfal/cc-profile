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
use tokio::sync::{Notify, Semaphore};
use uuid::Uuid;

pub struct Jobs {
    active: Mutex<HashMap<i64, Uuid>>,
    slots: Semaphore,
    changed: Notify,
}
impl Jobs {
    // Called under the store transaction: a superseded fetch cannot restore the old observation.
    pub fn invalidate(&self, db: &Connection, id: i64) -> Result<()> {
        db.execute("DELETE FROM quota_cache WHERE config_id=?", [id])?;
        self.active.lock().unwrap().remove(&id);
        self.changed.notify_waiters();
        Ok(())
    }
    pub fn new() -> Self {
        Self {
            active: Mutex::default(),
            slots: Semaphore::new(2),
            changed: Notify::new(),
        }
    }

    async fn wait_for(&self, tickets: &[(i64, Uuid)]) {
        loop {
            let changed = self.changed.notified();
            let pending = {
                let active = self.active.lock().unwrap();
                tickets
                    .iter()
                    .any(|(id, ticket)| active.get(id) == Some(ticket))
            };
            if !pending {
                return;
            }
            changed.await;
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
    fn from_fetch(stamp: String, prior: Option<Self>, ms: i64, result: Result<Value>) -> Self {
        match result {
            Ok(value) => Self {
                stamp,
                status: "fresh".into(),
                attempt: ms,
                success: Some(ms),
                retry: ms + 30_000,
                fails: 0,
                snapshot: Some(value.to_string()),
            },
            Err(error) => {
                let fails = prior
                    .as_ref()
                    .filter(|q| q.stamp == stamp || q.status == "rate_limited")
                    .map_or(1, |q| q.fails.max(0).saturating_add(1));
                let delay = 30_000_i64 << (fails - 1).min(6);
                let previous = prior.filter(|q| q.stamp == stamp);
                let status = match error.1 {
                    "sign_in_required" | "rate_limited" => error.1,
                    _ => "unavailable",
                };
                Self {
                    stamp,
                    status: status.into(),
                    attempt: ms,
                    success: previous.as_ref().and_then(|q| q.success),
                    retry: ms.saturating_add(delay),
                    fails,
                    snapshot: previous.and_then(|q| q.snapshot),
                }
            }
        }
    }

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
    let row = db.query_row(
        "SELECT status,attempt_ms,success_ms,retry_ms,failures,snapshot,stamp FROM quota_cache WHERE config_id=?",
        [id],
        |r| Ok(Cache {
            status: r.get(0)?,
            attempt: r.get(1)?,
            success: r.get(2)?,
            retry: r.get(3)?,
            fails: r.get(4)?,
            snapshot: r.get(5)?,
            stamp: r.get(6)?,
        }),
    ).optional();
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
    Routing(String),
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
    tickets: Vec<(i64, Uuid)>,
}
struct Job {
    config: Config,
    stamp: String,
    prior: Option<Cache>,
    ticket: Uuid,
}
impl Job {
    fn current(&self, s: &Shared, enabled_only: bool) -> Result<bool> {
        let id = self.config.id;
        let current = config(&*s.store.db()?, id)?;
        Ok((!enabled_only || current.enabled)
            && inputs(&current).as_ref() == Some(&self.stamp)
            && s.quotas.active.lock().unwrap().get(&id) == Some(&self.ticket))
    }

    fn persist(self, s: &Shared, result: Result<Value>) -> Result<()> {
        let id = self.config.id;
        s.store.write(|db| {
            if s.quotas.active.lock().unwrap().get(&id) != Some(&self.ticket) {
                return Ok(());
            }
            let mut cache = Cache::from_fetch(self.stamp, self.prior, now(), result);
            // A changed Codex login can retain its retry deadline, but never the old observation.
            if inputs(&config(db, id)?).as_ref() != Some(&cache.stamp) {
                if self.config.tool != "codex" {
                    return Ok(());
                }
                if cache.status == "fresh" {
                    cache.status = "unavailable".into();
                }
                cache.success = None;
                cache.snapshot = None;
            }
            db.execute(
                "INSERT INTO quota_cache(config_id,revision,attempt_ms,success_ms,retry_ms,failures,status,stamp,snapshot)
                 VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(config_id) DO UPDATE SET
                 revision=excluded.revision,attempt_ms=excluded.attempt_ms,success_ms=excluded.success_ms,
                 retry_ms=excluded.retry_ms,failures=excluded.failures,status=excluded.status,
                 stamp=excluded.stamp,snapshot=excluded.snapshot",
                params![id, self.config.revision, cache.attempt, cache.success, cache.retry, cache.fails,
                        cache.status, cache.stamp, cache.snapshot],
            )?;
            Ok(())
        })
    }

    async fn run(self, s: Shared, enabled_only: bool) {
        let id = self.config.id;
        let ticket = self.ticket;
        let _slot = s.quotas.slots.acquire().await.unwrap();
        let build = s.build(&self.config.tool).await;
        if self.current(&s, enabled_only).unwrap_or(false) {
            let result = quota::fetch(&self.config, &build, &s.state_dir).await;
            let app = s.clone();
            if blocking(move || self.persist(&app, result)).await.is_err() {
                eprintln!("ccpd: could not persist quota observation for config {id}");
            }
        }
        let mut active = s.quotas.active.lock().unwrap();
        if active.get(&id) == Some(&ticket) {
            active.remove(&id);
            s.quotas.changed.notify_waiters();
        }
    }
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
    let (candidates, next_check_ms, tickets) = blocking(move || {
        let db = a.store.db()?;
        let cs = match &scope {
            Scope::Account { id, .. } => vec![config(&db, *id)?],
            Scope::Routing(t) => configs(&db, t)?,
            Scope::All => configs(&db, "claude")?
                .into_iter()
                .chain(configs(&db, "codex")?)
                .chain(configs(&db, "agy")?)
                .collect(),
        };
        drop(db);
        // A slow profile directory must not hold the store lock during a roster sweep.
        let stamped: Vec<_> = cs.into_iter().filter(|c| !enabled_only || c.enabled)
            .filter(|c| !matches!(scope, Scope::Routing(_)) || quota::routing_quota_possible(c))
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
            jobs.push(Job { config: c, stamp, prior: cache, ticket: Uuid::new_v4() });
        }
        // Do not mark any jobs active until candidate enumeration succeeds.
        let mut active = a.quotas.active.lock().unwrap();
        let mut tickets = Vec::new();
        jobs.retain(|job| {
            let id = job.config.id;
            let ticket = *active.entry(id).or_insert(job.ticket);
            tickets.push((id, ticket));
            ticket == job.ticket
        });
        Ok((jobs, next, tickets))
    })
    .await?;
    let n = candidates.len();
    for job in candidates {
        tokio::spawn(job.run(s.clone(), enabled_only));
    }
    Ok(Queued {
        count: n,
        next_check_ms,
        tickets,
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
pub async fn before_launch(s: Shared, tool: String) {
    // Finish scheduling even if the caller disconnects or the wait expires.
    let queued = tokio::spawn(queue(s.clone(), Scope::Routing(tool), Refresh::Stale));
    let wait = async {
        match queued.await {
            Ok(Ok(q)) => s.quotas.wait_for(&q.tickets).await,
            _ => eprintln!("ccpd: could not queue stale routing quota checks"),
        }
    };
    let _ = tokio::time::timeout(ccpd::protocol::QUOTA_WAIT, wait).await;
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
    fn failed_fetch_retains_matching_snapshot_but_only_backoff_crosses_a_login_change() {
        let fresh = Cache::from_fetch("first".into(), None, 100, Ok(json!({"windows": []})));
        let failed = Cache::from_fetch(
            "first".into(),
            Some(fresh),
            200,
            Err(Error::unavailable("rate_limited", "")),
        );
        assert_eq!(failed.success, Some(100));
        assert_eq!(failed.snapshot.as_deref(), Some(r#"{"windows":[]}"#));
        assert_eq!((failed.fails, failed.retry), (1, 30_200));

        let unavailable = || Err(Error::unavailable("native_rpc", ""));
        let changed = Cache::from_fetch("second".into(), Some(failed), 300, unavailable());
        assert_eq!(changed.status, "unavailable");
        assert_eq!((changed.fails, changed.retry), (2, 60_300));
        assert!(changed.success.is_none() && changed.snapshot.is_none());

        let unrelated = Cache::from_fetch("third".into(), Some(changed), 400, unavailable());
        assert_eq!((unrelated.fails, unrelated.retry), (1, 30_400));
    }

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
