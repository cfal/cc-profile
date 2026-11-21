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
use tokio::{
    sync::{Semaphore, watch},
    task::JoinSet,
};

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
    running: Mutex<HashMap<ProbeKey, watch::Receiver<Option<bool>>>>,
    slots: Semaphore,
}
type ProbeKey = (PathBuf, i64, String);
impl Checks {
    pub fn new() -> Self {
        Self {
            cache: Mutex::default(),
            running: Mutex::default(),
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
    pub fn ready(&self, c: &Config, build: &Build, workspace: &Path) -> bool {
        let current = layers(&self.cwd);
        workspace == self.cwd
            && current == self.layers
            && (current == (String::new(), false)
                || self.verified.get(&c.id) == Some(&build.observer_stamp(c)))
    }
}
pub async fn check(s: &Shared, r: &Request) -> Result<Option<Eligibility>> {
    if r.tool != "codex"
        || r.login_profile.is_some()
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
        let sig = format!("{home_stamp}|{}|{layer_stamp}", c.revision);
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
        let flight = (cwd.clone(), c.id, sig.clone());
        let mut running = s.workspace.running.lock().unwrap();
        let mut wait = if let Some(rx) = running.get(&flight) {
            rx.clone()
        } else {
            if running.len() >= 512 {
                continue;
            }
            let (tx, rx) = watch::channel(None);
            running.insert(flight.clone(), rx.clone());
            let a = s.clone();
            let cwd = cwd.clone();
            let c = c.clone();
            let cacheable = !*linked;
            tokio::spawn(async move {
                let result = async {
                    let _slot = a.workspace.slots.acquire().await.ok()?;
                    // Workspace trust is not proof of global trust. Recheck the home in
                    // a neutral directory before evaluating any project override.
                    if recheck_home {
                        observer::check(&a.store, &a.codex, &c, Path::new("/"))
                            .await
                            .ok()?;
                    }
                    if !workspace_probe {
                        return Some(true);
                    }
                    let valid = observer::trusted(&a.codex, &c, Path::new("/"), &cwd)
                        .await
                        .ok()?;
                    if cacheable {
                        let mut cache = a.workspace.cache.lock().unwrap();
                        if cache.len() >= 512 {
                            cache.clear();
                        }
                        cache.insert(key, (sig, valid));
                    }
                    Some(valid)
                }
                .await;
                tx.send_replace(Some(result.unwrap_or(false)));
                a.workspace.running.lock().unwrap().remove(&flight);
            });
            rx
        };
        drop(running);
        probes.spawn(async move {
            let valid = *wait.wait_for(Option::is_some).await.ok()?;
            valid.unwrap_or(false).then_some((c.id, home_stamp))
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
