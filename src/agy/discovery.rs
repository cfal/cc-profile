use super::{conversation_dir, open_home, saved_in};
use crate::saved_files::entries as scan_dir;
use crate::{
    store::{Config, Store, configs},
    types::{Result, now},
};
use rustix::fs::{Dir, fstat, inotify};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    mem::MaybeUninit,
    os::fd::{AsRawFd, OwnedFd},
    time::{Duration, Instant},
};
use uuid::Uuid;

const TICK: Duration = Duration::from_millis(100);
const REFRESH: Duration = Duration::from_secs(1);
const RECONCILE: Duration = Duration::from_secs(60);
const SCAN_BATCH: usize = 128;

fn registered_dir(c: &Config) -> io::Result<Option<OwnedFd>> {
    let home = match open_home(&c.home) {
        Ok(home) => home,
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if !c.matches_open_home(&home) {
        return Ok(None);
    }
    match conversation_dir(&home) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn candidate(name: &[u8]) -> Option<Uuid> {
    let raw = name
        .strip_suffix(b".db")
        .or_else(|| name.strip_suffix(b".pb"))?;
    let id = Uuid::try_parse_ascii(raw).ok()?;
    (id.hyphenated()
        .encode_lower(&mut Uuid::encode_buffer())
        .as_bytes()
        == raw)
        .then_some(id)
}

struct Watched {
    key: (u64, u64),
    dir: OwnedFd,
    wd: Option<i32>,
}
fn watch_dir(fd: Option<&OwnedFd>, dir: &OwnedFd) -> Option<i32> {
    use inotify::WatchFlags as F;
    // Watch the already verified inode, not another resolution of the user's path.
    inotify::add_watch(
        fd?,
        format!("/proc/self/fd/{}", dir.as_raw_fd()),
        F::ONLYDIR
            | F::CREATE
            | F::MODIFY
            | F::CLOSE_WRITE
            | F::MOVED_TO
            | F::MOVED_FROM
            | F::DELETE
            | F::ATTRIB,
    )
    .ok()
}
struct Watcher {
    notify: Option<OwnedFd>,
    dirs: BTreeMap<i64, Watched>,
    scans: VecDeque<(i64, Dir)>,
    repair: bool,
}
impl Watcher {
    fn new() -> Self {
        Self {
            notify: inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
                .ok(),
            dirs: BTreeMap::new(),
            scans: VecDeque::new(),
            repair: false,
        }
    }
    fn forget(&mut self, id: i64) {
        if let Some(old) = self.dirs.remove(&id)
            && let (Some(fd), Some(wd)) = (&self.notify, old.wd)
        {
            let _ = inotify::remove_watch(fd, wd);
        }
        self.scans.retain(|(owner, _)| *owner != id);
    }
    fn refresh(&mut self, profiles: &[Config]) {
        let gone: Vec<_> = self
            .dirs
            .keys()
            .copied()
            .filter(|id| profiles.binary_search_by_key(id, |c| c.id).is_err())
            .collect();
        for id in gone {
            self.forget(id);
        }
        for c in profiles {
            let Some((dir, key)) = registered_dir(c).ok().flatten().and_then(|dir| {
                let m = fstat(&dir).ok()?;
                Some((dir, (m.st_dev, m.st_ino)))
            }) else {
                self.forget(c.id);
                continue;
            };
            if let Some(d) = self.dirs.get_mut(&c.id)
                && d.key == key
            {
                if d.wd.is_none() {
                    d.wd = watch_dir(self.notify.as_ref(), &dir);
                }
                continue;
            }
            self.forget(c.id);
            let wd = watch_dir(self.notify.as_ref(), &dir);
            if let Ok(scan) = scan_dir(&dir) {
                self.scans.push_back((c.id, scan));
            }
            self.dirs.insert(c.id, Watched { key, dir, wd });
        }
    }
    fn candidates(&mut self) -> BTreeSet<Uuid> {
        let mut ids = BTreeSet::new();
        if let Some(fd) = &self.notify {
            let mut buf = [MaybeUninit::uninit(); 8192];
            let mut reader = inotify::Reader::new(fd, &mut buf);
            // One kernel buffer per tick. An event is only a hint to stat a filename.
            while let Ok(event) = reader.next() {
                if event.events().contains(inotify::ReadFlags::QUEUE_OVERFLOW) {
                    self.repair = true;
                }
                if event.events().contains(inotify::ReadFlags::IGNORED) {
                    for d in self.dirs.values_mut().filter(|d| d.wd == Some(event.wd())) {
                        d.wd = None;
                    }
                    self.repair = true;
                }
                if let Some(id) = event.file_name().and_then(|n| candidate(n.to_bytes())) {
                    ids.insert(id);
                }
                if reader.is_buffer_empty() {
                    break;
                }
            }
        }
        if self.repair && self.scans.is_empty() {
            self.repair = false;
            self.scans.extend(
                self.dirs
                    .iter()
                    .filter_map(|(id, d)| Some((*id, scan_dir(&d.dir).ok()?))),
            );
        }
        for _ in 0..SCAN_BATCH {
            let Some((owner, mut scan)) = self.scans.pop_front() else {
                break;
            };
            if let Some(Ok(entry)) = scan.next() {
                if let Some(id) = candidate(entry.file_name().to_bytes()) {
                    ids.insert(id);
                }
                self.scans.push_back((owner, scan));
            }
        }
        ids
    }
}

fn record(store: &Store, ids: BTreeSet<Uuid>) -> Result<()> {
    use rusqlite::OptionalExtension;
    let (profiles, pending) = {
        let db = store.db()?;
        let mut existing = db.prepare_cached("SELECT owner,saved_file_seen_ms IS NOT NULL FROM sessions WHERE tool='agy' AND native_id=?")?;
        let mut pending = Vec::new();
        for id in ids {
            let row = existing
                .query_row([id.to_string()], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?))
                })
                .optional()?;
            if !row.is_some_and(|(_, known)| known) {
                pending.push((id, row.map(|(owner, _)| owner)));
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        (configs(&db, "agy")?, pending)
    };
    // Reopen the registered roots: a watch can outlive a rename or replacement.
    // Include disabled owners and check both suffixes before accepting any one owner.
    let mut dirs = Vec::new();
    for c in &profiles {
        match registered_dir(c) {
            Ok(Some(dir)) => dirs.push((c.id, dir)),
            Ok(None) => {}
            Err(_) => return Ok(()), // Cannot rule out another owner.
        }
    }
    let mut unique = Vec::new();
    'next: for (id, existing) in pending {
        let mut owner = None;
        for (c, dir) in &dirs {
            match saved_in(dir, id) {
                Ok(true) if owner.is_none() => owner = Some(c),
                Ok(false) => {}
                _ => continue 'next,
            }
        }
        if let Some(c) = owner
            && existing.is_none_or(|n| n == *c)
        {
            unique.push((id.to_string(), *c));
        }
    }
    if !unique.is_empty() {
        store.write(|db| {
            // Registration raced the filesystem check. Reconciliation will retry with all owners.
            if db.query_row("SELECT max(id) FROM configs WHERE tool='agy'", [], |r| r.get::<_,Option<i64>>(0))?
                != profiles.last().map(|c| c.id) { return Ok(()); }
            let mut insert = db.prepare_cached(
                "INSERT INTO sessions(tool,native_id,owner,evidence,bound_ms,saved_file_seen_ms)
                 VALUES('agy',?1,?2,'saved_file',?3,?3)
                 ON CONFLICT(tool,native_id) DO UPDATE SET saved_file_seen_ms=excluded.saved_file_seen_ms
                 WHERE sessions.owner=excluded.owner AND sessions.saved_file_seen_ms IS NULL",
            )?;
            let ms = now();
            for (id, owner) in unique { insert.execute(rusqlite::params![id,owner,ms])?; }
            Ok(())
        })?;
    }
    Ok(())
}

pub fn run(store: &Store) {
    let mut watcher = Watcher::new();
    let mut refresh = Instant::now();
    let mut reconcile = refresh + RECONCILE;
    loop {
        let t = Instant::now();
        if t >= refresh {
            if let Ok(profiles) = store.db().and_then(|db| configs(&db, "agy")) {
                watcher.refresh(&profiles);
            }
            refresh = t + REFRESH;
        }
        if t >= reconcile {
            watcher.repair = true;
            reconcile = t + RECONCILE;
        }
        let ids = watcher.candidates();
        if !ids.is_empty() && record(store, ids).is_err() {
            watcher.repair = true;
        }
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, symlink},
        path::PathBuf,
    };

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missed_events_reconcile_in_batches_and_known_files_need_no_write_lock() {
        let tmp = Scratch(std::env::temp_dir().join(format!("agy-saved-{}", Uuid::new_v4())));
        let home = tmp.0.join("profile");
        let state = tmp.0.join("state/db");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&home)
            .unwrap();
        fs::create_dir(state.parent().unwrap()).unwrap();
        let store = Store::open(&state).unwrap();
        let c = store
            .register(&json!({"tool":"agy","home":home,"label":"fixture"}))
            .unwrap();
        let dir = home
            .join(".gemini")
            .join(super::super::APP_DIR)
            .join("conversations");
        fs::create_dir_all(&dir).unwrap();
        let mut watcher = Watcher::new();
        watcher.notify = None; // No kernel events; use the same repair as overflow/periodic scans.
        watcher.refresh(std::slice::from_ref(&c));
        assert!(watcher.candidates().is_empty());
        let ids: BTreeSet<_> = (0..SCAN_BATCH + 1).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            fs::write(dir.join(format!("{id}.db")), b"x").unwrap();
        }
        assert!(watcher.candidates().is_empty());
        watcher.repair = true;
        let mut found = watcher.candidates();
        assert!(!found.is_empty() && found.len() <= SCAN_BATCH);
        found.extend(watcher.candidates());
        assert_eq!(found, ids);
        record(&store, found).unwrap();
        let db = rusqlite::Connection::open(&state).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM sessions", [], |r| r
                .get::<_, usize>(0))
                .unwrap(),
            ids.len()
        );
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        store.db().unwrap().busy_timeout(Duration::ZERO).unwrap();
        record(&store, ids).unwrap();
        db.execute_batch("ROLLBACK").unwrap();

        // A delayed event cannot bind a file through a replaced or symlinked profile.
        let moved = tmp.0.join("moved");
        fs::rename(&home, &moved).unwrap();
        let fresh = Uuid::new_v4();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{fresh}.pb")), b"x").unwrap();
        record(&store, [fresh].into()).unwrap();
        fs::rename(&home, tmp.0.join("replacement")).unwrap();
        symlink(&moved, &home).unwrap();
        fs::write(dir.join(format!("{fresh}.pb")), b"x").unwrap();
        record(&store, [fresh].into()).unwrap();
        watcher.refresh(&[c]);
        assert!(watcher.dirs.is_empty());
        assert!(
            !db.prepare("SELECT 1 FROM sessions WHERE native_id=?")
                .unwrap()
                .exists([fresh.to_string()])
                .unwrap()
        );
    }
}
