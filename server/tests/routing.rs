mod common;
use ccpd::{
    native::{Build, stamp},
    routing::{Observation, Request, launch, observe},
    store::{Page, config},
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
#[test]
fn retries_disabled_resumes_and_redacted_log() {
    let f = Fixture::new();
    let a = f.add("claude", "a");
    let b = f.add("claude", "b");
    let exe = build();
    let mut x = req(&f, "claude", &["-p", "private prompt"], &exe);
    let p = launch(&f.store, &exe, &x).unwrap();
    assert_eq!(p["config_id"], a.id);
    assert_eq!(p, launch(&f.store, &exe, &x).unwrap());
    x.argv.clear();
    assert_eq!(
        launch(&f.store, &exe, &x).unwrap_err().1,
        "request_conflict"
    );
    let id = p["argv"][1].as_str().unwrap();
    f.store.current("claude", Some(b.id)).unwrap();
    let r = req(&f, "claude", &["--resume", id], &exe);
    assert_eq!(launch(&f.store, &exe, &r).unwrap()["config_id"], a.id);
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
                .filter(|p| p["config_id"] == a.id)
                .count(),
            10
        );
    });
    let reopened = ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    assert_eq!(
        launch(&reopened, &exe, &req(&f, "claude", &[], &exe)).unwrap()["config_id"],
        a.id
    );
}
#[test]
fn delayed_reverse_observations_bind_the_admitted_configs() {
    let f = Fixture::new();
    let exe = build();
    let a = f.add("codex", "a");
    let b = f.add("codex", "b");
    for c in [&a, &b] {
        f.store
            .db()
            .unwrap()
            .execute(
                "UPDATE configs SET observer_stamp=? WHERE id=?",
                rusqlite::params![exe.observer_stamp(c), c.id],
            )
            .unwrap();
    }
    let x = req(&f, "codex", &[], &exe);
    let y = req(&f, "codex", &["exec", "--json"], &exe);
    assert_eq!(
        launch(&f.store, &exe, &x).unwrap()["argv"],
        json!(["--no-daemon"])
    );
    assert_eq!(
        launch(&f.store, &exe, &y).unwrap()["argv"],
        json!(["exec", "--json"])
    );
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
    assert!(!config(&*f.store.db().unwrap(), a.id).unwrap().enabled);
}
