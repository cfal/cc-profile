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
    install(&a).unwrap();
    let first = fs::read(&p).unwrap();
    install(&a).unwrap();
    assert_eq!(fs::read(&p).unwrap(), first);
    let v: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(v["extra"], "kept");
    assert!(v["hooks"]["Stop"].is_array());
    assert_eq!(v["hooks"]["SessionStart"][0], spec().unwrap());
    fs::write(&p, "invalid").unwrap();
    assert!(install(&a).is_err());
    assert_eq!(fs::read_to_string(&p).unwrap(), "invalid");
    fs::remove_file(&p).unwrap();
    std::os::unix::fs::symlink(f.root.join("elsewhere"), &p).unwrap();
    assert!(install(&a).is_err());
    assert!(!f.root.join("elsewhere").exists());
}
