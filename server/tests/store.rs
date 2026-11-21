mod common;
use ccpd::store::{Page, config};
use common::Fixture;
use serde_json::json;

#[test]
fn upgrades_pin_creation_time_without_rebinding_a_missing_home() {
    let f = Fixture::new();
    let c = f.add("claude", "present");
    let offline = f.add("codex", "offline");
    std::fs::rename(&offline.home, f.root.join("original")).unwrap();
    f.store
        .db()
        .unwrap()
        .execute_batch("DROP INDEX config_identity; ALTER TABLE configs DROP COLUMN birth_ns; PRAGMA user_version=2;")
        .unwrap();
    let open = || ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    let upgraded = open();
    assert!(config(&upgraded.db().unwrap(), c.id).unwrap().present());
    assert!(
        !config(&upgraded.db().unwrap(), offline.id)
            .unwrap()
            .present()
    );
    std::fs::rename(f.root.join("original"), &offline.home).unwrap();
    assert!(config(&open().db().unwrap(), offline.id).unwrap().present());
    // Deterministic even on filesystems that don't immediately recycle a deleted inode.
    upgraded
        .db()
        .unwrap()
        .execute("UPDATE configs SET birth_ns=birth_ns-1 WHERE id=?", [c.id])
        .unwrap();
    assert!(!config(&open().db().unwrap(), c.id).unwrap().present());
}

#[test]
fn current_is_per_provider_and_disable_does_not_elect_a_replacement() {
    let f = Fixture::new();
    let a = f.add("claude", "a");
    let b = f.add("claude", "b");
    let c = f.add("codex", "c");
    f.store.current("claude", Some(a.id)).unwrap();
    f.store.current("codex", Some(c.id)).unwrap();
    assert!(
        f.store
            .edit(a.id, &json!({"revision":a.revision,"enabled":false}))
            .is_err()
    );
    let rev = config(&f.store.db().unwrap(), a.id).unwrap().revision;
    f.store
        .edit(a.id, &json!({"revision":rev,"enabled":false}))
        .unwrap();
    let db = f.store.db().unwrap();
    for id in [a.id, b.id] {
        assert!(!config(&db, id).unwrap().current);
    }
    assert!(config(&db, c.id).unwrap().current);
}
#[test]
fn a_reused_inode_at_a_new_path_can_be_registered_without_inheriting_the_owner() {
    let f = Fixture::new();
    let original = f.add("claude", "old");
    let next = f.root.join("replacement");
    std::fs::rename(&original.home, &next).unwrap();
    f.store
        .db()
        .unwrap()
        .execute(
            "UPDATE configs SET birth_ns=birth_ns-1 WHERE id=?",
            [original.id],
        )
        .unwrap();
    let new = f
        .store
        .register(&json!({"tool":"claude","label":"new","home":next}))
        .unwrap();
    assert_ne!(new.id, original.id);
    let reopened = ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    let db = reopened.db().unwrap();
    assert!(!config(&db, original.id).unwrap().present());
    assert!(config(&db, new.id).unwrap().present());
    assert_eq!(
        db.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn proxy_secrets_are_write_only_and_canonical_aliases_are_rejected() {
    let f = Fixture::new();
    let a = f.add("claude", "work");
    f.store
        .db()
        .unwrap()
        .execute("UPDATE configs SET home=home||'/' WHERE id=?", [a.id])
        .unwrap();
    let read = config(&f.store.db().unwrap(), a.id).unwrap();
    assert!(read.present());
    assert_eq!(
        ccpd::native::env_patch(&read).0["CLAUDE_CONFIG_DIR"],
        a.home.to_str().unwrap()
    );
    let v=f.store.edit(a.id,&json!({"revision":a.revision,"proxy_mode":"custom","https_proxy":"http://user:secret@proxy:8080"})).unwrap();
    assert!(!v.public().to_string().contains("secret"));
    assert_eq!(v.public()["https_proxy"]["credentials_saved"], true);
    assert!(
        f.store
            .edit(
                a.id,
                &json!({"revision":v.revision,"https_proxy":"socks5://proxy"})
            )
            .is_err()
    );
    let alias = f.root.join("current");
    std::os::unix::fs::symlink(&a.home, &alias).unwrap();
    assert!(
        f.store
            .register(&json!({"tool":"codex","label":"alias","home":alias}))
            .is_err()
    );
}
#[test]
fn immutable_owners_and_keyset_pages_survive_new_insertions() {
    let f = Fixture::new();
    let a = f.add("claude", "a");
    let b = f.add("claude", "b");
    let insert = |i: i32| {
        f.store.write(|db|{db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) VALUES('claude',?,?,'assigned','/ws',1,1)",rusqlite::params![format!("id-{i}"),a.id])?;Ok(())}).unwrap()
    };
    for i in 0..60 {
        insert(i);
    }
    let first = f.store.page(true, &Page::default()).unwrap();
    insert(60);
    let second = f
        .store
        .page(
            true,
            &Page {
                before: first["next_cursor"].as_i64(),
                ..Page::default()
            },
        )
        .unwrap();
    assert_eq!(first["items"][24]["seq"], 36);
    assert_eq!(second["items"][0]["seq"], 35);
    assert!(
        f.store
            .db()
            .unwrap()
            .execute("UPDATE sessions SET owner=?", [b.id])
            .is_err()
    );
}

#[test]
fn explicit_resume_and_filtered_pages_have_indexed_access_at_scale() {
    let f = Fixture::new();
    let a = f.add("claude", "many");
    let db = f.store.db().unwrap();
    db.execute(
        "WITH RECURSIVE nums(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM nums WHERE x<100000)
        INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms)
        SELECT 'claude',printf('60000000-0000-4000-8000-%012d',x),?,'assigned','/ws',0,0 FROM nums",
        [a.id],
    )
    .unwrap();
    for q in [
        "SELECT owner FROM sessions WHERE tool='claude' AND native_id='60000000-0000-4000-8000-000000000002'",
        "SELECT seq FROM sessions WHERE tool='claude' AND owner=1 AND seq<99000 ORDER BY seq DESC LIMIT 26",
    ] {
        let plan: String = db
            .query_row(&format!("EXPLAIN QUERY PLAN {q}"), [], |r| r.get(3))
            .unwrap();
        assert!(plan.contains("SEARCH sessions USING"), "{plan}");
    }
}

#[test]
fn routing_state_cannot_be_registered_as_a_vendor_home() {
    let f = Fixture::new();
    assert!(
        f.store
            .register(&json!({"tool":"claude","label":"wrong","home":f.root.join("state")}))
            .is_err()
    );
}
