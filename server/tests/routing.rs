mod common;
use ccpd::{
    native::{Build, stamp},
    routing::{Observation, Request, launch, observe},
    store::{Config, Page, config},
};
use common::Fixture;
use serde_json::json;
use uuid::Uuid;
fn build() -> Build {
    let bin = std::env::current_exe().unwrap();
    Build {
        fingerprint: stamp(&bin),
        bin,
        version: "fixture".into(),
        supported: true,
    }
}
fn req(f: &Fixture, t: &str, args: &[&str], b: &Build) -> Request {
    Request {
        request_id: Uuid::new_v4().to_string(),
        ticket: "c1".repeat(32),
        tool: t.into(),
        argv: args.iter().map(|s| s.to_string()).collect(),
        cwd: f.root.clone(),
        binary: Some(b.bin.clone()),
        override_names: vec![],
    }
}
fn approve(f: &Fixture, build: &Build, c: &Config) {
    f.store
        .db()
        .unwrap()
        .execute(
            "UPDATE configs SET observer_stamp=? WHERE id=?",
            rusqlite::params![build.observer_stamp(c), c.id],
        )
        .unwrap();
}
#[test]
fn retries_disabled_resumes_and_redacted_log() {
    let f = Fixture::new();
    let a = f.add("claude", "a");
    let b = f.add("claude", "b");
    let exe = build();
    let mut x = req(&f, "claude", &["-p", "private prompt"], &exe);
    let p = launch(&f.store, &exe, &x).unwrap();
    assert_eq!(p.config_id, a.id);
    assert_eq!(p, launch(&f.store, &exe, &x).unwrap());
    x.argv.clear();
    assert_eq!(
        launch(&f.store, &exe, &x).unwrap_err().1,
        "request_conflict"
    );
    let id = p.argv[1].as_str();
    f.store.current("claude", Some(b.id)).unwrap();
    let r = req(&f, "claude", &["--resume", id], &exe);
    assert_eq!(launch(&f.store, &exe, &r).unwrap().config_id, a.id);
    f.store
        .edit(a.id, &json!({"enabled":false,"revision":a.revision}))
        .unwrap();
    assert_eq!(
        launch(&f.store, &exe, &req(&f, "claude", &["--resume", id], &exe))
            .unwrap_err()
            .1,
        "owner_disabled"
    );
    let rows = f.store.page(false, &Page::default()).unwrap();
    assert_eq!(rows["items"][0]["reason"], "owner_disabled");
    assert!(!rows.to_string().contains("private prompt"));
}
#[test]
fn restart_revalidates_the_native_executable_on_replay() {
    let f = Fixture::new();
    let original = build();
    f.add("claude", "a");
    let r = req(&f, "claude", &[], &original);
    let first = launch(&f.store, &original, &r).unwrap();
    let resume = req(&f, "claude", &["--resume", &first.argv[1]], &original);
    launch(&f.store, &original, &resume).unwrap();
    let mut changed = original.clone();
    changed.bin = f.root.join("other-version");
    std::fs::write(&changed.bin, "new native installation").unwrap();
    changed.fingerprint = stamp(&changed.bin);
    let reopened =
        ccpd::store::Store::open(&ccpd::store::Store::user_db_path(&f.root.join("state"))).unwrap();
    for request in [&r, &resume] {
        assert_eq!(
            launch(&reopened, &changed, request).unwrap_err().1,
            "stale_plan"
        );
    }
    let unverified = Build {
        supported: false,
        ..original
    };
    assert_eq!(
        launch(&reopened, &unverified, &resume).unwrap_err().1,
        "stale_plan"
    );
}

#[test]
fn replay_preserves_a_denied_workspace_error() {
    let f = Fixture::new();
    let b = build();
    let request = req(&f, "codex", &["-C", "missing"], &b);
    for _ in 0..2 {
        assert_eq!(launch(&f.store, &b, &request).unwrap_err().1, "bad_cwd");
    }
}

#[test]
fn replay_does_not_follow_a_redirected_workspace() {
    let f = Fixture::new();
    let b = build();
    let c = f.add("codex", "a");
    approve(&f, &b, &c);
    let alias = f.root.join("work");
    std::os::unix::fs::symlink(&f.root, &alias).unwrap();
    let request = req(&f, "codex", &["-C", "work", "exec"], &b);
    launch(&f.store, &b, &request).unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&c.home, alias).unwrap();
    assert_eq!(launch(&f.store, &b, &request).unwrap_err().1, "stale_plan");
}

#[test]
fn simultaneous_new_chats_split_evenly_and_survive_reopen() {
    let f = Fixture::new();
    let a = f.add("claude", "a");
    f.add("claude", "b");
    let exe = build();
    std::thread::scope(|s| {
        let jobs = (0..20)
            .map(|_| s.spawn(|| launch(&f.store, &exe, &req(&f, "claude", &[], &exe)).unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            jobs.into_iter()
                .map(|h| h.join().unwrap())
                .filter(|p| p.config_id == a.id)
                .count(),
            10
        );
    });
    let reopened = ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    assert_eq!(
        launch(&reopened, &exe, &req(&f, "claude", &[], &exe))
            .unwrap()
            .config_id,
        a.id
    );
}

#[test]
fn failed_audit_insert_rolls_back_the_assignment_and_cursor() {
    let f = Fixture::new();
    let b = build();
    let a = f.add("claude", "a");
    f.store.db().unwrap().execute_batch(
        "CREATE TRIGGER broken_log BEFORE INSERT ON launches BEGIN SELECT RAISE(ABORT,'fixture'); END;"
    ).unwrap();
    let r = req(&f, "claude", &[], &b);
    assert!(launch(&f.store, &b, &r).is_err());
    assert!(
        f.store.page(true, &Page::default()).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = f.store.db().unwrap();
    assert_eq!(
        db.query_row("SELECT cursor FROM tools WHERE tool='claude'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER broken_log").unwrap();
    drop(db);
    assert_eq!(launch(&f.store, &b, &r).unwrap().config_id, a.id);
}

#[test]
fn a_retry_cannot_resurrect_a_disabled_plan_or_change_the_native_arguments() {
    let f = Fixture::new();
    let b = build();
    let c = f.add("codex", "x");
    approve(&f, &b, &c);
    let r = req(&f, "codex", &["--no-daemon", "--", "--no-daemon"], &b);
    assert_eq!(launch(&f.store, &b, &r).unwrap().argv, r.argv);
    f.store
        .edit(c.id, &json!({"enabled":false,"revision":c.revision}))
        .unwrap();
    assert_eq!(launch(&f.store, &b, &r).unwrap_err().1, "stale_plan");
    let mut denied = req(&f, "codex", &[], &b);
    denied.override_names.push("CODEX_API_KEY".into());
    assert!(
        launch(&f.store, &b, &denied)
            .unwrap_err()
            .2
            .contains("CODEX_API_KEY")
    );
}
#[test]
fn delayed_reverse_observations_bind_the_admitted_configs() {
    let f = Fixture::new();
    let exe = build();
    let a = f.add("codex", "a");
    let b = f.add("codex", "b");
    for c in [&a, &b] {
        approve(&f, &exe, c);
    }
    let x = req(&f, "codex", &[], &exe);
    let y = req(&f, "codex", &["exec", "--json"], &exe);
    assert_eq!(launch(&f.store, &exe, &x).unwrap().argv, ["--no-daemon"]);
    assert_eq!(launch(&f.store, &exe, &y).unwrap().argv, ["exec", "--json"]);
    f.store
        .edit(a.id, &json!({"enabled":false,"revision":a.revision}))
        .unwrap();
    for (r, c) in [(&y, &b), (&x, &a)] {
        let mut o = Observation {
            request_id: r.request_id.clone(),
            ticket: r.ticket.clone(),
            native_id: Uuid::new_v4().to_string(),
            event: "SessionStart".into(),
            source: "startup".into(),
        };
        observe(&f.store, &o).unwrap();
        observe(&f.store, &o).unwrap();
        let rows = f
            .store
            .page(
                true,
                &Page {
                    native_id: Some(o.native_id.clone()),
                    ..Page::default()
                },
            )
            .unwrap();
        assert_eq!(rows["items"][0]["config_id"], c.id);
        o.native_id = Uuid::new_v4().to_string();
        assert_eq!(observe(&f.store, &o).unwrap_err().1, "observation_conflict");
    }
    assert!(!config(&f.store.db().unwrap(), a.id).unwrap().enabled);
}

#[test]
fn a_second_config_observing_an_owned_uuid_is_always_a_conflict() {
    let f = Fixture::new();
    let exe = build();
    let first = f.add("codex", "first");
    let other = f.add("codex", "other");
    for c in [&first, &other] {
        approve(&f, &exe, c);
    }
    let uid = Uuid::new_v4().to_string();
    for c in [&first, &other] {
        let r = req(&f, "codex", &["exec"], &exe);
        assert_eq!(launch(&f.store, &exe, &r).unwrap().config_id, c.id);
        let o = Observation {
            request_id: r.request_id,
            ticket: r.ticket,
            native_id: uid.clone(),
            event: "SessionStart".into(),
            source: "startup".into(),
        };
        if c.id == first.id {
            observe(&f.store, &o).unwrap();
        } else {
            for _ in 0..2 {
                assert_eq!(observe(&f.store, &o).unwrap_err().1, "observation_conflict");
            }
        }
    }
    assert_eq!(
        f.store.page(true, &Page::default()).unwrap()["items"][0]["config_id"],
        first.id
    );
    let log = f.store.page(false, &Page::default()).unwrap();
    assert_eq!(log["items"][0]["state"], "conflict");
    assert_eq!(log["items"][0]["native_id"], uid);
}

#[test]
fn a_replaced_current_home_is_not_an_unset_current_or_a_new_registration() {
    let f = Fixture::new();
    let exe = build();
    let c = f.add("claude", "original");
    assert_eq!(
        launch(&f.store, &exe, &req(&f, "claude", &["--version"], &exe))
            .unwrap_err()
            .1,
        "no_current"
    );
    f.store.current("claude", Some(c.id)).unwrap();
    std::fs::rename(&c.home, f.root.join("backup")).unwrap();
    std::fs::create_dir(&c.home).unwrap();
    let op = req(&f, "claude", &["auth", "status"], &exe);
    for _ in 0..2 {
        assert_eq!(
            launch(&f.store, &exe, &op).unwrap_err().1,
            "current_missing"
        );
    }
    let log = f.store.page(false, &Page::default()).unwrap();
    assert_eq!(log["items"][0]["config_id"], c.id);
    let err = f
        .store
        .register(&json!({"tool":"claude","label":"replacement","home":c.home}))
        .unwrap_err();
    assert_eq!(err.1, "home_replaced");
    assert!(err.2.contains("new path"));
    for suffix in ["/", "//", "/./"] {
        let alias = format!("{}{suffix}", c.home.display());
        assert_eq!(
            f.store
                .register(&json!({"tool":"codex","label":"new","home":alias}))
                .unwrap_err()
                .1,
            "home_replaced"
        );
    }
}
