use crate::{
    paths::uid,
    store::{Store, configs},
    types::{Error, Result, tool},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs, io,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
};
use uuid::Uuid;

fn occupied() -> Error {
    Error::conflict(
        "link_needs_repair",
        "Compatibility path is occupied or no longer matches the managed link. Inspect its source and target in Accounts.",
    )
}
fn target(path: &Path) -> Result<Option<PathBuf>> {
    match fs::read_link(path) {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        _ => Err(occupied()),
    }
}
fn safe_dir(p: &Path) -> Result<()> {
    let m = fs::symlink_metadata(p).map_err(|_| occupied())?;
    if !m.is_dir() || m.uid() != uid() || p.canonicalize()? != p || m.mode() & 0o002 != 0 {
        return Err(occupied());
    }
    Ok(())
}
fn existing_safe_dir(p: &Path) -> Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => safe_dir(p).map(|()| true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn ensure(p: &Path) -> Result<()> {
    match fs::create_dir(p) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    };
    safe_dir(p)
}
fn relative(from: &Path, to: &Path) -> PathBuf {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let same = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in &a[same..] {
        out.push("..")
    }
    for c in &b[same..] {
        out.push(c.as_os_str())
    }
    out
}
fn link(path: &Path, want: Option<&Path>, prior: Option<&Path>) -> Result<()> {
    let existing = target(path)?;
    if existing.as_deref() == want {
        return Ok(());
    }
    if existing.is_some() && existing.as_deref() != prior {
        return Err(occupied());
    }
    match (existing, want) {
        (None, Some(to)) => symlink(to, path)?,
        (Some(_), None) => fs::remove_file(path)?,
        (Some(_), Some(to)) => {
            let tmp = path.with_file_name(format!(".ccp-link-{}", Uuid::new_v4()));
            symlink(to, &tmp)?;
            let res = if target(path).ok().flatten().as_deref() == prior {
                fs::rename(&tmp, path).map_err(Into::into)
            } else {
                Err(occupied())
            };
            let _ = fs::remove_file(tmp);
            res?;
        }
        _ => {}
    }
    Ok(())
}
struct Projection {
    root: PathBuf,
    outer: PathBuf,
    prior: Option<PathBuf>,
    entry: Option<PathBuf>,
    external: Option<(PathBuf, PathBuf)>,
}
impl Projection {
    fn load(db: &Connection, t: &str) -> Result<Option<Self>> {
        tool(t)?;
        let (r, o, p): (Option<String>, Option<String>, Option<String>) = db.query_row(
            "SELECT link_root,outer_path,projected FROM tools WHERE tool=?",
            [t],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let (Some(r), Some(o)) = (r, o) else {
            return Ok(None);
        };
        let root = PathBuf::from(r).join(t);
        let mut entry = None;
        let mut external = None;
        if let Some(c) = configs(db, t)?.into_iter().find(|c| c.current) {
            let parent = c.home.parent().unwrap();
            let name = parent.file_name();
            if parent.parent() == Some(&root)
                && c.home.file_name().is_some_and(|n| n == "config")
                && name.is_some_and(|n| n != "current" && n != ".registered")
            {
                entry = name.map(PathBuf::from);
            } else {
                let e = PathBuf::from(".registered").join(c.id.to_string());
                let dir = root.join(&e);
                external = Some((dir.join("config"), relative(&dir, &c.home)));
                entry = Some(e);
            }
        }
        Ok(Some(Self {
            root,
            outer: o.into(),
            prior: p.map(Into::into),
            entry,
            external,
        }))
    }
    fn outer_target(&self) -> PathBuf {
        relative(
            self.outer.parent().unwrap(),
            &self.root.join("current/config"),
        )
    }
    fn checked_sources(&self) -> Result<()> {
        safe_dir(self.root.parent().unwrap())?;
        safe_dir(self.outer.parent().unwrap())?;
        existing_safe_dir(&self.root).map(|_| ())
    }
    fn detach(&self) -> Result<()> {
        let has_outer = existing_safe_dir(self.outer.parent().unwrap())?;
        let has_inner =
            existing_safe_dir(self.root.parent().unwrap())? && existing_safe_dir(&self.root)?;
        let inner = self.root.join("current");
        let outer_target = self.outer_target();
        // Preflight both sources before removing either one.
        if (has_outer && target(&self.outer)?.is_some_and(|p| p != outer_target))
            || (has_inner && target(&inner)?.is_some_and(|p| Some(p) != self.prior))
        {
            return Err(occupied());
        }
        if has_outer {
            link(&self.outer, None, Some(&outer_target))?;
        }
        if has_inner {
            link(&inner, None, self.prior.as_deref())?;
        }
        Ok(())
    }
    fn view(&self) -> Value {
        let inner = self.root.join("current");
        let want = self.outer_target();
        let actual_inner = target(&inner);
        let actual_outer = target(&self.outer);
        let healthy = self.checked_sources().is_ok()
            && actual_inner.as_ref().is_ok_and(|v| v == &self.entry)
            && actual_outer
                .as_ref()
                .is_ok_and(|v| v.as_ref() == Some(&want))
            && self
                .external
                .as_ref()
                .is_none_or(|(p, to)| target(p).is_ok_and(|v| v.as_ref() == Some(to)));
        json!({"root":self.root.parent(),"outer":self.outer,"outer_target":want,"observed_outer":actual_outer.ok().flatten(),
            "current":inner,"current_target":self.entry,"observed_current":actual_inner.ok().flatten(),"needs_repair":!healthy})
    }
    fn apply(&self) -> Result<()> {
        self.checked_sources()?;
        let outer_target = self.outer_target();
        if target(&self.outer)?.is_some_and(|p| p != outer_target) {
            return Err(occupied());
        }
        ensure(&self.root)?;
        if let Some((p, to)) = &self.external {
            ensure(&self.root.join(".registered"))?;
            ensure(p.parent().unwrap())?;
            link(p, Some(to), None)?;
        }
        link(
            &self.root.join("current"),
            self.entry.as_deref(),
            self.prior.as_deref(),
        )?;
        link(&self.outer, Some(&outer_target), None)
    }
}
pub fn status(db: &Connection, t: &str) -> Result<Value> {
    Ok(Projection::load(db, t)?
        .map(|p| p.view())
        .unwrap_or(Value::Null))
}
pub fn repair(s: &Store, t: &str) -> Result<()> {
    s.write(|db| {
        if let Some(p) = Projection::load(db, t)? {
            p.apply()?;
            db.execute(
                "UPDATE tools SET projected=? WHERE tool=?",
                params![p.entry.map(|p| p.to_string_lossy().into_owned()), t],
            )?;
        }
        Ok(())
    })
}
pub fn configure(
    s: &Store,
    t: &str,
    root: Option<&str>,
    outer: Option<&str>,
    state: &Path,
    runtime: &Path,
) -> Result<()> {
    tool(t)?;
    s.write(|db| {
        if let (Some(r), Some(o)) = (root, outer) {
            let rp = Path::new(r);
            let op = Path::new(o);
            let parent = op.parent().ok_or_else(occupied)?;
            if !op.is_absolute()
                || op.file_name().is_none()
                || op != parent.join(op.file_name().unwrap())
                || r.chars().chain(o.chars()).any(char::is_control)
                || rp.parent().is_none()
                || op.starts_with(rp)
                || [state, runtime].iter().any(|p| {
                    rp.starts_with(p) || op.starts_with(p) || p.starts_with(rp) || p.starts_with(op)
                })
            {
                return Err(occupied());
            }
            safe_dir(rp)?;
            safe_dir(parent)?;
            for t in ["claude", "codex"] {
                for c in configs(db, t)? {
                    if op.starts_with(&c.home) || c.home.starts_with(op) || rp.starts_with(&c.home)
                    {
                        return Err(occupied());
                    }
                }
            }
            if let Some(old) = Projection::load(db, t)?
                && (old.root.parent() != Some(rp) || old.outer != op)
            {
                return Err(Error::conflict(
                    "links_configured",
                    "Disable the existing projection before changing its locations.",
                ));
            }
            let expected = relative(parent, &rp.join(t).join("current/config"));
            if target(op)?.is_some_and(|v| v != expected) {
                return Err(occupied());
            }
            db.execute(
                "UPDATE tools SET link_root=?,outer_path=? WHERE tool=?",
                params![r, o, t],
            )?;
        } else if root.is_none() && outer.is_none() {
            if let Some(p) = Projection::load(db, t)? {
                p.detach()?;
                db.execute(
                    "UPDATE tools SET link_root=NULL,outer_path=NULL,projected=NULL WHERE tool=?",
                    [t],
                )?;
            }
        } else {
            return Err(Error::bad(
                "link_paths",
                "Supply both compatibility paths, or clear both.",
            ));
        }
        Ok(())
    })?;
    repair(s, t)
}
