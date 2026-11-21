mod common;
use ccpd::store::{Page, config};
use common::Fixture;
use serde_json::json;

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
    let rev = config(&*f.store.db().unwrap(), a.id).unwrap().revision;
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
fn proxy_secrets_are_write_only_and_canonical_aliases_are_rejected() {
    let f = Fixture::new();
    let a = f.add("claude", "work");
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
