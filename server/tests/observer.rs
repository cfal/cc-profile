mod common;
use ccpd::observer::{install, spec};
use common::Fixture;
use serde_json::Value;
use std::fs;
#[test]
fn merge_is_scoped_idempotent_and_does_not_follow_symlinks() {
    let f = Fixture::new();
    let a = f.add("codex", "a");
    let p = a.home.join("hooks.json");
    fs::write(&p, br#"{"extra":"kept","hooks":{"Stop":[]}}"#).unwrap();
    install(&f.store, &a).unwrap();
    let first = fs::read(&p).unwrap();
    install(&f.store, &a).unwrap();
    assert_eq!(fs::read(&p).unwrap(), first);
    let v: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(v["extra"], "kept");
    assert!(v["hooks"]["Stop"].is_array());
    assert_eq!(v["hooks"]["SessionStart"][0], spec().unwrap());
    let prior = "'/old/ccpd' observe";
    let mut moved = v;
    moved["hooks"]["SessionStart"][0]["hooks"][0]["command"] = prior.into();
    moved["hooks"]["SessionStart"].as_array_mut().unwrap().push(serde_json::json!({"matcher":"startup","hooks":[{"type":"command","command":"user-tool observe"}]}));
    fs::write(&p, serde_json::to_vec(&moved).unwrap()).unwrap();
    f.store
        .db()
        .unwrap()
        .execute(
            "UPDATE installed_observers SET command=? WHERE config_id=?",
            rusqlite::params![prior, a.id],
        )
        .unwrap();
    install(&f.store, &a).unwrap();
    let updated = fs::read_to_string(&p).unwrap();
    assert!(!updated.contains(prior) && updated.contains("user-tool observe"));
    fs::write(&p, "invalid").unwrap();
    assert!(install(&f.store, &a).is_err());
    assert_eq!(fs::read_to_string(&p).unwrap(), "invalid");
    fs::remove_file(&p).unwrap();
    std::os::unix::fs::symlink(f.root.join("elsewhere"), &p).unwrap();
    assert!(install(&f.store, &a).is_err());
    assert!(!f.root.join("elsewhere").exists());
}
