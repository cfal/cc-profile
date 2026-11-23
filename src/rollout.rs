//! Metadata-only ownership evidence for the pinned Codex rollout layout.
use crate::{
    paths,
    saved_files::{dir_at, entries, open_home},
    store::{Config, Store, configs},
    types::{Result, now},
};
use rusqlite::{Connection, OptionalExtension, params};
use rustix::fs::{AtFlags, FileType, fstat, statat};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    os::fd::OwnedFd,
    time::{Duration, Instant},
};
use time::{Date, Month, Time};
use uuid::Uuid;
mod worker;
pub use worker::run;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Unique(i64),
    Missing,
    Pending,
    Limit,
    Conflict,
    Unavailable(i64),
}

const MAX_PROFILES: usize = 128;
const MAX_TREE_ENTRIES: usize = 16384;
const MAX_LOOKUP_ENTRIES: usize = 512;
const MAX_GENERATIONS: usize = 256;
type Key = (i64, String);
type Check<T> = std::result::Result<T, Owner>;

fn digits(s: &[u8], width: usize) -> Option<u16> {
    (s.len() == width && s.iter().all(u8::is_ascii_digit))
        .then(|| s.iter().fold(0u16, |n, b| 10 * n + u16::from(b - b'0')))
}
fn date(year: u16, month: u16, day: u16) -> Option<Date> {
    Date::from_calendar_date(year.into(), Month::try_from(month as u8).ok()?, day as u8).ok()
}
fn thread(name: &[u8]) -> Option<Uuid> {
    let stem = name
        .strip_suffix(b".zst")
        .unwrap_or(name)
        .strip_suffix(b".jsonl")?
        .strip_prefix(b"rollout-")?;
    let (ts, ids) = stem.split_at_checked(20)?;
    if [ts[4], ts[7], ts[10], ts[13], ts[16], ts[19]] != *b"--T---" {
        return None;
    }
    date(
        digits(&ts[..4], 4)?,
        digits(&ts[5..7], 2)?,
        digits(&ts[8..10], 2)?,
    )?;
    Time::from_hms(
        digits(&ts[11..13], 2)? as u8,
        digits(&ts[14..16], 2)? as u8,
        digits(&ts[17..19], 2)? as u8,
    )
    .ok()?;
    let uuid = |raw: &[u8]| {
        (raw.len() == 36)
            .then(|| Uuid::try_parse_ascii(raw).ok())
            .flatten()
    };
    let (first, suffix) = ids.split_at_checked(36)?;
    if !suffix.is_empty() {
        uuid(suffix.strip_prefix(b"_")?)?;
    }
    uuid(first)
}

fn stamp(dir: &OwnedFd) -> io::Result<String> {
    let m = fstat(dir)?;
    Ok(format!(
        "{}:{}:{}:{}:{}:{}",
        m.st_dev, m.st_ino, m.st_mtime, m.st_mtime_nsec, m.st_ctime, m.st_ctime_nsec
    ))
}
fn file(dir: &OwnedFd, name: &str) -> io::Result<bool> {
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(m)
            if FileType::from_raw_mode(m.st_mode) == FileType::RegularFile
                && m.st_uid == paths::uid()
                && m.st_nlink == 1 =>
        {
            Ok(m.st_size > 0)
        }
        Err(rustix::io::Errno::NOENT) => Ok(false),
        _ => Err(io::ErrorKind::InvalidData.into()),
    }
}

fn profiles(db: &Connection) -> Result<Option<Vec<Config>>> {
    if db
        .prepare_cached("SELECT 1 FROM configs WHERE tool='codex' LIMIT 1 OFFSET ?")?
        .exists([MAX_PROFILES])?
    {
        return Ok(None);
    }
    Ok(Some(configs(db, "codex")?))
}

struct Budget {
    left: usize,
    exhausted: Owner,
    end: Instant,
}
impl Budget {
    fn take(&mut self) -> Check<()> {
        if self.left == 0 {
            return Err(self.exhausted);
        }
        if Instant::now() >= self.end {
            return Err(Owner::Pending);
        }
        self.left -= 1;
        Ok(())
    }
}

#[derive(Default)]
struct Tree {
    roots: BTreeMap<i64, OwnedFd>,
    leaves: BTreeMap<Key, String>,
    absent: BTreeSet<i64>,
}
impl Tree {
    // Enumerate calendar directories only. Never enumerate the profile home itself.
    fn collect(cs: &[Config], end: Instant, mut watch: impl FnMut(&Key, &OwnedFd)) -> Check<Self> {
        let mut tree = Self::default();
        let mut budget = Budget {
            left: MAX_TREE_ENTRIES,
            exhausted: Owner::Limit,
            end,
        };
        for c in cs {
            budget.take()?;
            let home = match open_home(&c.home) {
                Ok(fd) if c.matches_open_home(&fd) => fd,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    tree.absent.insert(c.id);
                    continue;
                }
                _ => return Err(Owner::Unavailable(c.id)),
            };
            for rel in ["sessions", "archived_sessions"] {
                match dir_at(&home, rel) {
                    Ok(fd) => {
                        tree.walk((c.id, rel.to_owned()), fd, &[], &mut budget, &mut watch)?
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(_) => return Err(Owner::Unavailable(c.id)),
                }
            }
            tree.roots.insert(c.id, home);
        }
        Ok(tree)
    }
    fn walk(
        &mut self,
        key: Key,
        fd: OwnedFd,
        parts: &[u16],
        budget: &mut Budget,
        watch: &mut impl FnMut(&Key, &OwnedFd),
    ) -> Check<()> {
        budget.take()?;
        let unavailable = |_| Owner::Unavailable(key.0);
        watch(&key, &fd);
        let before = stamp(&fd).map_err(unavailable)?;
        if key.1 == "archived_sessions" || parts.len() == 3 {
            self.leaves.insert(key, before);
            return Ok(());
        }
        for e in entries(&fd).map_err(|_| Owner::Unavailable(key.0))? {
            budget.take()?;
            let e = e.map_err(|_| Owner::Unavailable(key.0))?;
            let raw = e.file_name().to_bytes();
            let Some(n) = digits(raw, if parts.is_empty() { 4 } else { 2 }) else {
                continue;
            };
            if matches!(parts,[_] if !(1..=12).contains(&n))
                || matches!(parts,[y,m] if date(*y,*m,n).is_none())
            {
                continue;
            }
            let child = dir_at(&fd, e.file_name()).map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    Owner::Pending
                } else {
                    Owner::Unavailable(key.0)
                }
            })?;
            let child_key = (
                key.0,
                format!("{}/{}", key.1, std::str::from_utf8(raw).unwrap()),
            );
            self.walk(child_key, child, &[parts, &[n]].concat(), budget, watch)?;
        }
        if stamp(&fd).map_err(unavailable)? != before {
            return Err(Owner::Pending);
        }
        Ok(())
    }
    fn open(&self, key: &Key) -> Check<OwnedFd> {
        // Only reopen paths from our live traversal, never unvalidated strings from SQLite.
        let expected = self.leaves.get(key).ok_or(Owner::Pending)?;
        let mut parts = key.1.split('/');
        let err = |e: io::Error| {
            if e.kind() == io::ErrorKind::NotFound {
                Owner::Pending
            } else {
                Owner::Unavailable(key.0)
            }
        };
        let mut fd = dir_at(&self.roots[&key.0], parts.next().unwrap()).map_err(err)?;
        for name in parts {
            fd = dir_at(&fd, name).map_err(err)?;
        }
        if &stamp(&fd).map_err(err)? != expected {
            return Err(Owner::Pending);
        }
        Ok(fd)
    }
    fn recheck(&self, cs: &[Config], end: Instant) -> Check<()> {
        // A positive stat is not unique if a formerly negative leaf gained a
        // candidate during verification. This is not a cross-home snapshot.
        let after = Self::collect(cs, end, |_, _| {})?;
        if self.leaves != after.leaves || self.absent != after.absent || Instant::now() >= end {
            return Err(Owner::Pending);
        }
        Ok(())
    }
    fn dirty(&self, store: &Store, end: Instant) -> Result<Option<Vec<Key>>> {
        let mut leaves = self.leaves.iter().peekable();
        let mut out = Vec::new();
        while leaves.peek().is_some() {
            let db = store.db()?;
            let mut q =
                db.prepare_cached("SELECT stamp FROM rollout_dirs WHERE owner=? AND dir=?")?;
            for (key, stamp) in leaves.by_ref().take(128) {
                if Instant::now() >= end {
                    return Ok(None);
                }
                let saved: Option<String> = q
                    .query_row(params![key.0, key.1], |r| r.get(0))
                    .optional()?;
                if saved.as_ref() != Some(stamp) {
                    out.push(key.clone());
                }
            }
        }
        Ok(Some(out))
    }
}

type History = Option<(i64, bool)>;
fn history(db: &Connection, id: Uuid) -> Result<History> {
    Ok(db.prepare_cached("SELECT owner,saved_file_seen_ms IS NOT NULL FROM sessions WHERE tool='codex' AND native_id=?")?
        .query_row([id.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?)
}
fn candidates(db: &Connection, id: Uuid) -> Result<Vec<(Key, String)>> {
    let mut q =
        db.prepare_cached("SELECT owner,dir,name FROM rollout_paths WHERE native_id=? LIMIT ?")?;
    let mut paths = q
        .query_map(params![id.to_string(), MAX_GENERATIONS + 1], |r| {
            Ok(((r.get(0)?, r.get(1)?), r.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    paths.sort_unstable();
    Ok(paths)
}
fn verify(
    tree: &Tree,
    id: Uuid,
    past: History,
    indexed: &[(Key, String)],
    dirty: &[Key],
    budget: &mut Budget,
    stat: &mut impl FnMut(&OwnedFd, &str) -> io::Result<bool>,
) -> Check<Owner> {
    if indexed.len() > MAX_GENERATIONS {
        return Err(Owner::Limit);
    }
    let mut found = None;
    let mut matches = 0;
    let mut empty = Vec::new();
    let mut consider = |key: &Key, fd: &OwnedFd, name: &str| -> Check<()> {
        let owner = key.0;
        matches += 1;
        if matches > MAX_GENERATIONS {
            return Err(Owner::Limit);
        }
        if stat(fd, name).map_err(|_| Owner::Unavailable(owner))? {
            if found.is_some_and(|n| n != owner) || past.is_some_and(|(n, _)| n != owner) {
                return Err(Owner::Conflict);
            }
            found = Some(owner);
        } else {
            empty.push((key.clone(), name.to_owned()));
        }
        Ok(())
    };
    for (key, name) in indexed {
        budget.take()?;
        if tree.leaves.contains_key(key) && dirty.binary_search(key).is_err() {
            if thread(name.as_bytes()) != Some(id) {
                return Err(Owner::Pending);
            }
            consider(key, &tree.open(key)?, name)?;
        }
    }
    for key in dirty {
        let fd = tree.open(key)?;
        for e in entries(&fd).map_err(|_| Owner::Unavailable(key.0))? {
            budget.take()?;
            let e = e.map_err(|_| Owner::Unavailable(key.0))?;
            if thread(e.file_name().to_bytes()) == Some(id) {
                consider(key, &fd, e.file_name().to_str().unwrap())?;
            }
        }
        if stamp(&fd).ok().as_ref() != tree.leaves.get(key) {
            return Err(Owner::Pending);
        }
    }
    if let Some(owner) = found {
        // Filling an existing empty file doesn't change the directory's stamp.
        for (key, name) in empty {
            if key.0 != owner {
                if Instant::now() >= budget.end {
                    return Err(Owner::Pending);
                }
                if stat(&tree.open(&key)?, &name).map_err(|_| Owner::Unavailable(key.0))? {
                    return Err(Owner::Pending);
                }
            }
        }
        return Ok(Owner::Unique(owner));
    }
    if let Some((owner, _)) = past
        && tree.absent.contains(&owner)
    {
        return Err(Owner::Unavailable(owner));
    }
    Ok(Owner::Missing)
}
fn mark(db: &Connection, id: Uuid, owner: i64) -> Result<()> {
    let bound = db
        .prepare_cached(
            "INSERT INTO sessions(tool,native_id,owner,evidence,bound_ms,saved_file_seen_ms)
        VALUES('codex',?1,?2,'saved_file',?3,?3)
        ON CONFLICT(tool,native_id) DO UPDATE SET saved_file_seen_ms=excluded.saved_file_seen_ms
        WHERE sessions.owner=excluded.owner AND sessions.saved_file_seen_ms IS NULL",
        )?
        .execute(params![id.to_string(), owner, now()])?;
    if bound != 0 {
        db.prepare_cached(
            "UPDATE rollout_paths SET needs_check=0 WHERE native_id=? AND needs_check=1",
        )?
        .execute([id.to_string()])?;
    }
    Ok(())
}

pub struct Proof {
    id: Uuid,
    end: Instant,
    registry: Option<Vec<Config>>,
    past: History,
    paths: Vec<(Key, String)>,
    status: Owner,
}
impl Proof {
    /// Revalidate the probe's SQL inputs and record evidence inside the admission transaction.
    pub fn resolve(&self, db: &Connection) -> Result<Owner> {
        if !matches!(self.status, Owner::Unique(_) | Owner::Missing) {
            return Ok(self.status);
        }
        let current = history(db, self.id)?;
        let historical_owner = self.past.map(|(owner, _)| owner);
        if let Some((owner, _)) = current
            && (historical_owner.is_some_and(|old| old != owner)
                || matches!(self.status, Owner::Unique(file_owner) if owner != file_owner))
        {
            return Ok(Owner::Conflict);
        }
        if profiles(db)? != self.registry
            || candidates(db, self.id)? != self.paths
            || Instant::now() >= self.end
            || (self.past.is_some() && current.is_none())
            || (self.status == Owner::Missing && self.past != current)
        {
            return Ok(Owner::Pending);
        }
        if let Owner::Unique(owner) = self.status
            && !current.is_some_and(|(_, seen)| seen)
        {
            mark(db, self.id, owner)?;
        }
        Ok(self.status)
    }
}

/// Probe once per exact resume, immediately before taking the admission transaction.
pub fn probe(store: &Store, id: Uuid) -> Result<Proof> {
    probe_with(store, id, file)
}
fn probe_with(
    store: &Store,
    id: Uuid,
    mut stat: impl FnMut(&OwnedFd, &str) -> io::Result<bool>,
) -> Result<Proof> {
    let mut proof = {
        let db = store.db()?;
        Proof {
            id,
            end: Instant::now() + Duration::from_millis(100),
            registry: profiles(&db)?,
            past: history(&db, id)?,
            paths: Vec::new(),
            status: Owner::Limit,
        }
    };
    let Some(cs) = &proof.registry else {
        return Ok(proof);
    };
    let mut budget = Budget {
        left: MAX_LOOKUP_ENTRIES,
        exhausted: Owner::Pending,
        end: proof.end,
    };
    proof.status = Owner::Pending;
    for _ in 0..2 {
        let tree = match Tree::collect(cs, budget.end, |_, _| {}) {
            Ok(t) => t,
            Err(Owner::Pending) => continue,
            Err(status) => {
                proof.status = status;
                break;
            }
        };
        // Bracket the coverage check with the path snapshots here and in resolve.
        proof.paths = candidates(&*store.db()?, id)?;
        let Some(dirty) = tree.dirty(store, budget.end)? else {
            break;
        };
        let status = verify(
            &tree,
            id,
            proof.past,
            &proof.paths,
            &dirty,
            &mut budget,
            &mut stat,
        )
        .and_then(|s| tree.recheck(cs, budget.end).map(|()| s))
        .unwrap_or_else(|s| s);
        if status == Owner::Pending && budget.left > 0 && Instant::now() < budget.end {
            continue;
        }
        proof.status = status;
        break;
    }
    Ok(proof)
}
