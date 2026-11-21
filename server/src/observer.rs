use crate::{
    native::Build,
    paths, rpc,
    store::{Config, Store},
    types::{Error, Result},
};
use serde_json::{Value, json};
use std::{fs, path::Path};

pub fn command() -> Result<String> {
    let p = std::env::current_exe()?.canonicalize()?;
    let s = p.to_str().ok_or_else(Error::denied)?;
    Ok(format!("'{}' observe", s.replace('\'', "'\"'\"'")))
}
pub fn spec() -> Result<Value> {
    Ok(json!({"matcher":"startup","hooks":[{"type":"command","command":command()?}]}))
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
pub fn install(c: &Config) -> Result<()> {
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
    let entry = spec()?;
    if events.contains(&entry) {
        return Ok(());
    }
    events.push(entry);
    let mut data = serde_json::to_vec_pretty(&value).map_err(|_| bad())?;
    data.push(b'\n');
    paths::replace(&path, &data, prior.as_deref())
}
pub async fn check(store: &Store, build: &Build, c: &Config, cwd: &Path) -> Result<()> {
    target(c)?;
    if !build.supported {
        return Err(crate::native::unsupported_build());
    }
    let before = build.observer_stamp(c);
    let response = rpc::codex(build, c, cwd, "hooks/list", json!({"cwds":[cwd]})).await;
    let cmd = command()?;
    let path = c.home.join("hooks.json");
    let valid = response.as_ref().is_ok_and(|v| {
        v["data"].as_array().is_some_and(|a| {
            a.iter().any(|entry| {
                entry["errors"].as_array().is_some_and(Vec::is_empty)
                    && entry["hooks"].as_array().is_some_and(|h| {
                        h.iter().any(|h| {
                            h["eventName"] == "sessionStart"
                                && h["handlerType"] == "command"
                                && h["command"] == cmd
                                && h["enabled"] == true
                                && h["matcher"] == "startup"
                                && h["source"] == "user"
                                && h["sourcePath"].as_str() == path.to_str()
                                && h["trustStatus"] == "trusted"
                                && h["currentHash"].as_str().is_some_and(|s| !s.is_empty())
                        })
                    })
            })
        })
    });
    let valid = valid && before == build.observer_stamp(c);
    store.write(|db| {
        if db.execute(
            "UPDATE configs SET observer_stamp=? WHERE id=? AND revision=?",
            rusqlite::params![valid.then_some(&before), c.id, c.revision],
        )? != 1
        {
            return Err(Error::conflict(
                "stale_config",
                "Config changed. Check observer again.",
            ));
        }
        Ok(())
    })?;
    response?;
    if !valid {
        return Err(Error::conflict(
            "observer_required",
            "Approve the startup observer in the native /hooks screen, then check again.",
        ));
    }
    Ok(())
}
