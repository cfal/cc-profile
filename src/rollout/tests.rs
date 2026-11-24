use super::*;
use crate::native::Build;
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

struct Fixture {
    root: PathBuf,
    db: Store,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ccp-rollout-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("state")).unwrap();
        let db = Store::open(&root.join("state/router.sqlite3")).unwrap();
        Self { root, db }
    }
    fn profile(&self, name: &str) -> Config {
        let p = self.root.join(name);
        fs::create_dir(&p).unwrap();
        self.db
            .register(&json!({"tool":"codex","label":name,"home":p}))
            .unwrap()
    }
    fn owner(&self, id: Uuid) -> Owner {
        self.resolve(&probe(&self.db, id).unwrap())
    }
    fn resolve(&self, proof: &Proof) -> Owner {
        self.db.write(|db| proof.resolve(db)).unwrap()
    }
    fn seen(&self, id: Uuid) -> History {
        history(&self.db.db().unwrap(), id).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn name(id: Uuid) -> String {
    format!("rollout-2024-02-29T12-59-03-{id}.jsonl")
}
fn write(dir: &Path, name: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, b"x").unwrap();
    p
}

#[test]
fn filtered_list_never_binds_over_indexed_or_newly_dirty_archived_competitor() {
    use crate::{
        listed,
        protocol::CodexListed,
        routing::{Request, launch},
    };
    let f = Fixture::new();
    let a = f.profile("active");
    let a =
        f.db.edit(a.id, &json!({"revision":a.revision,"enabled":true}))
            .unwrap();
    let b = f.profile("other");
    let active = a.home.join("sessions/2024/02/29");
    let archive = b.home.join("archived_sessions");
    let (x, y, db_only) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    write(&active, &name(x));
    write(&active, &name(y));
    let indexed = write(&archive, &name(x));
    fs::write(&indexed, b"").unwrap();
    let key = (b.id, "archived_sessions".to_owned());
    index_batch(&f.db, &key, vec![(x, name(x))]).unwrap();
    f.db.db()
        .unwrap()
        .execute(
            "INSERT INTO rollout_dirs(owner,dir,stamp) VALUES(?,?,?)",
            params![b.id, key.1, stamp(&open_home(&archive).unwrap()).unwrap()],
        )
        .unwrap();

    let bin = std::env::current_exe().unwrap();
    let build = Build {
        fingerprint: crate::native::stamp(&bin),
        bin: bin.clone(),
        version: "fixture".into(),
        supported: true,
    };
    let req = Request {
        tool: "codex".into(),
        argv: vec!["app-server".into()],
        request_id: Uuid::new_v4().to_string(),
        ticket: "ad".repeat(32),
        binary: Some(bin.clone()),
        cwd: f.root.clone(),
        login_profile: None,
        override_names: vec![],
    };
    assert_eq!(launch(&f.db, &build, &req).unwrap().config_id, a.id);
    let submit = |bound: bool, id: Uuid, roster: &[(i64, i64)]| {
        listed::codex(
            &f.db,
            &build,
            CodexListed {
                binary: bin.clone(),
                profiles: if bound { vec![] } else { roster.to_vec() },
                bound: bound.then(|| (req.request_id.clone(), req.ticket.clone())),
                threads: vec![
                    (a.id, db_only.to_string(), "/db".into()),
                    (a.id, id.to_string(), "/active".into()),
                ],
            },
        )
    };
    let b =
        f.db.edit(b.id, &json!({"revision":b.revision,"enabled":true}))
            .unwrap();
    let one = [(a.id, a.revision)];
    let both = [one[0], (b.id, b.revision)];
    fs::write(&indexed, b"now nonempty").unwrap(); // Does not dirty the indexed directory.
    for bound in [false, true] {
        assert_eq!(submit(bound, x, &both).unwrap_err().1, "rollout_conflict");
        assert_eq!((f.seen(x), f.seen(db_only)), (None, None));
    }

    fs::write(&indexed, b"").unwrap(); // Still indexed; a filename alone is not a conflict.
    write(&archive, &name(y)); // New generation absent from rollout_paths.
    f.db.edit(b.id, &json!({"revision":b.revision,"enabled":false}))
        .unwrap();
    for bound in [false, true] {
        assert_eq!(submit(bound, y, &one).unwrap_err().1, "rollout_conflict");
        assert_eq!((f.seen(y), f.seen(db_only)), (None, None));
    }
    assert_eq!(submit(true, x, &one).unwrap(), 2);
    assert_eq!(f.seen(x), Some((a.id, false)));
    fs::rename(&b.home, f.root.join("offline")).unwrap();
    assert_eq!(submit(false, y, &one).unwrap(), 2);
}

#[test]
fn listed_batch_scans_one_dirty_leaf_for_many_ids_and_rechecks_sql_before_publication() {
    use crate::rollout::listed;
    let f = Fixture::new();
    let a = f.profile("listed");
    let b = f.profile("competitor");
    let archive = b.home.join("archived_sessions");
    fs::create_dir(&archive).unwrap();
    // Too many entries for the per-thread exact-resume probe, but one bounded
    // directory scan can establish the entire batch without per-ID traversal.
    for n in 0..=MAX_LOOKUP_ENTRIES {
        fs::write(archive.join(n.to_string()), b"").unwrap();
    }
    let ids: BTreeMap<_, _> = (0..256).map(|_| (Uuid::new_v4(), a.id)).collect();
    let end = Instant::now() + Duration::from_secs(5);
    let before = listed::probe(&f.db, &ids, end).unwrap();
    f.db.write(|db| before.recheck(db, end)).unwrap();
    let id = *ids.keys().next().unwrap();
    index_batch(
        &f.db,
        &(b.id, "archived_sessions".into()),
        vec![(id, name(id))],
    )
    .unwrap();
    assert_eq!(
        f.db.write(|db| before.recheck(db, end)).unwrap_err().1,
        "rollout_pending"
    );
    let before = listed::probe(&f.db, &ids, end).unwrap();
    f.profile("new-disabled-competitor");
    assert_eq!(
        f.db.write(|db| before.recheck(db, end)).unwrap_err().1,
        "rollout_pending"
    );
}

#[test]
fn pinned_filenames_key_by_thread_not_generation() {
    let id = Uuid::new_v4();
    let base = name(id);
    let stem = base.strip_suffix(".jsonl").unwrap();
    let second = Uuid::new_v4();
    for n in [
        &base,
        &format!("{base}.zst"),
        &format!("{stem}_{second}.jsonl"),
        &format!("{stem}_{second}.jsonl.zst"),
    ] {
        assert_eq!(thread(n.as_bytes()), Some(id), "{n}");
    }
    for n in [
        format!("{base}.bak"),
        format!("{stem}_oops.jsonl.zst"),
        format!("{base}.zst.zst"),
        base.replace("2024-02-29", "2025-02-29"),
        base.replace("12-59-03", "25-00-00"),
        format!("{stem}_{second}_{second}.jsonl"),
        base.replace("-02-29T", "/02/29T"),
        base.replace(&id.to_string(), &id.simple().to_string()),
    ] {
        assert_eq!(thread(n.as_bytes()), None, "{n}");
    }
}

#[test]
fn verify_exact_or_refuse_without_reassigning_history() {
    let f = Fixture::new();
    let a = f.profile("a");
    let b = f.profile("b");
    let a =
        f.db.edit(a.id, &json!({"revision":a.revision,"enabled":true}))
            .unwrap();
    assert!(!b.enabled);
    let id = Uuid::new_v4();
    let n = name(id);
    let active = a.home.join("sessions/2024/02/29");
    let archive = b.home.join("archived_sessions");
    write(&a.home.join("sessions/2024/02/30"), &n);
    write(&a.home.join("sessions/2024/2/29"), &n);
    write(&a.home.join("other/2024/02/29"), &n);
    assert_eq!(f.owner(id), Owner::Missing);
    let file = write(&active, &n);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let generation = format!(
        "{}_{}.jsonl.zst",
        n.strip_suffix(".jsonl").unwrap(),
        Uuid::new_v4()
    );
    let another = write(&a.home.join("archived_sessions"), &generation);
    assert_eq!(f.owner(id), Owner::Unique(a.id));
    assert_eq!(f.seen(id), Some((a.id, true)));
    assert!(f.db.db().unwrap().prepare("SELECT 1 FROM sessions WHERE tool='codex' AND evidence='saved_file' AND workspace IS NULL AND last_routed_ms IS NULL").unwrap().exists([]).unwrap());
    let dup = write(&archive, &generation);
    assert_eq!(f.owner(id), Owner::Conflict);
    fs::remove_file(&dup).unwrap();
    fs::remove_file(&file).unwrap();
    fs::remove_file(another).unwrap();
    assert_eq!(f.owner(id), Owner::Missing);
    write(&archive, &generation);
    assert_eq!(f.owner(id), Owner::Conflict);
    fs::remove_file(&dup).unwrap();
    fs::rename(&b.home, f.root.join("offline")).unwrap(); // Unrelated, physically absent.
    write(&active, &n);
    assert_eq!(f.owner(id), Owner::Unique(a.id));
    fs::rename(&a.home, f.root.join("old-a")).unwrap();
    assert_eq!(f.owner(id), Owner::Unavailable(a.id));
    fs::create_dir(&a.home).unwrap();
    write(&active, &n);
    assert_eq!(f.owner(id), Owner::Unavailable(a.id));
    assert_eq!(f.seen(id), Some((a.id, true)));
}

#[test]
fn unsafe_competitor_is_never_negative_evidence() {
    let f = Fixture::new();
    let a = f.profile("a");
    let b = f.profile("b");
    let id = Uuid::new_v4();
    let n = name(id);
    let actual = write(&a.home.join("archived_sessions"), &n);
    let hidden = b.home.join("elsewhere");
    fs::create_dir(&hidden).unwrap();
    let archive = b.home.join("archived_sessions");
    symlink(&hidden, &archive).unwrap();
    assert_eq!(f.owner(id), Owner::Unavailable(b.id));
    fs::remove_file(&archive).unwrap();
    fs::create_dir(&archive).unwrap();
    symlink(&actual, archive.join(&n)).unwrap();
    assert_eq!(f.owner(id), Owner::Unavailable(b.id));
    fs::remove_file(archive.join(&n)).unwrap();
    let other = write(&hidden, "unrelated");
    fs::hard_link(other, archive.join(&n)).unwrap();
    assert_eq!(f.owner(id), Owner::Unavailable(b.id));
    fs::remove_file(archive.join(&n)).unwrap();
    if paths::uid() != 0 {
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(f.owner(id), Owner::Unavailable(b.id));
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(f.seen(id), None);
    assert_eq!(f.owner(id), Owner::Unique(a.id));
}

#[test]
fn blocked_rollout_stat_does_not_block_new_admission() {
    use crate::routing::{Request, launch};
    use std::sync::mpsc::channel;

    let f = Fixture::new();
    let a = f.profile("a");
    f.db.edit(a.id, &json!({"enabled":true,"revision":a.revision}))
        .unwrap();
    let id = Uuid::new_v4();
    write(&a.home.join("archived_sessions"), &name(id));
    let bin = std::env::current_exe().unwrap();
    let build = Build {
        fingerprint: crate::native::stamp(&bin),
        bin: bin.clone(),
        version: "fixture".into(),
        supported: true,
    };
    let req = Request {
        tool: "codex".into(),
        argv: Vec::new(),
        cwd: f.root.clone(),
        binary: Some(bin),
        login_profile: None,
        request_id: Uuid::new_v4().to_string(),
        ticket: "c1".repeat(32),
        override_names: Vec::new(),
    };
    let store = &f.db;
    std::thread::scope(|s| {
        let (entered, waiting) = channel();
        let (release, paused) = channel();
        s.spawn(move || {
            let mut pause = Some(paused);
            probe_with(store, id, |fd, n| {
                if let Some(paused) = pause.take() {
                    entered.send(()).unwrap();
                    paused.recv().unwrap();
                }
                file(fd, n)
            })
            .unwrap()
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let (admitted, done) = channel();
        s.spawn(move || admitted.send(launch(store, &build, &req)).unwrap());
        let plan = done.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        assert_eq!(
            plan.expect("admission blocked by rollout stat")
                .unwrap()
                .config_id,
            a.id
        );
    });
    assert_eq!(f.seen(id), None);
}

#[test]
fn resolve_rechecks_registry_candidates_and_historical_binding() {
    let f = Fixture::new();
    let a = f.profile("a");
    let id = Uuid::new_v4();
    let n = name(id);
    write(&a.home.join("archived_sessions"), &n);
    let mut expired = probe(&f.db, id).unwrap();
    expired.end = Instant::now();
    assert_eq!(f.resolve(&expired), Owner::Pending);
    assert_eq!(f.seen(id), None);
    let proof = probe(&f.db, id).unwrap();
    let b = f.profile("late");
    assert!(!b.enabled);
    let dupe = write(&b.home.join("archived_sessions"), &n);
    assert_eq!(f.resolve(&proof), Owner::Pending);
    assert_eq!(f.seen(id), None);
    assert_eq!(f.owner(id), Owner::Conflict);
    fs::remove_file(dupe).unwrap();

    let observed = |id: Uuid, owner| {
        f.db.db().unwrap().execute(
            "INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms)
             VALUES('codex',?,?,'observed','/verified',1,2)",params![id.to_string(),owner]
        ).unwrap();
    };
    let proof = probe(&f.db, id).unwrap();
    index_batch(&f.db, &(b.id, "archived_sessions".into()), vec![(id, n)]).unwrap();
    assert_eq!(f.resolve(&proof), Owner::Pending);
    let proof = probe(&f.db, id).unwrap();
    observed(id, b.id);
    assert_eq!(f.resolve(&proof), Owner::Conflict);
    assert_eq!(f.seen(id), Some((b.id, false)));

    let missing = Uuid::new_v4();
    let proof = probe(&f.db, missing).unwrap();
    observed(missing, a.id);
    assert_eq!(f.resolve(&proof), Owner::Pending);
    assert_eq!(f.owner(missing), Owner::Missing);

    let agrees = Uuid::new_v4();
    write(&a.home.join("archived_sessions"), &name(agrees));
    let proof = probe(&f.db, agrees).unwrap();
    observed(agrees, a.id);
    assert_eq!(f.resolve(&proof), Owner::Unique(a.id));
    assert_eq!(f.resolve(&proof), Owner::Unique(a.id));
    assert_eq!(f.seen(agrees), Some((a.id, true)));
    assert!(f.db.db().unwrap().prepare("SELECT 1 FROM sessions WHERE native_id=? AND evidence='observed' AND workspace='/verified' AND bound_ms=1 AND last_routed_ms=2").unwrap().exists([agrees.to_string()]).unwrap());
}

#[test]
fn competitor_saved_before_first_positive_never_binds_the_wrong_owner() {
    for worker in [false, true] {
        for layout in ["covered", "late-day", "empty"] {
            let f = Fixture::new();
            // B sorts first, so its zero-byte candidate (when present) is read first.
            let b = f.profile("b");
            let a = f.profile("a");
            let id = Uuid::new_v4();
            let n = name(id);
            let b_dir = match layout {
                "late-day" => b.home.join("sessions/2024/02/29"),
                _ => b.home.join("archived_sessions"),
            };
            if layout != "late-day" {
                fs::create_dir(&b_dir).unwrap();
            }
            if layout == "empty" {
                fs::File::create(b_dir.join(&n)).unwrap();
            }
            let a_dir = a.home.join("archived_sessions");
            fs::create_dir(&a_dir).unwrap();
            fs::File::create(a_dir.join(&n)).unwrap();
            Worker::new().tick(&f.db).unwrap();
            assert_eq!(f.seen(id), None);

            let mut reads = 0;
            let mut stat = |fd: &OwnedFd, name: &str| {
                reads += 1;
                if reads == (if layout == "empty" { 2 } else { 1 }) {
                    write(&b_dir, &n);
                    write(&a_dir, &n);
                }
                file(fd, name)
            };
            if worker {
                record_with(&f.db, [id].into(), &mut stat).unwrap();
            } else {
                let proof = probe_with(&f.db, id, &mut stat).unwrap();
                assert!(matches!(
                    f.resolve(&proof),
                    Owner::Pending | Owner::Conflict
                ));
            }
            assert!(reads > 0);
            assert_eq!(f.seen(id), None, "worker={worker} layout={layout}");
            assert_eq!(f.owner(id), Owner::Conflict);
        }
    }
}

#[test]
fn partial_index_never_proves_absence_and_large_index_stays_searchable() {
    let f = Fixture::new();
    let a = f.profile("a");
    let b = f.profile("b");
    let id = Uuid::new_v4();
    let n = name(id);
    let day = a.home.join("sessions/2024/02/29");
    let archive = b.home.join("archived_sessions");
    fs::create_dir(&archive).unwrap();
    write(&day, &n);
    // Budget includes non-matching names; an incomplete lookup can't assume uniqueness.
    for i in 0..2400 {
        fs::write(archive.join(format!("ignored-{i}")), b"").unwrap();
    }
    assert_eq!(f.owner(id), Owner::Pending);
    assert_eq!(f.seen(id), None);
    let mut w = Worker::new();
    for _ in 0..24 {
        w.tick(&f.db).unwrap();
    }
    assert_eq!(f.seen(id), Some((a.id, true)));
    assert_eq!(f.owner(id), Owner::Unique(a.id));
    write(&archive, &format!("{n}.zst"));
    assert!(matches!(f.owner(id), Owner::Pending | Owner::Conflict));
    for _ in 0..24 {
        w.tick(&f.db).unwrap();
    }
    assert_eq!(f.owner(id), Owner::Conflict);
    // A new worker must not trust the previous process's index over fresh metadata.
    drop(w);
    fs::remove_file(archive.join(format!("{n}.zst"))).unwrap();
    let mut w = Worker::new();
    for _ in 0..24 {
        w.tick(&f.db).unwrap();
    }
    assert_eq!(f.owner(id), Owner::Unique(a.id));
    assert_eq!(
        f.db.db()
            .unwrap()
            .query_row("SELECT count(*) FROM launches", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn over_limit_is_not_a_promise_that_the_index_will_catch_up() {
    let f = Fixture::new();
    let c = f.profile("owner");
    let id = Uuid::new_v4();
    let d = c.home.join("archived_sessions");
    let stem = name(id);
    for _ in 0..=MAX_GENERATIONS {
        write(
            &d,
            &format!(
                "{}_{}.jsonl",
                stem.strip_suffix(".jsonl").unwrap(),
                Uuid::new_v4()
            ),
        );
    }
    assert_eq!(f.owner(id), Owner::Limit);
    assert!(f.seen(id).is_none());
}

#[test]
fn watches_late_paths_append_and_rename_without_rewriting_known_rows() {
    let f = Fixture::new();
    let a = f.profile("a");
    let b = f.profile("b");
    let sessions = a.home.join("sessions");
    fs::create_dir(&sessions).unwrap();
    let archive = b.home.join("archived_sessions");
    fs::create_dir(&archive).unwrap();
    let mut w = Worker::new();
    w.tick(&f.db).unwrap();
    let id = Uuid::new_v4();
    let other = Uuid::new_v4();
    let late = sessions.join("2024/02/29");
    fs::create_dir_all(&late).unwrap();
    let mut empty = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(late.join(name(id)))
        .unwrap();
    w.tick(&f.db).unwrap();
    assert!(f.seen(id).is_none());
    let staged = write(&archive, "incoming");
    std::thread::scope(|s| {
        s.spawn(|| empty.write_all(b"still open").unwrap());
        s.spawn(|| fs::rename(staged, archive.join(name(other))).unwrap());
    });
    for _ in 0..8 {
        w.tick(&f.db).unwrap();
    }
    assert_eq!(f.seen(id), Some((a.id, true)));
    assert_eq!(f.seen(other), Some((b.id, true)));
    assert!(
        !f.db
            .db()
            .unwrap()
            .prepare("SELECT 1 FROM rollout_paths WHERE needs_check=1")
            .unwrap()
            .exists([])
            .unwrap()
    );
    let writer = Connection::open(f.root.join("state/router.sqlite3")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    f.db.db().unwrap().busy_timeout(Duration::ZERO).unwrap();
    empty.write_all(b"append").unwrap();
    w.tick(&f.db).unwrap();
    w.reset = Instant::now();
    w.tick(&f.db).unwrap();
    w.tick(&f.db).unwrap();
    let proof = probe(&f.db, id).unwrap();
    assert_eq!(
        proof.resolve(&f.db.db().unwrap()).unwrap(),
        Owner::Unique(a.id)
    );
    writer.execute_batch("ROLLBACK").unwrap();

    // Simulate a dropped MODIFY for an already-indexed empty name. Directory
    // stamps alone cannot see this change: overflow must also reset the path sweep.
    let delayed = Uuid::new_v4();
    let delayed_path = archive.join(name(delayed));
    fs::write(&delayed_path, b"").unwrap();
    for _ in 0..4 {
        w.tick(&f.db).unwrap();
    }
    assert!(f.seen(delayed).is_none());
    fs::write(delayed_path, b"saved").unwrap();
    w.update(
        &f.db,
        Notices {
            overflow: true,
            ..Notices::default()
        },
    )
    .unwrap();
    assert_eq!(f.seen(delayed), Some((b.id, true)));
}

#[test]
fn kernel_overflow_repairs_a_dropped_save_event() {
    let limit: usize = fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    if limit > 32768 {
        return;
    } // Don't flood an unusually large host queue in a unit test.
    let f = Fixture::new();
    let c = f.profile("owner");
    let id = Uuid::new_v4();
    let dir = c.home.join("archived_sessions");
    fs::create_dir(&dir).unwrap();
    let p = dir.join(name(id));
    fs::write(&p, b"").unwrap();
    let mut w = Worker::new();
    w.tick(&f.db).unwrap();
    assert!(f.seen(id).is_none());
    let staging = dir.join("staging");
    for _ in 0..=limit / 2 {
        fs::File::create(&staging).unwrap();
        fs::remove_file(&staging).unwrap();
    }
    fs::write(&p, b"save after overflow").unwrap();
    let mut lost = false;
    for _ in 0..=limit {
        let n = w.watches.events();
        assert!(!n.files.contains(&id));
        if n.overflow {
            w.update(&f.db, n).unwrap();
            lost = true;
            break;
        }
        if !n.directory {
            break;
        }
    }
    assert!(lost);
    assert_eq!(f.seen(id), Some((c.id, true)));
}

#[test]
fn missing_watches_degrade_to_bounded_reconciliation() {
    let f = Fixture::new();
    let c = f.profile("late");
    let mut w = Worker::new();
    w.watches.fd = None;
    w.tick(&f.db).unwrap();
    let dir = c.home.join("archived_sessions");
    fs::create_dir(&dir).unwrap();
    let id = Uuid::new_v4();
    let p = dir.join(name(id));
    fs::write(&p, b"").unwrap();
    w.refresh = Instant::now();
    w.tick(&f.db).unwrap();
    assert!(f.seen(id).is_none());
    fs::write(&p, b"delayed write without an event").unwrap();
    w.reset = Instant::now();
    w.tick(&f.db).unwrap();
    assert_eq!(f.seen(id), Some((c.id, true)));
    fs::remove_file(p).unwrap();
    w.next_prune = Instant::now();
    w.reset = Instant::now();
    w.refresh = Instant::now();
    w.tick(&f.db).unwrap();
    w.tick(&f.db).unwrap();
    assert_eq!(f.owner(id), Owner::Missing);
    assert_eq!(f.seen(id), Some((c.id, true)));
    assert!(
        !f.db
            .db()
            .unwrap()
            .prepare("SELECT 1 FROM rollout_paths")
            .unwrap()
            .exists([])
            .unwrap()
    );
}
