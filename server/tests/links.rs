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

#[test]
fn detaching_does_not_follow_a_replaced_parent_or_partially_remove_links() {
    let f = Fixture::new();
    let c = f.add("codex", "work");
    let root = f.root.join("profiles");
    fs::create_dir(&root).unwrap();
    let outer = f.root.join(".codex");
    let state = f.root.join("state");
    let rt = f.root.join("runtime");
    f.store.current("codex", Some(c.id)).unwrap();
    links::configure(
        &f.store,
        "codex",
        root.to_str(),
        outer.to_str(),
        &state,
        &rt,
    )
    .unwrap();
    let inner = root.join("codex/current");
    let saved = fs::read_link(&inner).unwrap();
    fs::rename(&root, f.root.join("original")).unwrap();
    let other = f.root.join("other");
    fs::create_dir_all(other.join("codex")).unwrap();
    std::os::unix::fs::symlink(&saved, other.join("codex/current")).unwrap();
    std::os::unix::fs::symlink(&other, &root).unwrap();
    assert!(links::configure(&f.store, "codex", None, None, &state, &rt).is_err());
    assert!(outer.is_symlink());
    assert!(other.join("codex/current").is_symlink());
    assert_eq!(
        links::status(&f.store.db().unwrap(), "codex").unwrap()["needs_repair"],
        true
    );
}
