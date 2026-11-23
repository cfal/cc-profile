use super::service::{Shared, blocking};
use axum::{Json, extract::State};
use ccpd::{
    agy::{self, acp},
    native::{clean_command, unsupported_build, validate_binary},
    store::{Config, config, configs},
    types::{Error, Result, now},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::BufReader, time::timeout};

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

type Roster = Vec<(Config, Option<agy::LoginStamp>)>;
fn roster(db: &Connection) -> Result<Roster> {
    let mut result = Vec::new();
    for c in configs(db, "agy")?.into_iter().filter(|c| c.enabled) {
        if result.len() >= 32 {
            return Err(limit());
        }
        if !c.present() {
            return Err(unavailable());
        }
        let stamp = agy::login_stamp(&c.home)?;
        result.push((c, stamp));
    }
    Ok(result)
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
        if self.rows.contains_key(id) {
            return Err(Error::conflict(
                "ambiguous_session",
                "Duplicate AgY session ID; no browse results were published.",
            ));
        }
        self.size += row.to_string().len() + 1;
        if self.size > 400 * 1024 || self.rows.len() >= 2048 {
            return Err(limit());
        }
        self.rows.insert(id.into(), (owner, row));
        Ok(())
    }

    fn publish(self, db: &Connection, filter: Option<&str>) -> Result<Json<Value>> {
        let mut rows = Vec::new();
        let mut prior = db.prepare_cached(
            "SELECT owner,evidence FROM sessions WHERE tool='agy' AND native_id=?",
        )?;
        for (id, (owner, row)) in self.rows {
            let cwd = row["cwd"].as_str().unwrap();
            let binding: Option<(i64, String)> = prior
                .query_row([&id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if binding.as_ref().is_some_and(|(old, _)| *old != owner) {
                return Err(Error::conflict(
                    "ambiguous_session",
                    "AgY session conflicts with its recorded owner; no browse results were published.",
                ));
            }
            if filter.is_some_and(|wanted| wanted != cwd) {
                continue;
            }
            // Native listing is ACP evidence, not a launch or a successful resume.
            match binding {
                None => {
                    db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms) VALUES('agy',?,?,'acp_listed',?,?)",
                        params![id,owner,cwd,now()])?;
                }
                Some((_, e)) if e == "saved_file" => {
                    db.execute("UPDATE sessions SET evidence='acp_listed',workspace=? WHERE tool='agy' AND native_id=?", params![cwd,id])?;
                }
                _ => {}
            }
            rows.push(row);
        }
        // Preserve the stable ID order for equal or absent timestamps.
        rows.sort_by_cached_key(|r| {
            std::cmp::Reverse(r["updatedAt"].as_str().and_then(|s| {
                time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
            }))
        });
        Ok(Json(json!({"sessions":rows})))
    }
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

async fn collect(s: &Shared, binary: &Path, profiles: &Roster) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    for (c, stamp) in profiles {
        let app = s.clone();
        let id = c.id;
        let current = blocking(move || config(&*app.store.db()?, id)).await?;
        pinned(s, binary)?;
        if current != *c || !c.present() || agy::login_stamp(&c.home)? != *stamp {
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
    pinned(&s, &q.binary)?;
    if q.cwd.as_deref().is_some_and(|s| !acp::workspace(s)) {
        return Err(Error::bad("cwd", "Expected an absolute workspace."));
    }
    let _slot = s.acp_listing.try_acquire().map_err(|_| {
        Error::unavailable(
            "acp_list_busy",
            "Another AgY browse is in progress. Try again.",
        )
    })?;
    let app = s.clone();
    let profiles = blocking(move || roster(&*app.store.db()?)).await?;
    let catalog = timeout(Duration::from_secs(30), collect(&s, &q.binary, &profiles))
        .await
        .map_err(|_| unavailable())??;
    let app = s.clone();
    blocking(move || {
        app.store.write(|db| {
            pinned(&app, &q.binary)?;
            if profiles != roster(db)? {
                return Err(unavailable());
            }
            catalog.publish(db, q.cwd.as_deref())
        })
    })
    .await
}
