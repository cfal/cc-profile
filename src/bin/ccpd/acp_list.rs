use super::service::{Shared, blocking};
use axum::{Json, extract::State};
use ccpd::{
    agy::{self, acp},
    native::{clean_command, unsupported_build, validate_binary},
    protocol::ListedPage,
    routing::acp_owner,
    store::{Config, Store, config, configs},
    types::{Error, Result, now},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    sync::OwnedSemaphorePermit,
    time::{Instant, timeout, timeout_at},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    binary: PathBuf,
    cwd: Option<String>,
}

fn pinned(app: &Shared, path: &Path) -> Result<()> {
    if !app.agy.supported {
        return Err(unsupported_build());
    }
    validate_binary(&app.agy, Some(path))
}
pub async fn check(State(s): State<Shared>, Json(path): Json<PathBuf>) -> Result<Json<String>> {
    pinned(&s, &path)?;
    Ok(Json(s.agy.version.clone()))
}

fn unavailable() -> Error {
    Error::unavailable(
        "acp_list",
        "Could not list every enabled AgY profile. No partial list was returned.",
    )
}
fn limit() -> Error {
    Error::unavailable(
        "acp_list_limit",
        "The AgY session list exceeded the browsing limit. No partial list was returned.",
    )
}

fn enabled(db: &Connection) -> Result<Vec<Config>> {
    let profiles: Vec<_> = configs(db, "agy")?
        .into_iter()
        .filter(|c| c.enabled)
        .collect();
    if profiles.len() > 32 {
        return Err(limit());
    }
    Ok(profiles)
}
fn saved_login(c: &Config) -> Result<agy::LoginStamp> {
    if !c.present() {
        return Err(unavailable());
    }
    agy::login_stamp(&c.home)?.ok_or_else(|| {
        Error::unavailable(
            "acp_login",
            format!(
                "AgY profile {} has no saved login. No partial list was returned.",
                c.id
            ),
        )
    })
}
type Roster = Vec<(Config, agy::LoginStamp)>;
fn snapshot<T>(
    store: &Store,
    mut probe: impl FnMut(&Config) -> Result<T>,
) -> Result<Vec<(Config, T)>> {
    let profiles = enabled(&*store.db()?)?;
    profiles
        .into_iter()
        .map(|c| {
            let value = probe(&c)?;
            Ok((c, value))
        })
        .collect()
}

// A timed-out blocking stat can finish later; retain the single browse slot until it does.
async fn browse_task<T: Send + 'static>(
    slot: &Arc<OwnedSemaphorePermit>,
    task: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let slot = slot.clone();
    blocking(move || {
        let _slot = slot;
        task()
    })
    .await
}

#[derive(Default)]
struct Catalog {
    rows: BTreeMap<String, (i64, Value)>,
    size: usize,
}

impl Catalog {
    fn add(&mut self, owner: i64, raw: &Value) -> Result<()> {
        let id = agy::session_id(raw["sessionId"].as_str().ok_or_else(unavailable)?)?;
        let cwd = raw["cwd"]
            .as_str()
            .filter(|s| acp::workspace(s))
            .ok_or_else(unavailable)?;
        let mut row = json!({"sessionId":id,"cwd":cwd});
        for k in ["title", "updatedAt"] {
            if !raw[k].is_null() {
                row[k] = raw[k].as_str().ok_or_else(unavailable)?.into();
            }
        }
        if !raw["additionalDirectories"].is_null() {
            let dirs = raw["additionalDirectories"]
                .as_array()
                .filter(|dirs| {
                    dirs.len() <= 64 && dirs.iter().all(|d| d.as_str().is_some_and(acp::workspace))
                })
                .ok_or_else(unavailable)?;
            row["additionalDirectories"] = Value::Array(dirs.clone());
        }
        if self.rows.contains_key(id) {
            return Err(Error::conflict(
                "ambiguous_session",
                "Duplicate AgY session ID; no browse results were published.",
            ));
        }
        self.size += row.to_string().len() + 1;
        if self.size > 400 * 1024 || self.rows.len() >= acp::MAX_LIST_ROWS {
            return Err(limit());
        }
        self.rows.insert(id.into(), (owner, row));
        Ok(())
    }

    fn publish(self, db: &Connection, filter: Option<&str>) -> Result<Vec<Value>> {
        let mut rows = Vec::new();
        for (id, (owner, row)) in self.rows {
            let cwd = row["cwd"].as_str().unwrap();
            let matches = filter.is_none_or(|wanted| wanted == cwd);
            record_listed(db, owner, &id, cwd, matches)?;
            if matches {
                rows.push(row);
            }
        }
        Ok(rows)
    }
}

fn record_listed(db: &Connection, owner: i64, id: &str, cwd: &str, publish: bool) -> Result<()> {
    let binding: Option<(i64, String)> = db
        .prepare_cached("SELECT owner,evidence FROM sessions WHERE tool='agy' AND native_id=?")?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    match binding {
        Some((old, _)) if old != owner => {
            return Err(Error::conflict(
                "ambiguous_session",
                "AgY session conflicts with its recorded owner; no browse results were published.",
            ));
        }
        None if publish => {
            db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms) VALUES('agy',?,?,'acp_listed',?,?)",params![id,owner,cwd,now()])?;
        }
        Some((_, e)) if publish && e == "saved_file" => {
            db.execute("UPDATE sessions SET evidence='acp_listed',workspace=? WHERE tool='agy' AND native_id=?",params![cwd,id])?;
        }
        _ => {}
    }
    Ok(())
}

pub async fn listed(State(s): State<Shared>, Json(page): Json<ListedPage>) -> Result<Json<usize>> {
    let end = Instant::now() + Duration::from_secs(5);
    if page.sessions.len() > acp::MAX_LIST_ROWS {
        return Err(limit());
    }
    let mut seen = HashSet::new();
    for (id, cwd) in &page.sessions {
        if agy::session_id(id).is_err() || !acp::workspace(cwd) || !seen.insert(id) {
            return Err(Error::bad(
                "acp_page",
                "Invalid AgY session list page; no sessions were published.",
            ));
        }
    }
    blocking(move || {
        let owner = |db: &Connection| config(db, acp_owner(db, &page.request_id, &page.ticket)?);
        let before = owner(&*s.store.db()?)?;
        if !before.enabled || !before.present() {
            return Err(Error::denied());
        }
        s.store.write(|db| {
            if Instant::now() >= end || owner(db)? != before {
                return Err(Error::denied());
            }
            for (id, cwd) in &page.sessions {
                record_listed(db, before.id, id, cwd, true)?;
            }
            if Instant::now() >= end {
                return Err(unavailable());
            }
            Ok(Json(page.sessions.len()))
        })
    })
    .await
}

async fn read_profile(binary: &Path, c: &Config, catalog: &mut Catalog) -> Result<()> {
    let rt = agy::Runtime::new()?;
    let mut cmd = clean_command(binary, &c.home, Some(c));
    rt.configure(
        &mut cmd,
        &c.home,
        &["acp".into(), "--stdio".into(), "--disable-telemetry".into()],
    );
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut writer = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}});
    acp::check_initialize(&acp::exchange(&mut writer, &mut reader, &init).await?)?;
    acp::write(
        &mut writer,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await?;
    let mut cursors = HashSet::new();
    let mut args = json!({});
    for page in 1..=64 {
        let query = json!({"jsonrpc":"2.0","id":page,"method":"session/list","params":args});
        let result = acp::exchange(&mut writer, &mut reader, &query).await?;
        for row in result["sessions"].as_array().ok_or_else(unavailable)? {
            catalog.add(c.id, row)?;
        }
        if result["nextCursor"].is_null() {
            drop(writer);
            return if child.wait().await?.success() {
                Ok(())
            } else {
                Err(unavailable())
            };
        }
        let next = result["nextCursor"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .ok_or_else(unavailable)?;
        if !cursors.insert(next.to_owned()) {
            return Err(unavailable());
        }
        args = json!({"cursor":next});
    }
    Err(limit())
}

async fn collect(
    s: &Shared,
    binary: &Path,
    profiles: &Roster,
    slot: &Arc<OwnedSemaphorePermit>,
) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    for (c, stamp) in profiles {
        let app = s.clone();
        let id = c.id;
        let bin = binary.to_owned();
        let (current, current_stamp) = browse_task(slot, move || {
            pinned(&app, &bin)?;
            let current = config(&*app.store.db()?, id)?;
            if !current.enabled {
                return Err(unavailable());
            }
            let stamp = saved_login(&current)?;
            Ok((current, stamp))
        })
        .await?;
        if current != *c || current_stamp != *stamp {
            return Err(unavailable());
        }
        match timeout(
            Duration::from_secs(8),
            read_profile(binary, c, &mut catalog),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) if matches!(e.1, "ambiguous_session" | "acp_list_limit") => return Err(e),
            _ => {
                return Err(Error::unavailable(
                    "acp_list",
                    format!(
                        "AgY profile {id} could not list its sessions. No partial list was returned."
                    ),
                ));
            }
        }
    }
    Ok(catalog)
}

pub async fn list(State(s): State<Shared>, Json(q): Json<Query>) -> Result<Json<Value>> {
    if q.cwd.as_deref().is_some_and(|s| !acp::workspace(s)) {
        return Err(Error::bad("cwd", "Expected an absolute workspace."));
    }
    let slot = Arc::new(s.acp_listing.clone().try_acquire_owned().map_err(|_| {
        Error::unavailable(
            "acp_list_busy",
            "Another AgY browse is in progress. Try again.",
        )
    })?);
    let end = Instant::now() + Duration::from_secs(30);
    timeout_at(end, browse(s, q, slot, end))
        .await
        .map_err(|_| unavailable())?
}

async fn browse(
    s: Shared,
    q: Query,
    slot: Arc<OwnedSemaphorePermit>,
    end: Instant,
) -> Result<Json<Value>> {
    let app = s.clone();
    let bin = q.binary.clone();
    let profiles = browse_task(&slot, move || {
        pinned(&app, &bin)?;
        snapshot(&app.store, saved_login)
    })
    .await?;
    let catalog = collect(&s, &q.binary, &profiles, &slot).await?;
    browse_task(&slot, move || {
        pinned(&s, &q.binary)?;
        if snapshot(&s.store, saved_login)? != profiles {
            return Err(unavailable());
        }
        let mut rows = s.store.write(|db| {
            if Instant::now() >= end || !enabled(db)?.iter().eq(profiles.iter().map(|(c, _)| c)) {
                return Err(unavailable());
            }
            let result = catalog.publish(db, q.cwd.as_deref())?;
            if Instant::now() >= end {
                return Err(unavailable());
            }
            Ok(result)
        })?;
        // Preserve the stable ID order for equal or absent timestamps.
        rows.sort_by_cached_key(|r| {
            std::cmp::Reverse(r["updatedAt"].as_str().and_then(|s| {
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
            }))
        });
        Ok(Json(json!({"sessions":rows})))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccpd::{
        native::{Build, stamp},
        routing::{Request, launch},
    };
    use std::{fs, os::unix::fs::DirBuilderExt, sync::mpsc};
    use uuid::Uuid;

    #[tokio::test]
    async fn stalled_metadata_probe_does_not_block_admission_or_release_slot_early() {
        let root = std::env::temp_dir().join(format!("ccp-list-test-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("state")).unwrap();
        let store = Arc::new(Store::open(&root.join("state/router.sqlite3")).unwrap());
        for tool in ["agy", "claude"] {
            let home = root.join(tool);
            fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
            let c = store
                .register(&json!({"tool":tool,"label":"fixture","home":home}))
                .unwrap();
            store
                .edit(c.id, &json!({"enabled":true,"revision":c.revision}))
                .unwrap();
        }
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = Arc::new(slots.clone().try_acquire_owned().unwrap());
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, paused) = mpsc::channel();
        let db = store.clone();
        let job = tokio::spawn(async move {
            browse_task(&permit, move || {
                let mut entered = Some(entered);
                snapshot(&db, move |_| {
                    entered.take().unwrap().send(()).unwrap();
                    paused.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
            })
            .await
        });
        waiting.await.unwrap();
        job.abort(); // Same cancellation as the browse deadline expiring.
        let _ = job.await;
        assert_eq!(slots.available_permits(), 0);
        let bin = std::env::current_exe().unwrap();
        let build = Build {
            fingerprint: stamp(&bin),
            bin: bin.clone(),
            version: "fixture".into(),
            supported: true,
        };
        let req = Request {
            tool: "claude".into(),
            argv: vec![],
            binary: Some(bin),
            cwd: root.clone(),
            request_id: Uuid::new_v4().to_string(),
            ticket: "a1".repeat(32),
            login_profile: None,
            override_names: vec![],
        };
        let admission = timeout(
            Duration::from_secs(2),
            blocking(move || launch(&store, &build, &req)),
        )
        .await;
        release.send(()).unwrap();
        assert!(
            admission
                .expect("browse metadata held the store lock")
                .is_ok()
        );
        drop(
            timeout(Duration::from_secs(2), slots.acquire())
                .await
                .unwrap()
                .unwrap(),
        );
        fs::remove_dir_all(root).unwrap();
    }
}
