use crate::types::{Error, Result, now, tool};
use rusqlite::{
    Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter,
    types::Value as SqlValue,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};
use url::Url;

pub struct Store {
    db: Mutex<Connection>,
    state_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub id: i64,
    pub tool: String,
    pub label: String,
    pub home: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub enabled: bool,
    pub current: bool,
    pub revision: i64,
    pub mode: String,
    pub http: Option<String>,
    pub https: Option<String>,
    pub observer: Option<String>,
}
const FIELDS: &str = "id,tool,label,home,dev,ino,enabled,current,revision,proxy_mode,http_proxy,https_proxy,observer_stamp";
fn row(r: &Row) -> rusqlite::Result<Config> {
    Ok(Config {
        id: r.get(0)?,
        tool: r.get(1)?,
        label: r.get(2)?,
        home: PathBuf::from(r.get::<_, String>(3)?),
        dev: r.get::<_, i64>(4)? as u64,
        ino: r.get::<_, i64>(5)? as u64,
        enabled: r.get(6)?,
        current: r.get(7)?,
        revision: r.get(8)?,
        mode: r.get(9)?,
        http: r.get(10)?,
        https: r.get(11)?,
        observer: r.get(12)?,
    })
}
impl Config {
    pub fn present(&self) -> bool {
        fs::symlink_metadata(&self.home)
            .is_ok_and(|m| m.is_dir() && m.dev() == self.dev && m.ino() == self.ino)
            && self.home.canonicalize().is_ok_and(|p| p == self.home)
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
            "current":self.current,"revision":self.revision,"present":self.present(),"proxy_mode":self.mode,
            "http_proxy":redact(&self.http),"https_proxy":redact(&self.https)})
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
fn proxy_settings(
    v: &Value,
    c: Option<&Config>,
) -> Result<(String, Option<String>, Option<String>)> {
    let mode = match v.get("proxy_mode") {
        Some(v) => v.as_str().unwrap_or(""),
        None => c.map(|c| c.mode.as_str()).unwrap_or("inherit"),
    };
    if !matches!(mode, "inherit" | "custom" | "direct") {
        return Err(Error::bad("bad_proxy", "Unknown proxy mode."));
    }
    let http = v
        .get("http_proxy")
        .map(proxy)
        .transpose()?
        .unwrap_or_else(|| c.and_then(|c| c.http.clone()));
    let https = v
        .get("https_proxy")
        .map(proxy)
        .transpose()?
        .unwrap_or_else(|| c.and_then(|c| c.https.clone()));
    if mode == "custom" && http.is_none() && https.is_none() {
        return Err(Error::bad(
            "bad_proxy",
            "Set an HTTP or HTTPS proxy for Custom mode.",
        ));
    }
    Ok(if mode == "custom" {
        (mode.to_owned(), http, https)
    } else {
        (mode.to_owned(), None, None)
    })
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
pub fn directory(path: &Path, state: &Path) -> Result<(u64, u64)> {
    let bad = || {
        Error::bad(
            "bad_home",
            "Use an existing, user-owned physical config directory, not a symlink, home, or service directory.",
        )
    };
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
        return Err(bad());
    }
    let m = fs::symlink_metadata(path).map_err(|_| bad())?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
        return Err(bad());
    }
    Ok((m.dev(), m.ino()))
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )?;
        let v: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v == 0 {
            if path.with_file_name("initialized").try_exists()? {
                return Err(Error::unavailable(
                    "missing_schema",
                    "Previously initialized router database has no schema. Restore it from backup.",
                ));
            }
            db.execute_batch("BEGIN IMMEDIATE;")?;
            db.execute_batch(include_str!("schema.sql"))?;
            db.execute_batch("COMMIT;")?;
        } else if v != 1 {
            return Err(Error::unavailable(
                "schema",
                "Unsupported router database version.",
            ));
        }
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
    pub fn register(&self, v: &Value) -> Result<Config> {
        let t = tool(v["tool"].as_str().unwrap_or(""))?;
        let name = label(v["label"].as_str().unwrap_or(""))?;
        let home = v["home"].as_str().unwrap_or("");
        let (dev, ino) = directory(Path::new(home), &self.state_dir)?;
        let (mode, http, https) = proxy_settings(v, None)?;
        self.write(|db|{
            db.execute("INSERT INTO configs(tool,label,home,dev,ino,proxy_mode,http_proxy,https_proxy) VALUES(?,?,?,?,?,?,?,?)",
                params![t,name,home,dev as i64,ino as i64,mode,http,https])?;
            config(db,db.last_insert_rowid())
        })
    }
    pub fn edit(&self, id: i64, v: &Value) -> Result<Config> {
        self.write(|db|{
            let c=config(db,id)?; revision(v,&c)?;
            let name=label(v.get("label").map(|s|s.as_str().unwrap_or("")).unwrap_or(&c.label))?;
            let enabled=match v.get("enabled"){Some(v)=>v.as_bool().ok_or_else(||Error::bad("bad_enabled","Expected a boolean."))?,None=>c.enabled};
            let (mode,http,https)=proxy_settings(v,Some(&c))?;
            db.execute("UPDATE configs SET label=?,enabled=?,current=current AND ?,proxy_mode=?,http_proxy=?,https_proxy=?,revision=revision+1 WHERE id=?",
                params![name,enabled,enabled,mode,http,https,id])?;
            config(db,id)
        })
    }
    pub fn current(&self, t: &str, id: Option<i64>) -> Result<()> {
        tool(t)?;
        self.write(|db|{
            if let Some(id)=id {let c=config(db,id)?;
                if c.tool!=t || !c.enabled || !c.present(){return Err(Error::conflict("not_eligible","Select an enabled, present config for this tool."))}
            }
            db.execute("UPDATE configs SET current=0,revision=revision+1 WHERE tool=? AND current=1 AND id IS NOT ?",params![t,id])?;
            if let Some(id)=id {db.execute("UPDATE configs SET current=1,revision=revision+1 WHERE id=? AND current=0",[id])?;}
            Ok(())
        })
    }
    pub fn prune(&self) -> Result<()> {
        self.write(|db|{
            db.execute("UPDATE launches SET state='unobserved',reason='observer_timeout' WHERE state='pending' AND created_ms<?",[now()-86_400_000])?;
            db.execute("DELETE FROM launches WHERE created_ms<?",[now()-90*86_400_000])?; Ok(())
        })
    }
    pub fn page(&self, sessions: bool, q: &Page) -> Result<Value> {
        let (table, owner, sql) = if sessions {
            (
                "sessions",
                "owner",
                "SELECT s.seq,s.tool,s.native_id,s.owner,c.label,c.enabled,s.evidence,s.bound_ms,s.last_routed_ms,s.workspace",
            )
        } else {
            (
                "launches",
                "config_id",
                "SELECT s.seq,s.tool,s.native_id,s.config_id,c.label,c.enabled,s.intent,s.state,s.reason,s.created_ms,s.workspace,s.request_id,s.observed_ms",
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
            if sessions {v["evidence"]=r.get::<_,String>(6)?.into();v["bound_ms"]=r.get::<_,i64>(7)?.into();v["last_routed_ms"]=r.get::<_,i64>(8)?.into();v["workspace"]=r.get::<_,String>(9)?.into();}
            else{for (i,k) in [(6,"intent"),(7,"state"),(8,"reason"),(10,"workspace"),(11,"request_id")]{v[k]=r.get::<_,Option<String>>(i)?.into();}
                 v["created_ms"]=r.get::<_,i64>(9)?.into();v["observed_ms"]=r.get::<_,Option<i64>>(12)?.into();}
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
