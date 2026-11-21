use crate::service::{Shared, blocking};
use ccpd::{
    invocation::{Intent, classify},
    native::{Build, stamp},
    observer,
    routing::Request,
    store::{Config, configs},
    types::Result,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinSet};

// The user hook may be trusted at home but disabled by a trusted project layer.
// Ask Codex about those workspaces; never parse a project's config or run its hooks here.
fn layers(cwd: &Path) -> (String, bool) {
    let mut key = String::new();
    let mut linked = false;
    for dir in cwd.ancestors() {
        for file in ["config.toml", "hooks.json"] {
            let path = dir.join(".codex").join(file);
            let s = stamp(&path);
            if !s.is_empty() {
                key.push_str(&format!("{}:{s};", path.display()));
            }
        }
        if dir.join(".git").is_file() {
            linked = true;
        }
    }
    (key, linked)
}
pub struct Checks {
    cache: Mutex<HashMap<(PathBuf, i64), (String, bool)>>,
    slots: Semaphore,
}
impl Checks {
    pub fn new() -> Self {
        Self {
            cache: Mutex::default(),
            slots: Semaphore::new(2),
        }
    }
}
pub struct Eligibility {
    cwd: PathBuf,
    layers: (String, bool),
    verified: HashMap<i64, String>,
}
impl Eligibility {
    pub fn ready(&self, c: &Config, build: &Build) -> bool {
        let current = layers(&self.cwd);
        current == self.layers
            && (current == (String::new(), false)
                || self.verified.get(&c.id) == Some(&build.observer_stamp(c)))
    }
}
pub async fn check(s: &Shared, r: &Request) -> Result<Option<Eligibility>> {
    if r.tool != "codex"
        || !r.override_names.is_empty()
        || r.binary.as_deref() != Some(&s.codex.bin)
        || !s.codex.supported
    {
        return Ok(None);
    }
    let Ok(i) = classify(&r.tool, &r.argv, &r.cwd) else {
        return Ok(None);
    };
    if i.intent != Intent::New {
        return Ok(None);
    }
    let cwd = i.workspace;
    let mut out = Eligibility {
        layers: layers(&cwd),
        cwd: cwd.clone(),
        verified: HashMap::new(),
    };
    let (layer_stamp, linked) = &out.layers;
    let workspace_probe = !layer_stamp.is_empty() || *linked;
    let app = s.clone();
    let homes = blocking(move || configs(&*app.store.db()?, "codex")).await?;
    let mut probes = JoinSet::new();
    for c in homes
        .into_iter()
        .filter(|c| c.enabled && c.observer.is_some())
    {
        let recheck_home = match s.codex.ready(&c) {
            "ready" => false,
            "observer_required" => true,
            _ => continue,
        };
        if !workspace_probe && !recheck_home {
            continue;
        }
        let home_stamp = s.codex.observer_stamp(&c);
        let key = (cwd.clone(), c.id);
        let sig = format!("{home_stamp}|{layer_stamp}");
        if !recheck_home
            && !linked
            && let Some((prior, ok)) = s.workspace.cache.lock().unwrap().get(&key)
            && prior == &sig
        {
            if *ok {
                out.verified.insert(c.id, home_stamp);
            }
            continue;
        }
        let a = s.clone();
        let cwd = cwd.clone();
        let cacheable = !*linked;
        probes.spawn(async move {
            let _slot = a.workspace.slots.acquire().await.ok()?;
            // Workspace trust is not proof of global trust. Recheck the home in
            // a neutral directory before evaluating any project override.
            if recheck_home {
                observer::check(&a.store, &a.codex, &c, &a.state_dir)
                    .await
                    .ok()?;
            }
            if !workspace_probe {
                return Some((c.id, home_stamp));
            }
            let valid = observer::trusted(&a.codex, &c, &a.state_dir, &cwd)
                .await
                .ok()?;
            if cacheable {
                let mut cache = a.workspace.cache.lock().unwrap();
                if cache.len() >= 512 {
                    cache.clear();
                }
                cache.insert(key, (sig, valid));
            }
            valid.then_some((c.id, home_stamp))
        });
    }
    // Leave room for durable admission inside the client's five-second budget.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(result) = probes.join_next().await {
            if let Ok(Some((id, stamp))) = result {
                out.verified.insert(id, stamp);
            }
        }
    })
    .await;
    Ok(Some(out))
}
