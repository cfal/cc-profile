mod common;
use ccpd::{
    native::{Build, stamp},
    protocol::RoutingMode::*,
    quota::{FRESH_MS, cached_remaining, inputs},
    routing::{Observation, Request, admit, launch, observe},
    store::{Config, Page, config},
    types::now,
};
use common::Fixture;
use serde_json::{Value, json};
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
        login_profile: None,
        request_id: Uuid::new_v4().to_string(),
        ticket: "c1".repeat(32),
        tool: t.into(),
        argv: args.iter().map(|s| s.to_string()).collect(),
        cwd: f.root.clone(),
        binary: Some(b.bin.clone()),
        override_names: vec![],
    }
}

fn cache(f: &Fixture, c: &Config, mut snapshot: Value, ms: i64) {
    if c.tool == "codex" {
        snapshot["file_auth_verified"] = true.into();
    }
    f.store.db().unwrap().execute(
        "INSERT OR REPLACE INTO quota_cache(config_id,revision,attempt_ms,success_ms,retry_ms,failures,status,stamp,snapshot)
        VALUES(?,?,?3,?3,0,0,'fresh',?,?)",
        rusqlite::params![c.id,c.revision,ms,inputs(c).unwrap(),snapshot.to_string()]
    ).unwrap();
}
fn window(used: f64) -> Value {
    json!({"windows":[{"kind":"account","used_pct":used}]})
}

#[test]
fn agy_tui_resume_replay_rechecks_the_saved_conversation_with_native_options() {
    use std::os::unix::fs::DirBuilderExt;

    let f = Fixture::new();
    let b = build();
    let home = f.root.join("agy-owner");
    let dir = home.join(".gemini/antigravity-cli/conversations");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    let c = f
        .store
        .register(&json!({"tool":"agy","label":"owner","home":home}))
        .unwrap();
    f.store
        .edit(c.id, &json!({"enabled":true,"revision":c.revision}))
        .unwrap();
    let id = Uuid::new_v4();
    let saved = dir.join(format!("{id}.db"));
    std::fs::write(&saved, "fixture").unwrap();
    let r = req(
        &f,
        "agy",
        &["--conversation", &id.to_string(), "--sandbox"],
        &b,
    );
    assert_eq!(launch(&f.store, &b, &r).unwrap().config_id, c.id);
    assert_eq!(launch(&f.store, &b, &r).unwrap().config_id, c.id);
    let first = &f.store.page(true, &Page::default()).unwrap()["items"][0];
    assert_eq!(first["evidence"], "saved_file");
    let reopened = ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    assert_eq!(
        reopened.page(true, &Page::default()).unwrap()["items"],
        json!([first])
    );
    let again = req(&f, "agy", &["--conversation", &id.to_string()], &b);
    reopened
        .db()
        .unwrap()
        .execute("UPDATE sessions SET last_routed_ms=1", [])
        .unwrap();
    launch(&reopened, &b, &again).unwrap();
    assert_eq!(
        reopened.page(true, &Page::default()).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let row = &reopened.page(true, &Page::default()).unwrap()["items"][0];
    assert_eq!(row["seq"], first["seq"]);
    assert_eq!(row["bound_ms"], first["bound_ms"]);
    assert!(row["last_routed_ms"].as_i64().unwrap() > 1);
    std::fs::remove_file(saved).unwrap();
    assert_eq!(launch(&f.store, &b, &r).unwrap_err().1, "stale_plan");
    let acp = req(&f, "agy", &["acp"], &b);
    launch(&f.store, &b, &acp).unwrap();
    let mut event = Observation {
        request_id: acp.request_id,
        ticket: acp.ticket,
        event: "session/load".into(),
        source: "acp".into(),
        native_id: id.to_string(),
        workspace: Some("/acp".into()),
    };
    assert!(observe(&f.store, &event).is_err());
    event.event = "session/new".into();
    observe(&f.store, &event).unwrap();
    event.event = "session/load".into();
    observe(&f.store, &event).unwrap();
    let confirmed = &f.store.page(true, &Page::default()).unwrap()["items"][0];
    assert_eq!(confirmed["seq"], first["seq"]);
    assert_eq!(confirmed["evidence"], "observed");
    assert_eq!(confirmed["workspace"], "/acp");
    assert_eq!(confirmed["tui_workspace"], f.root.to_str().unwrap());
}

#[test]
fn an_empty_nested_git_directory_does_not_hide_outer_codex_settings() {
    use ccpd::quota::no_codex_project_config;
    use std::fs;
    let f = Fixture::new();
    fs::create_dir(f.root.join(".git")).unwrap();
    fs::write(f.root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir(f.root.join(".codex")).unwrap();
    fs::write(
        f.root.join(".codex/config.toml"),
        "cli_auth_credentials_store='keyring'\n",
    )
    .unwrap();
    let nested = f.root.join("nested");
    fs::create_dir_all(nested.join(".git")).unwrap();
    assert!(!no_codex_project_config(&nested));
    fs::write(nested.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert!(no_codex_project_config(&nested));
    let worktree = f.root.join("worktree");
    fs::create_dir(&worktree).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: ../.git/worktrees/worktree\n",
    )
    .unwrap();
    assert!(no_codex_project_config(&worktree));
}
#[test]
fn modes_skip_exhaustion_and_round_robin_among_equal_ranks() {
    let f = Fixture::new();
    let exe = build();
    let cs = ["empty", "unknown", "nearly-empty", "roomy-a", "roomy-b"]
        .map(|name| f.add("claude", name));
    for (c, s) in cs.iter().zip([
        window(100.),
        json!({"windows":[{"kind":"model","used_pct":100}]}),
        json!({"windows":[{"kind":"account","used_pct":10},{"kind":"account","used_pct":99.9}]}),
        window(25.),
        window(25.),
    ]) {
        std::fs::write(c.home.join(".credentials.json"), "{}").unwrap();
        cache(&f, c, s, now());
    }
    for (mode, picks) in [
        (RoundRobin, vec![1, 2, 3, 4, 1]),
        (MostRemaining, vec![3, 4, 3]),
        (LeastRemaining, vec![2, 2]),
    ] {
        f.store.set_routing_mode(mode).unwrap();
        for n in picks {
            let p = launch(&f.store, &exe, &req(&f, "claude", &[], &exe)).unwrap();
            assert_eq!((p.config_id, p.decision.as_str()), (cs[n].id, mode.name()));
        }
    }
    f.store.set_routing_mode(Priority).unwrap();
    for c in &cs[..=2] {
        f.store
            .edit(c.id, &json!({"priority":100,"revision":c.revision}))
            .unwrap();
    }
    for n in [1, 2, 1] {
        let p = launch(&f.store, &exe, &req(&f, "claude", &[], &exe)).unwrap();
        assert_eq!((p.config_id, p.decision.as_str()), (cs[n].id, "priority"));
    }
    for c in &cs[2..] {
        cache(&f, c, window(100.), now());
    }
    for mode in [MostRemaining, LeastRemaining] {
        f.store.set_routing_mode(mode).unwrap();
        assert_eq!(
            launch(&f.store, &exe, &req(&f, "claude", &[], &exe))
                .unwrap()
                .config_id,
            cs[1].id
        );
    }
}

#[test]
fn only_new_admissions_consult_quota_or_request_background_refresh() {
    let f = Fixture::new();
    let exe = build();
    let c = f.add("claude", "one");
    f.store.current("claude", Some(c.id)).unwrap();
    std::fs::write(c.home.join(".credentials.json"), "{}").unwrap();
    f.store.set_routing_mode(MostRemaining).unwrap();
    let new = req(&f, "claude", &[], &exe);
    let first = admit(&f.store, &exe, &new).unwrap();
    assert!(first.refresh_quota);
    let plan = first.plan.unwrap();
    cache(&f, &c, window(100.), now());
    let replay = admit(&f.store, &exe, &new).unwrap();
    assert!(!replay.refresh_quota);
    assert_eq!(replay.plan.unwrap(), plan);
    let mut setup = req(&f, "claude", &["auth", "login"], &exe);
    setup.login_profile = Some(c.id);
    for (request, reason) in [
        (
            req(&f, "claude", &["--resume", &plan.argv[1]], &exe),
            "sticky",
        ),
        (req(&f, "claude", &["--version"], &exe), "current"),
        (setup, "profile_login"),
    ] {
        let admission = admit(&f.store, &exe, &request).unwrap();
        assert!(!admission.refresh_quota);
        assert_eq!(admission.plan.unwrap().decision, reason);
    }
    for mode in [RoundRobin, Priority, MostRemaining, LeastRemaining] {
        f.store.set_routing_mode(mode).unwrap();
        let request = req(&f, "claude", &[], &exe);
        for refresh in [mode.uses_remaining(), false] {
            let admission = admit(&f.store, &exe, &request).unwrap();
            assert_eq!(admission.plan.unwrap_err().1, "quota_exhausted");
            assert_eq!(admission.refresh_quota, refresh);
        }
        let row = &f.store.page(false, &Page::default()).unwrap()["items"][0];
        assert_eq!(row["state"], "denied");
        assert_eq!(row["reason"], "quota_exhausted");
    }
}

#[test]
fn only_recent_successful_identifiable_quota_is_trusted() {
    let f = Fixture::new();
    let c = f.add("codex", "one");
    let auth = c.home.join("auth.json");
    let ms = 999_999;
    let score = |at| cached_remaining(&f.store.db().unwrap(), &c, at);
    cache(&f, &c, window(100.), ms);
    assert_eq!(score(ms), None); // No local login identity.
    std::fs::write(&auth, "{}").unwrap();
    cache(&f, &c, window(100.), ms);
    assert_eq!(score(ms - 1), None);
    assert_eq!(score(ms + FRESH_MS - 1), Some(0.));
    assert_eq!(score(ms + FRESH_MS), None);
    for change in [
        "status='rate_limited'",
        "failures=1",
        "attempt_ms=1",
        "success_ms=NULL",
        "success_ms='invalid'",
        "snapshot='not JSON'",
        "snapshot=NULL",
        "stamp='different'",
    ] {
        f.store
            .db()
            .unwrap()
            .execute(&format!("UPDATE quota_cache SET {change}"), [])
            .unwrap();
        assert_eq!(score(ms), None, "{change}");
        cache(&f, &c, window(100.), ms);
    }
    std::fs::write(&auth, "changed login").unwrap();
    assert_eq!(score(ms), None);
    cache(&f, &c, window(100.), ms);
    let edited = f
        .store
        .edit(c.id, &json!({"revision":c.revision,"proxy_mode":"direct"}))
        .unwrap();
    assert_eq!(cached_remaining(&f.store.db().unwrap(), &edited, ms), None);
}

#[test]
fn codex_admission_and_tickets_are_scoped_to_the_launch_mode() {
    let f = Fixture::new();
    let exe = build();
    let a = f.add("codex", "a");
    let b = f.add("codex", "b");
    let r = req(&f, "codex", &[], &exe);
    let start = |r: &Request| launch(&f.store, &exe, r);
    let p = start(&r).unwrap();
    assert_eq!(p, start(&r).unwrap());
    assert!(p.argv.is_empty() && p.config_id == a.id);
    assert!(!p.env_set.contains_key("CCP_OBSERVE_TICKET"));
    let mut o = Observation {
        request_id: r.request_id.clone(),
        ticket: r.ticket.clone(),
        event: "SessionStart".into(),
        source: "startup".into(),
        native_id: Uuid::new_v4().to_string(),
        workspace: None,
    };
    assert!(observe(&f.store, &o).is_err());
    o.event = "thread/started".into();
    o.source = "app-server".into();
    observe(&f.store, &o).unwrap();
    let server = req(&f, "codex", &["app-server", "--listen", "stdio://"], &exe);
    assert_eq!(start(&server).unwrap().config_id, b.id);
    o.request_id = server.request_id;
    o.ticket = server.ticket;
    assert!(observe(&f.store, &o).is_err());
    o.event = "thread/start".into();
    assert_eq!(observe(&f.store, &o).unwrap_err().1, "observation_conflict");
    o.native_id = Uuid::new_v4().to_string();
    observe(&f.store, &o).unwrap();
    observe(&f.store, &o).unwrap();
    let mut resume = req(
        &f,
        "codex",
        &["app-server", "--for-session", &o.native_id],
        &exe,
    );
    assert_eq!(start(&resume).unwrap().config_id, b.id);
    f.store
        .edit(b.id, &json!({"revision":b.revision,"enabled":false}))
        .unwrap();
    resume.request_id = Uuid::new_v4().to_string();
    assert_eq!(start(&resume).unwrap_err().1, "owner_disabled");
    resume.argv = vec!["--remote".into(), "unix:///wrong".into()];
    resume.request_id = Uuid::new_v4().to_string();
    assert!(start(&resume).is_err());
}

#[test]
fn upgrading_v4_keeps_the_codex_owner_and_cursor_without_touching_native_hooks() {
    let f = Fixture::new();
    let exe = build();
    let first = f.add("codex", "one");
    let second = f.add("codex", "two");
    let uid = Uuid::new_v4().to_string();
    let pending = req(&f, "codex", &[], &exe);
    launch(&f.store, &exe, &pending).unwrap();
    let path = first.home.join("hooks.json");
    let hooks = r#"{"hooks":{"SessionStart":[{"matcher":"startup","hooks":[{"type":"command","command":"my-other-hook"}]}]}}"#;
    std::fs::write(&path, hooks).unwrap();
    {
        let db = f.store.db().unwrap();
        db.execute_batch(
            "DROP TABLE settings; ALTER TABLE configs DROP COLUMN priority;
            ALTER TABLE configs ADD COLUMN observer_stamp TEXT;
            CREATE TABLE installed_observers(config_id INTEGER PRIMARY KEY, command TEXT);
            PRAGMA user_version=4;",
        )
        .unwrap();
        db.execute(
            "INSERT INTO installed_observers VALUES(?,?)",
            rusqlite::params![first.id, "'/old/bin/ccpd' observe"],
        )
        .unwrap();
        db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) VALUES('codex',?,?,'observed','/work',1,1)",rusqlite::params![uid,first.id]).unwrap();
    }
    let upgraded = ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), hooks);
    assert_eq!(
        launch(&upgraded, &exe, &req(&f, "codex", &["resume", &uid], &exe))
            .unwrap()
            .config_id,
        first.id
    );
    assert_eq!(
        launch(&upgraded, &exe, &req(&f, "codex", &[], &exe))
            .unwrap()
            .config_id,
        second.id
    );
    assert_eq!(
        upgraded
            .db()
            .unwrap()
            .query_row(
                "SELECT state FROM launches WHERE request_id=?",
                [&pending.request_id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "unobserved"
    );
}

#[test]
fn login_plan_targets_only_the_named_claude_profile_even_on_retry() {
    let f = Fixture::new();
    let exe = build();
    let work = f.add("claude", "work");
    f.store.current("claude", Some(work.id)).unwrap();
    let other = f.add("codex", "other");
    let personal = f.add("claude", "personal");
    f.store
        .edit(
            personal.id,
            &json!({"revision":personal.revision, "enabled":false}),
        )
        .unwrap();
    let mut r = req(&f, "claude", &["auth", "login"], &exe);
    r.login_profile = Some(personal.id);
    let p = launch(&f.store, &exe, &r).unwrap();
    assert_eq!(p.config_id, personal.id);
    assert_eq!(p.argv, ["auth", "login"]);
    assert_eq!(launch(&f.store, &exe, &r).unwrap(), p);
    r.request_id = Uuid::new_v4().to_string();
    r.login_profile = Some(other.id);
    assert_eq!(launch(&f.store, &exe, &r).unwrap_err().1, "login_profile");
    r.argv = vec!["chat".into()];
    assert_eq!(launch(&f.store, &exe, &r).unwrap_err().1, "bad_request");
    let db = f.store.db().unwrap();
    assert!(config(&db, work.id).unwrap().current);
    assert_eq!(
        db.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn codex_profile_setup_is_maintenance_only_for_exact_setup_argv() {
    let f = Fixture::new();
    let exe = build();
    let c = f.add("codex", "work");
    f.store
        .edit(c.id, &json!({"revision":c.revision, "enabled":false}))
        .unwrap();
    let mut r = req(&f, "codex", &["login"], &exe);
    assert_eq!(launch(&f.store, &exe, &r).unwrap_err().1, "no_current");
    r.request_id = Uuid::new_v4().to_string();
    r.login_profile = Some(c.id);
    let p = launch(&f.store, &exe, &r).unwrap();
    assert_eq!(p, launch(&f.store, &exe, &r).unwrap());
    assert_eq!(p.decision, "profile_login");
    assert_eq!(p.argv, ["login"]);
    assert!(!p.env_set.contains_key("CCP_LAUNCH_ID"));
    assert_eq!(
        f.store
            .db()
            .unwrap()
            .query_row("SELECT cursor FROM tools WHERE tool='codex'", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    r.argv.push("chat".into());
    assert_eq!(launch(&f.store, &exe, &r).unwrap_err().1, "bad_request");
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
    let alias = f.root.join("work");
    std::os::unix::fs::symlink(&f.root, &alias).unwrap();
    let request = req(
        &f,
        "codex",
        &["-C", "work", "exec", "--skip-git-repo-check"],
        &b,
    );
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
    let r = req(&f, "codex", &["--", "--no-daemon"], &b);
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
    let x = req(&f, "codex", &[], &exe);
    let y = req(
        &f,
        "codex",
        &["exec", "--json", "--skip-git-repo-check"],
        &exe,
    );
    assert!(launch(&f.store, &exe, &x).unwrap().argv.is_empty());
    assert_eq!(launch(&f.store, &exe, &y).unwrap().argv, y.argv);
    f.store
        .edit(a.id, &json!({"enabled":false,"revision":a.revision}))
        .unwrap();
    for (r, c) in [(&y, &b), (&x, &a)] {
        let mut o = Observation {
            workspace: None,
            request_id: r.request_id.clone(),
            ticket: r.ticket.clone(),
            native_id: Uuid::new_v4().to_string(),
            event: "thread/started".into(),
            source: "app-server".into(),
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
    let uid = Uuid::new_v4().to_string();
    for c in [&first, &other] {
        let r = req(&f, "codex", &["exec", "--skip-git-repo-check"], &exe);
        assert_eq!(launch(&f.store, &exe, &r).unwrap().config_id, c.id);
        let o = Observation {
            workspace: None,
            request_id: r.request_id,
            ticket: r.ticket,
            native_id: uid.clone(),
            event: "thread/started".into(),
            source: "app-server".into(),
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
