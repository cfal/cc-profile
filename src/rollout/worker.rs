use super::*;
use crate::native::Build;
use rustix::fs::{Dir, inotify};
use std::{collections::VecDeque, mem::MaybeUninit, os::fd::AsRawFd};

const BATCH: usize = 256;
const WATCHES: usize = 512;
const REFRESH: Duration = Duration::from_secs(3);
const RECONCILE: Duration = Duration::from_secs(30);
const PRUNE: Duration = Duration::from_secs(24 * 60 * 60);
const TICK: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Notices {
    directory: bool,
    overflow: bool,
    files: BTreeSet<Uuid>,
}
struct Watches {
    fd: Option<OwnedFd>,
    watches: BTreeMap<Key, (u64, u64, i32)>,
    visited: BTreeSet<Key>,
}
impl Watches {
    fn new() -> Self {
        Self {
            fd: inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC).ok(),
            watches: BTreeMap::new(),
            visited: BTreeSet::new(),
        }
    }
    fn add(&mut self, key: &Key, dir: &OwnedFd) {
        use inotify::WatchFlags as F;
        let Some(fd) = &self.fd else { return };
        let Ok(st) = fstat(dir) else { return };
        self.visited.insert(key.clone());
        if let Some(&(dev, ino, wd)) = self.watches.get(key) {
            if (dev, ino) == (st.st_dev, st.st_ino) {
                return;
            }
            let _ = inotify::remove_watch(fd, wd);
            self.watches.remove(key);
        }
        if self.watches.len() < WATCHES
            && let Ok(wd) = inotify::add_watch(
                fd,
                format!("/proc/self/fd/{}", dir.as_raw_fd()),
                F::ONLYDIR
                    | F::CREATE
                    | F::MOVED_TO
                    | F::MOVED_FROM
                    | F::DELETE
                    | F::MODIFY
                    | F::ATTRIB
                    | F::CLOSE_WRITE
                    | F::MOVE_SELF
                    | F::DELETE_SELF,
            )
        {
            self.watches.insert(key.clone(), (st.st_dev, st.st_ino, wd));
        }
    }
    fn finish(&mut self) {
        self.watches.retain(|key, (_, _, wd)| {
            if self.visited.contains(key) {
                true
            } else {
                if let Some(fd) = &self.fd {
                    let _ = inotify::remove_watch(fd, *wd);
                }
                false
            }
        });
        self.visited.clear();
    }
    fn events(&mut self) -> Notices {
        use inotify::ReadFlags as F;
        let mut notices = Notices::default();
        if let Some(fd) = &self.fd {
            let mut buf = [MaybeUninit::uninit(); 8192];
            let mut r = inotify::Reader::new(fd, &mut buf);
            while let Ok(e) = r.next() {
                if e.events().contains(F::IGNORED) {
                    self.watches.retain(|_, (_, _, wd)| *wd != e.wd());
                }
                notices.overflow |= e.events().contains(F::QUEUE_OVERFLOW);
                notices.directory |= e.events().intersects(
                    F::CREATE
                        | F::MOVED_TO
                        | F::MOVED_FROM
                        | F::DELETE
                        | F::MOVE_SELF
                        | F::DELETE_SELF
                        | F::QUEUE_OVERFLOW
                        | F::IGNORED,
                );
                if let Some(id) = e.file_name().and_then(|n| thread(n.to_bytes())) {
                    notices.files.insert(id);
                }
                if r.is_buffer_empty() {
                    break;
                }
            }
        }
        notices
    }
}

struct Scan {
    key: Key,
    stamp: String,
    iter: Dir,
}
struct Worker {
    watches: Watches,
    tree: Tree,
    todo: VecDeque<Key>,
    scan: Option<Scan>,
    refresh: Instant,
    reset: Instant,
    cursor: i64,
    tail: bool,
    prune: bool,
    next_prune: Instant,
}
impl Worker {
    fn new() -> Self {
        Self {
            watches: Watches::new(),
            tree: Tree::default(),
            todo: VecDeque::new(),
            scan: None,
            refresh: Instant::now(),
            reset: Instant::now() + RECONCILE,
            cursor: 0,
            tail: false,
            prune: false,
            next_prune: Instant::now() + PRUNE,
        }
    }
    fn tick(&mut self, store: &Store) -> Result<()> {
        let n = self.watches.events();
        self.update(store, n)
    }
    fn update(&mut self, store: &Store, n: Notices) -> Result<()> {
        let t = Instant::now();
        if n.overflow {
            self.watches = Watches::new();
            self.refresh = t;
            self.cursor = 0;
            self.tail = false;
            self.prune = false;
            self.scan = None;
            store.write(|db| {
                db.execute("DELETE FROM rollout_dirs", [])?;
                Ok(())
            })?;
        }
        if t >= self.refresh || n.directory {
            self.refresh_tree(store, t)?;
        }
        if self.tree.roots.is_empty() {
            return Ok(());
        }
        if self.scan.is_some() || !self.todo.is_empty() {
            self.index(store, t)?;
        }
        if self.scan.is_none() && self.todo.is_empty() {
            let mut ids = n.files;
            if self.tail && t >= self.reset {
                self.cursor = 0;
                self.tail = false;
                self.reset = t + RECONCILE;
                self.prune = t >= self.next_prune;
                if self.prune {
                    self.next_prune = t + PRUNE;
                }
            }
            if !self.tail {
                self.sweep(store, &mut ids)?;
            }
            record(store, ids)?;
        } else if !n.files.is_empty() {
            // An already-indexed empty file may have been filled while another
            // directory was incomplete. Revisit it once coverage is restored.
            self.reset = t;
        }
        Ok(())
    }
    fn refresh_tree(&mut self, store: &Store, t: Instant) -> Result<()> {
        let cs = {
            let db = store.db()?;
            profiles(&db)?
        };
        let tree = cs
            .as_deref()
            .and_then(|cs| Tree::collect(cs, t + REFRESH, |k, fd| self.watches.add(k, fd)).ok());
        self.watches.finish();
        self.refresh = t + REFRESH;
        let Some(tree) = tree else {
            self.tree = Tree::default();
            self.scan = None;
            self.todo.clear();
            return Ok(());
        };
        if self
            .scan
            .as_ref()
            .is_some_and(|s| tree.leaves.get(&s.key) != Some(&s.stamp))
        {
            self.scan = None;
        }
        let dirty = tree.dirty(store, t + REFRESH)?;
        let Some(dirty) = dirty else {
            self.tree = Tree::default();
            self.scan = None;
            self.todo.clear();
            return Ok(());
        };
        self.todo = dirty
            .into_iter()
            .filter(|k| !self.scan.as_ref().is_some_and(|s| &s.key == k))
            .collect();
        self.tree = tree;
        Ok(())
    }
    fn index(&mut self, store: &Store, t: Instant) -> Result<()> {
        let mut left = BATCH;
        while left > 0 {
            if self.scan.is_none() {
                let Some(key) = self.todo.pop_front() else {
                    break;
                };
                left -= 1;
                let Some(iter) = self.tree.open(&key).ok().and_then(|dir| entries(&dir).ok())
                else {
                    continue;
                };
                self.scan = Some(Scan {
                    stamp: self.tree.leaves[&key].clone(),
                    key,
                    iter,
                });
                self.tail = false;
            }
            let scan = self.scan.as_mut().unwrap();
            let mut batch = Vec::new();
            let mut done = false;
            while left > 0 {
                left -= 1;
                match scan.iter.next() {
                    None => {
                        done = true;
                        break;
                    }
                    Some(Err(_)) => {
                        self.scan = None;
                        return Ok(());
                    }
                    Some(Ok(e)) => {
                        if let Some(id) = thread(e.file_name().to_bytes()) {
                            batch.push((id, e.file_name().to_str().unwrap().to_owned()));
                        }
                    }
                }
            }
            index_batch(store, &scan.key, batch)?;
            if done {
                if self.tree.open(&scan.key).is_ok() {
                    store.write(|db| {
                        db.execute(
                            "INSERT INTO rollout_dirs(owner,dir,stamp) VALUES(?,?,?)
                            ON CONFLICT(owner,dir) DO UPDATE SET stamp=excluded.stamp",
                            params![scan.key.0, scan.key.1, scan.stamp],
                        )?;
                        Ok(())
                    })?;
                } else {
                    self.refresh = t;
                }
                self.scan = None;
            }
        }
        Ok(())
    }
    fn sweep(&mut self, store: &Store, ids: &mut BTreeSet<Uuid>) -> Result<()> {
        let paths = {
            let db = store.db()?;
            // Pending names include zero-byte files that can change without a directory
            // event. Known files are rechecked on exact lookup, not every 30 seconds.
            let sql = if self.prune {
                "SELECT rowid,owner,dir,name,native_id FROM rollout_paths WHERE rowid>? ORDER BY rowid LIMIT ?"
            } else {
                "SELECT rowid,owner,dir,name,native_id FROM rollout_paths WHERE needs_check=1 AND rowid>? ORDER BY rowid LIMIT ?"
            };
            db.prepare_cached(sql)?
                .query_map(params![self.cursor, BATCH], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        (r.get::<_, i64>(1)?, r.get::<_, String>(2)?),
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut stale = Vec::new();
        for (row, key, name, raw) in &paths {
            if let Some(id) = thread(name.as_bytes())
                && id.to_string() == *raw
            {
                ids.insert(id);
                // Removing an entire directory's stale hints also invalidates its
                // coverage stamp, so restoring that directory forces a new scan.
                if (!self.tree.leaves.contains_key(key) && self.tree.roots.contains_key(&key.0))
                    || self.tree.open(key).is_ok_and(|fd| {
                        matches!(
                            statat(&fd, name.as_str(), AtFlags::SYMLINK_NOFOLLOW),
                            Err(rustix::io::Errno::NOENT)
                        )
                    })
                {
                    stale.push((*row, key));
                }
            }
        }
        if !stale.is_empty() {
            store.write(|db| {
                let mut q = db.prepare_cached("DELETE FROM rollout_paths WHERE rowid=?")?;
                for (n, key) in stale {
                    q.execute([n])?;
                    if !self.tree.leaves.contains_key(key) {
                        db.execute(
                            "DELETE FROM rollout_dirs WHERE owner=? AND dir=?",
                            params![key.0, key.1],
                        )?;
                    }
                }
                Ok(())
            })?;
        }
        if let Some(last) = paths.last() {
            self.cursor = last.0;
        }
        self.tail = paths.len() < BATCH;
        Ok(())
    }
}

fn index_batch(store: &Store, key: &Key, batch: Vec<(Uuid, String)>) -> Result<()> {
    let missing = {
        let db = store.db()?;
        let mut exists =
            db.prepare_cached("SELECT 1 FROM rollout_paths WHERE owner=? AND dir=? AND name=?")?;
        let mut missing = Vec::new();
        for (id, name) in batch {
            if !exists.exists(params![key.0, key.1, name])? {
                missing.push((id, name));
            }
        }
        missing
    };
    if !missing.is_empty() {
        store.write(|db| {
            let mut q = db.prepare_cached(
                "INSERT OR IGNORE INTO rollout_paths(owner,dir,name,native_id,needs_check)
                 VALUES(?1,?2,?3,?4,NOT EXISTS(SELECT 1 FROM sessions WHERE tool='codex'
                        AND native_id=?4 AND saved_file_seen_ms IS NOT NULL))",
            )?;
            for (id, name) in missing {
                q.execute(params![key.0, key.1, name, id.to_string()])?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn record(store: &Store, ids: BTreeSet<Uuid>) -> Result<()> {
    record_with(store, ids, file)
}
fn record_with(
    store: &Store,
    ids: BTreeSet<Uuid>,
    mut stat: impl FnMut(&OwnedFd, &str) -> io::Result<bool>,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let (cs, pending) = {
        let db = store.db()?;
        let mut pending = Vec::new();
        for id in ids {
            let past = history(&db, id)?;
            if !past.is_some_and(|(_, known)| known) {
                pending.push((id, past));
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        let Some(cs) = profiles(&db)? else {
            return Ok(());
        };
        (cs, pending)
    };
    // File events can arrive while enumerating. Reopen the registered homes and
    // require complete coverage before treating any indexed name as unique.
    let end = Instant::now() + REFRESH;
    let Ok(tree) = Tree::collect(&cs, end, |_, _| {}) else {
        return Ok(());
    };
    if !tree.dirty(store, end)?.is_some_and(|v| v.is_empty()) {
        return Ok(());
    }
    let mut unique = Vec::new();
    for (id, past) in pending {
        if Instant::now() >= end {
            return Ok(());
        }
        let paths = candidates(&*store.db()?, id)?;
        let mut budget = Budget {
            left: MAX_GENERATIONS,
            exhausted: Owner::Limit,
            end: end.min(Instant::now() + TICK),
        };
        if let Ok(Owner::Unique(c)) = verify(&tree, id, past, &paths, &[], &mut budget, &mut stat) {
            unique.push((id, c));
        }
    }
    if !unique.is_empty() && tree.recheck(&cs, end).is_ok() {
        store.write(|db| {
            if profiles(db)?.as_ref() != Some(&cs) || Instant::now() >= end {
                return Ok(());
            }
            for (id, c) in unique {
                mark(db, id, c)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub fn run(store: &Store, build: &Build) {
    let mut worker = Worker::new();
    while build.supported && build.unchanged() {
        if worker.tick(store).is_err() {
            worker.scan = None;
            worker.refresh = Instant::now();
        }
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
