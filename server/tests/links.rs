mod common;
use ccpd::links;
use common::Fixture;
use std::fs;
#[test]
fn current_projection_is_relative_optional_and_never_replaces_user_data() {
    let f = Fixture::new();
    let c = f.add("claude", "work");
    let dir = f.root.join("compat");
    fs::create_dir(&dir).unwrap();
    let outer = f.root.join(".claude");
    let state = f.root.join("state");
    let rt = f.root.join("runtime");
    f.store.current("claude", Some(c.id)).unwrap();
    links::configure(
        &f.store,
        "claude",
        dir.to_str(),
        outer.to_str(),
        &state,
        &rt,
    )
    .unwrap();
    assert!(!fs::read_link(&outer).unwrap().is_absolute());
    assert_eq!(outer.canonicalize().unwrap(), c.home);
    f.store.current("claude", None).unwrap();
    links::repair(&f.store, "claude").unwrap();
    assert!(outer.is_symlink() && !outer.exists());
    fs::remove_file(&outer).unwrap();
    fs::write(&outer, "untouchable").unwrap();
    assert!(links::repair(&f.store, "claude").is_err());
    assert_eq!(fs::read_to_string(outer).unwrap(), "untouchable");
}
