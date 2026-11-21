#![allow(dead_code)]
use ccpd::store::{Config, Store};
use serde_json::json;
use std::{fs, path::PathBuf};
use uuid::Uuid;

pub struct Fixture {
    pub root: PathBuf,
    pub store: Store,
}
impl Fixture {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ccpd-test-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("state")).unwrap();
        let store = Store::open(&root.join("state/router.sqlite3")).unwrap();
        Self { root, store }
    }
    pub fn add(&self, tool: &str, name: &str) -> Config {
        let home = self.root.join(format!("{tool}-{name}"));
        fs::create_dir(&home).unwrap();
        let c = self
            .store
            .register(&json!({"tool":tool,"label":name,"home":home}))
            .unwrap();
        self.store
            .edit(c.id, &json!({"revision":c.revision,"enabled":true}))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
