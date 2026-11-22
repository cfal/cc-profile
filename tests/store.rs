mod common;
use ccpd::store::{Page, config};
use common::Fixture;
use serde_json::json;

#[test]
fn directory_identity_is_compatible_with_existing_native_databases() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let c = f.add("claude", "work");
    let metadata = std::fs::symlink_metadata(&c.home).unwrap();
    assert_eq!((c.dev, c.ino), (metadata.dev(), metadata.ino()));
    #[cfg(target_env = "gnu")]
    {
        let native_birth = metadata
            .created()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let stored: i64 = f
            .store
            .db()
            .unwrap()
            .query_row("SELECT birth_ns FROM configs WHERE id=?", [c.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored as u128, native_birth.as_nanos());
    }
}

#[test]
fn upgrades_pin_creation_time_without_rebinding_a_missing_home() {
    let f = Fixture::new();
    let c = f.add("claude", "present");
    let offline = f.add("codex", "offline");
    std::fs::rename(&offline.home, f.root.join("original")).unwrap();
    f.store
        .db()
        .unwrap()
        .execute_batch("DROP TABLE settings; ALTER TABLE configs DROP COLUMN priority;
            DROP INDEX config_identity; ALTER TABLE configs DROP COLUMN birth_ns; ALTER TABLE configs ADD COLUMN observer_stamp TEXT;
            PRAGMA user_version=2;")
        .unwrap();
    let open = || ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    let upgraded = open();
    assert!(config(&upgraded.db().unwrap(), c.id).unwrap().present());
    assert!(
        !config(&upgraded.db().unwrap(), offline.id)
            .unwrap()
            .present()
    );
    let registration = json!({"tool":"codex","label":"second","home":f.root.join("original")});
    assert_eq!(
        upgraded.register(&registration).unwrap_err().1,
        "legacy_home"
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
fn a_conflicting_legacy_pin_never_prevents_opening_the_router() {
    let f = Fixture::new();
    let owner = f.add("codex", "owner");
    let db = f.store.db().unwrap();
    // An early v4 registration recorded this identity while the legacy home was offline.
    let birth: i64 = db
        .query_row("SELECT birth_ns FROM configs WHERE id=?", [owner.id], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute("UPDATE configs SET birth_ns=NULL WHERE id=?", [owner.id])
        .unwrap();
    db.execute("INSERT INTO configs(tool,label,home,dev,ino,birth_ns) SELECT tool,'second',?,dev,ino,? FROM configs WHERE id=?",
        rusqlite::params![f.root.join("moved").to_str(),birth,owner.id]).unwrap();
    let second = db.last_insert_rowid();
    drop(db);
    let store = ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    assert!(!config(&store.db().unwrap(), owner.id).unwrap().present());
    std::fs::rename(&owner.home, f.root.join("moved")).unwrap();
    assert!(config(&store.db().unwrap(), second).unwrap().present());
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
fn adding_agy_preserves_existing_owners_cursors_and_quota_cache() {
    let f = Fixture::new();
    let path = f.root.join("state/v6.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(include_str!("../src/schema.sql")).unwrap();
    db.execute_batch(include_str!("../src/panel_password.sql"))
        .unwrap();
    db.execute_batch("ALTER TABLE configs ADD COLUMN birth_ns INTEGER;")
        .unwrap();
    db.execute_batch(include_str!("../src/directory_identity.sql"))
        .unwrap();
    db.execute_batch("ALTER TABLE configs DROP COLUMN observer_stamp;
        PRAGMA user_version=6;
        INSERT INTO configs(id,tool,label,home,dev,ino,birth_ns,enabled,current,revision,proxy_mode,proxy_url)
            VALUES(42,'codex','existing','/offline/home',1,2,3,1,1,4,'custom','http://saved:3128/');
        UPDATE sqlite_sequence SET seq=50 WHERE name='configs';
        UPDATE tools SET cursor=42 WHERE tool='codex';
        INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms)
            VALUES('codex','existing-id',42,'observed','/ws',1,1);
        UPDATE sqlite_sequence SET seq=90 WHERE name='sessions';
        INSERT INTO quota_cache(config_id,revision,attempt_ms,retry_ms,failures,status,stamp,snapshot)
            VALUES(42,4,1,0,0,'ok','original','{}');").unwrap();
    drop(db);
    let upgraded = ccpd::store::Store::open(&path).unwrap();
    let db = upgraded.db().unwrap();
    let c = config(&db, 42).unwrap();
    assert!(c.enabled && c.current);
    assert_eq!(c.revision, 4);
    assert_eq!(c.priority, 0);
    assert_eq!(
        ccpd::store::routing_mode(&db).unwrap().name(),
        "round_robin"
    );
    assert_eq!(c.proxy_url.as_deref(), Some("http://saved:3128/"));
    assert!(
        db.prepare(
            "SELECT 1 FROM sessions s JOIN quota_cache q ON s.owner=q.config_id
        JOIN tools t ON t.tool=s.tool AND t.cursor=s.owner
        WHERE s.native_id='existing-id' AND s.owner=42 AND q.stamp='original'"
        )
        .unwrap()
        .exists([])
        .unwrap()
    );
    db.execute(
        "INSERT INTO configs(tool,label,home,dev,ino,birth_ns) VALUES('agy','new','/new',1,3,4)",
        [],
    )
    .unwrap();
    assert_eq!(db.last_insert_rowid(), 51);
    db.execute(
        "INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms)
        VALUES('agy','new-uuid',51,'saved_file','/ws',2,2)",
        [],
    )
    .unwrap();
    assert_eq!(db.last_insert_rowid(), 91);
    assert!(
        !db.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap()
    );
}

#[test]
fn upgrading_an_old_tui_startup_does_not_create_a_mapping() {
    let f = Fixture::new();
    f.store.db().unwrap().execute_batch(
        "INSERT INTO configs(id,tool,label,home,dev,ino,birth_ns) VALUES(42,'agy','old','/offline',1,2,3);
         INSERT INTO launches(request_id,tool,intent,state,reason,native_id,config_id,workspace,digest,ticket_digest,created_ms)
           VALUES('old','agy','tui_resume','routed','sticky','legacy-id',42,'/work',x'01',x'01',1);
         PRAGMA user_version=9;",
    ).unwrap();
    let upgraded = ccpd::store::Store::open(&f.root.join("state/router.sqlite3")).unwrap();
    assert_eq!(
        upgraded.page(false, &Page::default()).unwrap()["items"][0]["mapped"],
        false
    );
    assert_eq!(
        upgraded.page(true, &Page::default()).unwrap()["items"],
        json!([])
    );
}

#[test]
fn unsupported_config_schema_is_not_rewritten() {
    let f = Fixture::new();
    let path = f.root.join("state/unsupported.sqlite3");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE configs(http_proxy TEXT, https_proxy TEXT);
        INSERT INTO configs VALUES('http://one','http://two'); PRAGMA user_version=5;",
    )
    .unwrap();
    assert_eq!(ccpd::store::Store::open(&path).err().unwrap().1, "schema");
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        5
    );
    assert!(
        db.prepare(
            "SELECT 1 FROM configs WHERE http_proxy='http://one' AND https_proxy='http://two'"
        )
        .unwrap()
        .exists([])
        .unwrap()
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
    let v=f.store.edit(a.id,&json!({"revision":a.revision,"proxy_mode":"custom","proxy_url":"http://user:secret@proxy:8080"})).unwrap();
    let (set, unset) = ccpd::native::env_patch(&v);
    for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
        assert_eq!(set[name], "http://user:secret@proxy:8080/");
    }
    assert!(unset.contains(&"ALL_PROXY".to_owned()));
    assert!(!v.public().to_string().contains("secret"));
    assert_eq!(v.public()["proxy_url"]["credentials_saved"], true);
    assert!(
        f.store
            .edit(
                a.id,
                &json!({"revision":v.revision,"proxy_url":"socks5://proxy"})
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

#[test]
fn admission_settings_and_priority_are_constrained_in_sql() {
    let f = Fixture::new();
    let c = f.add("codex", "one");
    let db = f.store.db().unwrap();
    for priority in ["-1", "101", "0.5", "'bad'", "NULL"] {
        assert!(
            db.execute(
                &format!("UPDATE configs SET priority={priority} WHERE id=?"),
                [c.id]
            )
            .is_err()
        );
    }
    assert!(
        db.execute("UPDATE settings SET routing_mode='unsupported'", [])
            .is_err()
    );
    assert!(
        db.execute("INSERT INTO settings VALUES(2,'priority')", [])
            .is_err()
    );
    assert_eq!(config(&db, c.id).unwrap().priority, 0);
}
