use crate::{
    protocol::RoutingMode,
    types::{Error, Result, now, tool},
};
use rusqlite::{
    Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter,
    types::Value as SqlValue,
};
use rustix::fs::{AtFlags, CWD, FileType, StatxFlags, makedev, statx};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};
use url::Url;

pub struct Store {
    db: Mutex<Connection>,
    state_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub id: i64,
    pub tool: String,
    pub label: String,
    pub home: PathBuf,
    pub dev: u64,
    pub ino: u64,
    birth_ns: Option<i64>,
    pub enabled: bool,
    pub is_default_account: bool,
    pub priority: i64,
    pub revision: i64,
    pub mode: String,
    pub proxy_url: Option<String>,
}
const FIELDS: &str =
    "id,tool,label,home,dev,ino,enabled,current,revision,proxy_mode,proxy_url,birth_ns,priority";
fn row(r: &Row) -> rusqlite::Result<Config> {
    Ok(Config {
        id: r.get(0)?,
        tool: r.get(1)?,
        label: r.get(2)?,
        home: Path::new(&r.get::<_, String>(3)?).components().collect(),
        dev: r.get::<_, i64>(4)? as u64,
        ino: r.get::<_, i64>(5)? as u64,
        enabled: r.get(6)?,
        is_default_account: r.get(7)?,
        revision: r.get(8)?,
        mode: r.get(9)?,
        proxy_url: r.get(10)?,
        birth_ns: r.get(11)?,
        priority: r.get(12)?,
    })
}
impl Config {
    fn observed_birth(&self) -> Option<i64> {
        let (dev, ino, birth) = directory_identity(&self.home).ok()?;
        (dev == self.dev && ino == self.ino && self.home.canonicalize().ok()? == self.home)
            .then_some(birth)
    }
    pub(crate) fn directory_matches(&self) -> bool {
        self.birth_ns.is_some() && self.observed_birth() == self.birth_ns
    }
    pub(crate) fn matches_open_home(&self, fd: &std::os::fd::OwnedFd) -> bool {
        directory_identity_at(fd, Path::new(""), AtFlags::EMPTY_PATH).is_ok_and(
            |(dev, ino, birth)| (dev, ino, Some(birth)) == (self.dev, self.ino, self.birth_ns),
        )
    }
    pub fn present(&self) -> bool {
        self.directory_matches()
            && (self.tool != "agy" || crate::agy::has_login(&self.home).is_ok())
    }
    pub fn public(&self) -> Value {
        fn redact(raw: &Option<String>) -> Value {
            raw.as_ref()
                .and_then(|s| Url::parse(s).ok())
                .map(|mut url| {
                    let saved = !url.username().is_empty() || url.password().is_some();
                    let _ = url.set_password(None);
                    let _ = url.set_username("");
                    json!({"url":url.as_str(),"credentials_saved":saved})
                })
                .unwrap_or(Value::Null)
        }
        json!({"id":self.id,"tool":self.tool,"label":self.label,"home":self.home,"enabled":self.enabled,
            "current":self.is_default_account,"priority":self.priority,"revision":self.revision,"present":self.present(),"proxy_mode":self.mode,
            "proxy_url":redact(&self.proxy_url)})
    }
}
pub fn routing_mode(db: &Connection) -> Result<RoutingMode> {
    let s: String = db.query_row("SELECT routing_mode FROM settings WHERE id=1", [], |r| {
        r.get(0)
    })?;
    RoutingMode::parse(&s)
        .ok_or_else(|| Error::unavailable("schema", "Invalid stored routing mode."))
}
fn priority(v: &Value, default: i64) -> Result<i64> {
    match v.get("priority") {
        None => Ok(default),
        Some(v) => v.as_i64().filter(|p| (0..=100).contains(p)).ok_or_else(|| {
            Error::bad("bad_priority", "Priority must be an integer from 0 to 100.")
        }),
    }
}
pub fn config(db: &Connection, id: i64) -> Result<Config> {
    db.query_row(
        &format!("SELECT {FIELDS} FROM configs WHERE id=?"),
        [id],
        row,
    )
    .optional()?
    .ok_or_else(|| Error::missing("unknown_config", "Config no longer exists."))
}
pub fn configs(db: &Connection, t: &str) -> Result<Vec<Config>> {
    Ok(db
        .prepare(&format!(
            "SELECT {FIELDS} FROM configs WHERE tool=? ORDER BY id"
        ))?
        .query_map([tool(t)?], row)?
        .collect::<rusqlite::Result<_>>()?)
}
pub(crate) fn enabled_configs(db: &Connection, tool: &str, limit: usize) -> Result<Vec<Config>> {
    Ok(db
        .prepare_cached(&format!(
            "SELECT {FIELDS} FROM configs WHERE tool=? AND enabled=1 ORDER BY id LIMIT ?"
        ))?
        .query_map(params![tool, limit], row)?
        .collect::<rusqlite::Result<_>>()?)
}
fn label(raw: &str) -> Result<&str> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 100 || s.chars().any(char::is_control) {
        Err(Error::bad(
            "bad_label",
            "Use a nonempty label of at most 100 bytes without control characters.",
        ))
    } else {
        Ok(s)
    }
}
fn proxy(raw: &Value) -> Result<Option<String>> {
    if raw.is_null() {
        return Ok(None);
    }
    let bad = || {
        Error::bad(
            "bad_proxy",
            "Expected an HTTP(S) proxy URL without a path, query or fragment.",
        )
    };
    let s = raw.as_str().ok_or_else(bad)?;
    if s.len() > 2048
        || s.chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(bad());
    }
    let u = Url::parse(s).map_err(|_| bad())?;
    if !matches!(u.scheme(), "http" | "https")
        || u.host().is_none()
        || u.path() != "/"
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(bad());
    }
    Ok(Some(u.to_string()))
}
fn proxy_settings(v: &Value, c: Option<&Config>) -> Result<(String, Option<String>)> {
    let mode = match v.get("proxy_mode") {
        Some(v) => v.as_str().unwrap_or(""),
        None => c.map(|c| c.mode.as_str()).unwrap_or("inherit"),
    };
    if !matches!(mode, "inherit" | "custom" | "direct") {
        return Err(Error::bad("bad_proxy", "Unknown proxy mode."));
    }
    let url = v
        .get("proxy_url")
        .map(proxy)
        .transpose()?
        .unwrap_or_else(|| c.and_then(|c| c.proxy_url.clone()));
    if mode == "custom" && url.is_none() {
        return Err(Error::bad("bad_proxy", "Set a proxy URL for Custom mode."));
    }
    Ok((mode.to_owned(), if mode == "custom" { url } else { None }))
}
pub fn revision(v: &Value, c: &Config) -> Result<()> {
    if v.get("revision").and_then(Value::as_i64) != Some(c.revision) {
        Err(Error::conflict(
            "stale_config",
            "The config changed. Reload before editing.",
        ))
    } else {
        Ok(())
    }
}
fn bad_home() -> Error {
    Error::bad(
        "bad_home",
        "Use an existing, user-owned physical config directory, not a symlink, home, or service directory.",
    )
}
fn directory_identity(path: &Path) -> Result<(u64, u64, i64)> {
    directory_identity_at(CWD, path, AtFlags::SYMLINK_NOFOLLOW)
}
fn directory_identity_at(
    fd: impl std::os::fd::AsFd,
    path: &Path,
    flags: AtFlags,
) -> Result<(u64, u64, i64)> {
    // Metadata::created() is unavailable on musl. Read the full identity in one statx call.
    let basic = StatxFlags::TYPE | StatxFlags::UID | StatxFlags::INO;
    let m = statx(fd, path, flags, basic | StatxFlags::BTIME).map_err(|_| bad_home())?;
    let fields = StatxFlags::from_bits_retain(m.stx_mask);
    if !fields.contains(basic)
        || FileType::from_raw_mode(m.stx_mode.into()) != FileType::Directory
        || m.stx_uid != crate::paths::uid()
    {
        return Err(bad_home());
    }
    let t = m.stx_btime;
    let birth = t.tv_sec.checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(i64::from(t.tv_nsec)))
        .filter(|&n| n >= 0 && fields.contains(StatxFlags::BTIME))
        .ok_or_else(|| Error::bad("home_identity", "Use a filesystem that reports a directory creation time so ccp can detect replaced configs."))?;
    Ok((makedev(m.stx_dev_major, m.stx_dev_minor), m.stx_ino, birth))
}
fn directory(path: &Path, state: &Path) -> Result<(u64, u64, i64)> {
    if !path.is_absolute()
        || path.canonicalize().ok().as_deref() != Some(path)
        || path.parent().is_none()
        || path
            .to_str()
            .is_none_or(|p| p.chars().any(char::is_control))
        || std::env::var_os("HOME")
            .as_deref()
            .is_some_and(|h| Path::new(h).starts_with(path))
        || path.starts_with(state)
        || state.starts_with(path)
        || crate::paths::runtime_dir()
            .is_ok_and(|runtime| path.starts_with(&runtime) || runtime.starts_with(path))
    {
        return Err(bad_home());
    }
    directory_identity(path)
}
impl Store {
    pub fn user_db_path(state: &Path) -> PathBuf {
        state.join("router.sqlite3")
    }
    pub fn open_user(state: &Path) -> Result<Self> {
        crate::paths::private_dir(state)?;
        let path = Self::user_db_path(state);
        let marker = state.join("initialized");
        if marker.try_exists()? && !path.try_exists()? {
            return Err(Error::unavailable(
                "missing_database",
                "Database is missing; restore it before starting ccpd.",
            ));
        }
        if path.symlink_metadata().is_ok_and(|m| !m.is_file()) {
            return Err(Error::denied());
        }
        let store = Self::open(&path)?;
        if !marker.try_exists()?
            && let Err(e) = crate::paths::replace(&marker, b"router initialized\n", None)
            && !marker.symlink_metadata().is_ok_and(|m| m.is_file())
        {
            return Err(e);
        }
        Ok(store)
    }
    pub fn open(path: &Path) -> Result<Self> {
        let mut db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.execute_batch(
            "PRAGMA foreign_keys=OFF; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let v: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v == 0 {
            if path.with_file_name("initialized").try_exists()? {
                return Err(Error::unavailable(
                    "missing_schema",
                    "Previously initialized router database has no schema. Restore it from backup.",
                ));
            }
            tx.execute_batch(include_str!("schema.sql"))?;
        } else if !(1..=18).contains(&v)
            || !tx
                .prepare("SELECT 1 FROM pragma_table_info('configs') WHERE name='proxy_url'")?
                .exists([])?
        {
            return Err(Error::unavailable(
                "schema",
                "Unsupported router database schema.",
            ));
        }
        if v == 0 || v == 1 {
            tx.execute_batch(include_str!("panel_password.sql"))?;
        }
        if (0..=2).contains(&v) {
            tx.execute_batch(
                "ALTER TABLE configs ADD COLUMN birth_ns INTEGER; PRAGMA user_version=3;",
            )?;
        }
        if v < 4 {
            tx.execute_batch(include_str!("directory_identity.sql"))?;
            if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
                return Err(Error::unavailable(
                    "schema",
                    "Config migration found inconsistent references. Restore the database from backup.",
                ));
            }
        }
        if v < 5 {
            // Old callbacks cannot complete pending launches on protocol v2.
            // Completed bindings and every user's native profile stay intact.
            tx.execute_batch(
                "DROP TABLE IF EXISTS installed_observers;
                ALTER TABLE configs DROP COLUMN observer_stamp;
                UPDATE launches SET state='unobserved',reason='router_upgraded'
                WHERE tool='codex' AND state='pending';
                PRAGMA user_version=5;",
            )?;
        }
        if v < 7 {
            tx.execute_batch(include_str!("antigravity.sql"))?;
            if tx.prepare("PRAGMA foreign_key_check")?.exists([])? {
                return Err(Error::unavailable(
                    "schema",
                    "Profile migration found inconsistent session references.",
                ));
            }
        }
        if v < 8 {
            tx.execute_batch(include_str!("admission.sql"))?;
        }
        if v < 9 {
            tx.execute_batch(
                "CREATE TABLE tool_cursors (tool TEXT PRIMARY KEY, cursor INTEGER NOT NULL DEFAULT 0);
                INSERT INTO tool_cursors SELECT tool,cursor FROM tools;
                DROP TABLE tools;
                ALTER TABLE tool_cursors RENAME TO tools;
                PRAGMA user_version=9;",
            )?;
        }
        if v < 10 {
            tx.execute_batch(include_str!("saved_file_sessions.sql"))?;
        }
        if v < 11 {
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN tui_workspace TEXT;
                 UPDATE sessions SET tui_workspace=workspace WHERE evidence='saved_file';
                 PRAGMA user_version=11;",
            )?;
        }
        if v < 12 {
            tx.execute_batch(include_str!("reset_grants.sql"))?;
        }
        if v < 13 {
            tx.execute_batch(include_str!("discovered_sessions.sql"))?;
        }
        if v < 14 {
            tx.execute_batch(include_str!("earliest_expiry.sql"))?;
        }
        if v < 15 {
            tx.execute_batch(include_str!("rollout_sessions.sql"))?;
        }
        if v < 16 {
            tx.execute_batch(include_str!("acp_listed_sessions.sql"))?;
        }
        if v < 17 {
            tx.execute_batch(include_str!("split_expiry.sql"))?;
        }
        if v < 18 {
            tx.execute_batch(include_str!("codex_listed_sessions.sql"))?;
        }
        // Older databases know only the device and inode. Pin creation time once,
        // including when an original home was offline during the upgrade.
        let legacy = tx
            .prepare(&format!(
                "SELECT {FIELDS} FROM configs WHERE birth_ns IS NULL"
            ))?
            .query_map([], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for c in legacy {
            if let Some(birth) = c.observed_birth() {
                tx.execute(
                    "UPDATE configs SET birth_ns=?1 WHERE id=?2 AND NOT EXISTS (
                        SELECT 1 FROM configs other WHERE other.dev=configs.dev
                        AND other.ino=configs.ino AND other.birth_ns=?1)",
                    params![birth, c.id],
                )?;
            }
        }
        tx.commit()?;
        db.execute_batch("PRAGMA foreign_keys=ON")?;
        Ok(Self {
            db: Mutex::new(db),
            state_dir: path.parent().unwrap_or(Path::new(".")).to_owned(),
        })
    }
    pub fn db(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| Error::unavailable("database", "Database lock unavailable."))
    }
    pub fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }
    pub fn create_profile(&self, mut v: Value) -> Result<Config> {
        use std::os::unix::fs::DirBuilderExt;
        if v.get("home").is_some() {
            return Err(Error::bad(
                "bad_home",
                "Choose either a new profile or an existing directory.",
            ));
        }
        label(v["label"].as_str().unwrap_or(""))?;
        priority(&v, 0)?;
        proxy_settings(&v, None)?;
        let root = crate::paths::profile_root(v["tool"].as_str().unwrap_or(""))?;
        crate::paths::private_dir(&root)?;
        let home = root.join(uuid::Uuid::new_v4().to_string());
        std::fs::DirBuilder::new().mode(0o700).create(&home)?;
        v["home"] = json!(home);
        let result = self.register(&v);
        if result.is_err() {
            let _ = std::fs::remove_dir(&home);
        }
        result
    }
    pub fn set_routing_mode(&self, mode: RoutingMode) -> Result<()> {
        self.write(|db| {
            db.execute(
                "UPDATE settings SET routing_mode=? WHERE id=1",
                [mode.name()],
            )?;
            Ok(())
        })
    }
    pub fn register(&self, v: &Value) -> Result<Config> {
        let t = tool(v["tool"].as_str().unwrap_or(""))?;
        let name = label(v["label"].as_str().unwrap_or(""))?;
        let requested = Path::new(v["home"].as_str().unwrap_or(""));
        let (dev, ino, birth) = directory(requested, &self.state_dir)?;
        if t == "agy" {
            crate::agy::has_login(requested)?;
        }
        let normalized: PathBuf = requested.components().collect();
        let home = normalized.to_str().ok_or_else(Error::denied)?;
        let (mode, url) = proxy_settings(v, None)?;
        let priority = priority(v, 0)?;
        self.write(|db|{
            let legacy: Option<String> = db.query_row(
                "SELECT home FROM configs WHERE dev=? AND ino=? AND birth_ns IS NULL LIMIT 1",
                params![dev as i64,ino as i64], |r| r.get(0)).optional()?;
            if let Some(path) = legacy {
                return Err(Error::conflict("legacy_home",format!("An older config at {path} has not been verified. Restore its original directory and restart ccpd, or register a separate new directory.")));
            }
            // Legacy rows may contain a trailing slash or an embedded "./".
            for tool in crate::types::TOOLS {
                if configs(db,tool)?.iter().any(|c| c.home == normalized && (c.dev != dev || c.ino != ino || c.birth_ns != Some(birth))) {
                return Err(Error::conflict("home_replaced", format!(
                    "{home} belongs to a registered config whose directory was replaced. Restore the original directory to keep its sessions, or move the replacement to a new path and register it there."
                )));
                }
            }
            db.execute("INSERT INTO configs(tool,label,home,dev,ino,birth_ns,proxy_mode,proxy_url,priority) VALUES(?,?,?,?,?,?,?,?,?)",
                params![t,name,home,dev as i64,ino as i64,birth,mode,url,priority])?;
            config(db,db.last_insert_rowid())
        })
    }
    pub fn edit(&self, id: i64, v: &Value) -> Result<Config> {
        self.write(|db|{
            let c=config(db,id)?; revision(v,&c)?;
            let name=label(v.get("label").map(|s|s.as_str().unwrap_or("")).unwrap_or(&c.label))?;
            let enabled=match v.get("enabled"){Some(v)=>v.as_bool().ok_or_else(||Error::bad("bad_enabled","Expected a boolean."))?,None=>c.enabled};
            let (mode,url)=proxy_settings(v,Some(&c))?;
            let priority=priority(v,c.priority)?;
            db.execute("UPDATE configs SET label=?,enabled=?,current=current AND ?,proxy_mode=?,proxy_url=?,priority=?,revision=revision+1 WHERE id=?",
                params![name,enabled,enabled,mode,url,priority,id])?;
            config(db,id)
        })
    }
    pub fn set_default_account(&self, t: &str, id: Option<i64>) -> Result<()> {
        tool(t)?;
        self.write(|db|{
            if let Some(id) = id {
                let c = config(db, id)?;
                if c.tool != t || !c.enabled || !c.present() {
                    return Err(Error::conflict("not_eligible",
                        "To set a default account, choose an enabled profile for this provider with its original directory available."));
                }
            }
            db.execute("UPDATE configs SET current=0,revision=revision+1 WHERE tool=? AND current=1 AND id IS NOT ?",params![t,id])?;
            if let Some(id)=id {db.execute("UPDATE configs SET current=1,revision=revision+1 WHERE id=? AND current=0",[id])?;}
            Ok(())
        })
    }
    pub fn prune(&self) -> Result<()> {
        self.write(|db|{
            // Keep successful receipts and unresolved keys; old unconfirmed offers/definite negatives can be relisted.
            db.execute("DELETE FROM reset_grants WHERE seen_ms<? AND (attempt IS NULL OR outcome IN ('nothingToReset','noCredit'))", [now()-86_400_000])?;
            db.execute("UPDATE launches SET state='unobserved',reason='thread_id_timeout' WHERE state='pending' AND created_ms<?",[now()-86_400_000])?;
            db.execute("DELETE FROM launches WHERE created_ms<?",[now()-90*86_400_000])?; Ok(())
        })
    }
    pub fn page(&self, sessions: bool, q: &Page) -> Result<Value> {
        let (table, owner, sql) = if sessions {
            (
                "sessions",
                "owner",
                "SELECT s.seq,s.tool,s.native_id,s.owner,c.label,c.enabled,s.evidence,s.bound_ms,s.last_routed_ms,s.workspace,s.tui_workspace,s.saved_file_seen_ms",
            )
        } else {
            (
                "launches",
                "config_id",
                "SELECT s.seq,s.tool,s.native_id,s.config_id,c.label,c.enabled,s.intent,s.state,s.reason,s.created_ms,s.workspace,s.request_id,s.observed_ms,
                 EXISTS(SELECT 1 FROM sessions m WHERE m.tool=s.tool AND m.native_id=s.native_id AND m.owner=s.config_id)",
            )
        };
        let mut sql =
            format!("{sql} FROM {table} s LEFT JOIN configs c ON c.id=s.{owner} WHERE s.seq < ?");
        let mut args: Vec<SqlValue> = vec![q.before.unwrap_or(i64::MAX).into()];
        for (col, val) in [
            ("tool", q.tool.as_deref()),
            ("native_id", q.native_id.as_deref()),
            ("state", if sessions { None } else { q.state.as_deref() }),
        ] {
            if let Some(v) = val {
                if col == "tool" {
                    tool(v)?;
                }
                sql.push_str(&format!(" AND s.{col}=?"));
                args.push(v.to_owned().into());
            }
        }
        if let Some(id) = q.config_id {
            sql.push_str(&format!(" AND s.{owner}=?"));
            args.push(id.into());
        }
        let count = q.limit.unwrap_or(25).clamp(1, 100);
        sql.push_str(" ORDER BY s.seq DESC LIMIT ?");
        args.push((count + 1).into());
        let db = self.db()?;
        let mut items=db.prepare(&sql)?.query_map(params_from_iter(args),|r|{
            let mut v=json!({"seq":r.get::<_,i64>(0)?,"tool":r.get::<_,String>(1)?,"native_id":r.get::<_,Option<String>>(2)?,
                "config_id":r.get::<_,Option<i64>>(3)?,"label":r.get::<_,Option<String>>(4)?,"enabled":r.get::<_,Option<bool>>(5)?});
            if sessions {v["evidence"]=r.get::<_,String>(6)?.into();v["bound_ms"]=r.get::<_,i64>(7)?.into();v["last_routed_ms"]=r.get::<_,Option<i64>>(8)?.into();v["workspace"]=r.get::<_,Option<String>>(9)?.into();v["tui_workspace"]=r.get::<_,Option<String>>(10)?.into();
                if v["tool"]=="agy" || v["tool"]=="codex" {v["saved_file_seen_ms"]=r.get::<_,Option<i64>>(11)?.into();}}
            else{for (i,k) in [(6,"intent"),(7,"state"),(8,"reason"),(10,"workspace"),(11,"request_id")]{v[k]=r.get::<_,Option<String>>(i)?.into();}
                 v["created_ms"]=r.get::<_,i64>(9)?.into();v["observed_ms"]=r.get::<_,Option<i64>>(12)?.into();v["mapped"]=r.get::<_,bool>(13)?.into();}
            Ok(v)
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        let cursor = if items.len() > count as usize {
            items.pop();
            items.last().map(|v| v["seq"].clone())
        } else {
            None
        };
        Ok(json!({"items":items,"next_cursor":cursor}))
    }
}
#[derive(Default, Deserialize)]
pub struct Page {
    pub tool: Option<String>,
    pub native_id: Option<String>,
    pub config_id: Option<i64>,
    pub state: Option<String>,
    pub before: Option<i64>,
    pub limit: Option<i64>,
}
