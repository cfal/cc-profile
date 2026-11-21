use crate::{
    native::Build,
    paths, rpc,
    store::{Config, Store},
    types::{Error, Result},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub struct Binary {
    path: PathBuf,
    stamp: String,
    command: String,
}
impl Binary {
    fn from_path(path: PathBuf) -> Result<Self> {
        let s = path.to_str().ok_or_else(Error::denied)?;
        let command = format!("'{}' observe", s.replace('\'', "'\"'\"'"));
        Ok(Self {
            stamp: crate::native::stamp(&path),
            path,
            command,
        })
    }
    pub fn unchanged(&self) -> bool {
        !self.stamp.is_empty() && crate::native::stamp(&self.path) == self.stamp
    }
    pub fn signature(&self) -> String {
        format!("{}:{}", self.command, self.stamp)
    }
}
pub fn binary() -> Result<&'static Binary> {
    // /proc/self/exe can point at an unlinked inode after an upgrade.
    static BINARY: OnceLock<Result<Binary>> = OnceLock::new();
    BINARY
        .get_or_init(|| Binary::from_path(std::env::current_exe()?.canonicalize()?))
        .as_ref()
        .map_err(Clone::clone)
}
pub fn spec() -> Result<Value> {
    Ok(entry(&binary()?.command))
}
fn entry(command: &str) -> Value {
    json!({"matcher":"startup","hooks":[{"type":"command","command":command}]})
}
fn target(c: &Config) -> Result<()> {
    if c.tool != "codex" || !c.present() {
        return Err(Error::conflict(
            "bad_observer_home",
            "Select a present Codex config.",
        ));
    }
    Ok(())
}
pub fn install(store: &Store, c: &Config) -> Result<()> {
    use rusqlite::{OptionalExtension, params};
    let command = &binary()?.command;
    store.write(|db| {
        let old: Option<String> = db.query_row("SELECT command FROM installed_observers WHERE config_id=?", [c.id], |r| r.get(0)).optional()?;
        merge(c, old.as_deref(), command)?;
        db.execute("INSERT INTO installed_observers(config_id, command) VALUES(?,?) ON CONFLICT(config_id) DO UPDATE SET command=excluded.command",params![c.id,command])?;
        Ok(())
    })
}
fn merge(c: &Config, previous: Option<&str>, command: &str) -> Result<()> {
    target(c)?;
    let path = c.home.join("hooks.json");
    let bad = || {
        Error::conflict(
            "invalid_hooks",
            "Refusing to overwrite invalid or symlinked hooks.json.",
        )
    };
    let prior = match fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => Some(paths::read_limited(&path, 1 << 20)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        _ => return Err(bad()),
    };
    let mut value: Value = if let Some(bytes) = &prior {
        serde_json::from_slice(bytes).map_err(|_| bad())?
    } else {
        json!({})
    };
    let hooks = value
        .as_object_mut()
        .ok_or_else(bad)?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let events = hooks
        .as_object_mut()
        .ok_or_else(bad)?
        .entry("SessionStart")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(bad)?;
    let len = events.len();
    if let Some(old) = previous.filter(|old| *old != command) {
        let old = entry(old);
        events.retain(|e| *e != old);
    }
    let entry = entry(command);
    if !events.contains(&entry) {
        events.push(entry);
    } else if events.len() == len {
        return Ok(());
    }
    let mut data = serde_json::to_vec_pretty(&value).map_err(|_| bad())?;
    data.push(b'\n');
    paths::replace(&path, &data, prior.as_deref())
}
pub async fn trusted(build: &Build, c: &Config, neutral: &Path, workspace: &Path) -> Result<bool> {
    target(c)?;
    let observer = binary()?;
    if !build.supported || !observer.unchanged() {
        return Err(crate::native::unsupported_build());
    }
    let v = rpc::codex(build, c, neutral, "hooks/list", json!({"cwds":[workspace]})).await?;
    let path = c.home.join("hooks.json");
    let failed = || {
        Error::unavailable(
            "hook_metadata",
            "Codex could not inspect hooks for this workspace. Try again.",
        )
    };
    let entries = v["data"].as_array().ok_or_else(failed)?;
    let [entry] = entries.as_slice() else {
        return Err(failed());
    };
    if !entry["errors"].as_array().is_some_and(Vec::is_empty) {
        return Err(failed());
    }
    let hooks = entry["hooks"].as_array().ok_or_else(failed)?;
    Ok(hooks.iter().any(|h| {
        h["eventName"] == "sessionStart"
            && h["handlerType"] == "command"
            && h["command"] == observer.command
            && h["enabled"] == true
            && h["matcher"] == "startup"
            && h["source"] == "user"
            && h["sourcePath"].as_str() == path.to_str()
            && h["trustStatus"] == "trusted"
            && h["currentHash"].as_str().is_some_and(|s| !s.is_empty())
    }))
}
pub async fn check(store: &Store, build: &Build, c: &Config, cwd: &Path) -> Result<()> {
    let before = build.observer_stamp(c);
    // Only a completed check can confirm or withdraw an approval.
    let valid = trusted(build, c, cwd, cwd).await?;
    store.write(|db| {
        if before != build.observer_stamp(c) || !c.present() || !binary()?.unchanged() || db.execute(
            "UPDATE configs SET observer_stamp=?1 WHERE id=?2 AND revision=?3 AND (observer_stamp IS ?4 OR observer_stamp IS ?1)",
            rusqlite::params![valid.then_some(&before), c.id, c.revision, c.observer],
        )? != 1
        {
            return Err(Error::conflict(
                "stale_config",
                "Config changed. Check observer again.",
            ));
        }
        Ok(())
    })?;
    if !valid {
        return Err(Error::conflict(
            "observer_required",
            "Approve the startup observer in the native /hooks screen, then check again.",
        ));
    }
    Ok(())
}
