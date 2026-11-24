//! A filtered thread/list establishes a positive native owner, but cannot rule out
//! another profile's active or archived rollout. Check all listed IDs in one walk.
use super::*;
use crate::types::Error;

fn failure(status: Owner) -> Error {
    match status {
        Owner::Conflict => Error::conflict(
            "rollout_conflict",
            "A listed Codex thread has a rollout in another profile. No results were published.",
        ),
        Owner::Limit => Error::conflict(
            "rollout_limit",
            "Verifying the listed threads exceeds the rollout lookup limit.",
        ),
        Owner::Unavailable(_) => Error::conflict(
            "rollout_unavailable",
            "Cannot verify a registered Codex rollout directory.",
        ),
        _ => Error::unavailable(
            "rollout_pending",
            "Codex rollout verification was incomplete. Retry after indexing.",
        ),
    }
}

type Index = BTreeMap<Uuid, Vec<(Key, String)>>;

pub(crate) struct BatchProof {
    registry: Vec<Config>,
    index: Index,
}
impl BatchProof {
    pub(crate) fn recheck(&self, db: &Connection, end: Instant) -> Result<()> {
        if Instant::now() >= end || profiles(db)?.as_ref() != Some(&self.registry) {
            return Err(failure(Owner::Pending));
        }
        for (id, before) in &self.index {
            if Instant::now() >= end || candidates(db, *id)? != *before {
                return Err(failure(Owner::Pending));
            }
        }
        Ok(())
    }
}

pub(crate) fn probe(
    store: &Store,
    claims: &BTreeMap<Uuid, i64>,
    end: Instant,
) -> Result<BatchProof> {
    let registry = profiles(&*store.db()?)?.ok_or_else(|| failure(Owner::Limit))?;
    let sole_owner = claims
        .values()
        .next()
        .filter(|first| claims.values().all(|owner| owner == *first));
    let competitors: Vec<_> = registry
        .iter()
        .filter(|c| Some(&c.id) != sole_owner)
        .cloned()
        .collect();
    let tree = Tree::collect(&competitors, end, |_, _| {}).map_err(failure)?;
    let mut index = Index::new();
    if !tree.leaves.is_empty() {
        let mut count = 0;
        for &id in claims.keys() {
            if Instant::now() >= end {
                return Err(failure(Owner::Pending));
            }
            let paths = candidates(&*store.db()?, id)?;
            count += paths.len();
            if paths.len() > MAX_GENERATIONS || count > MAX_TREE_ENTRIES {
                return Err(failure(Owner::Limit));
            }
            index.insert(id, paths);
        }
    }
    let dirty = tree
        .dirty(store, end)?
        .ok_or_else(|| failure(Owner::Pending))?;
    check(&tree, &competitors, claims, &index, &dirty, end).map_err(failure)?;
    Ok(BatchProof { registry, index })
}

fn check(
    tree: &Tree,
    competitors: &[Config],
    claims: &BTreeMap<Uuid, i64>,
    index: &Index,
    dirty: &[Key],
    end: Instant,
) -> Check<()> {
    let mut budget = Budget {
        left: MAX_TREE_ENTRIES,
        end,
        exhausted: Owner::Limit,
    };
    let mut empty = Vec::new();
    let mut foreign = |key: &Key, fd: &OwnedFd, name: &str| {
        if file(fd, name).map_err(|_| Owner::Unavailable(key.0))? {
            return Err(Owner::Conflict);
        }
        empty.push((key.clone(), name.to_owned()));
        Ok(())
    };
    for (id, paths) in index {
        for (key, name) in paths {
            budget.take()?;
            if claims[id] != key.0
                && tree.leaves.contains_key(key)
                && dirty.binary_search(key).is_err()
            {
                if thread(name.as_bytes()) != Some(*id) {
                    return Err(Owner::Pending);
                }
                foreign(key, &tree.open(key)?, name)?;
            }
        }
    }
    for key in dirty {
        budget.take()?;
        let fd = tree.open(key)?;
        let unavailable = |_| Owner::Unavailable(key.0);
        for entry in entries(&fd).map_err(unavailable)? {
            budget.take()?;
            let entry = entry.map_err(|_| Owner::Unavailable(key.0))?;
            let raw = entry.file_name();
            if let Some(id) = thread(raw.to_bytes())
                && claims.get(&id).is_some_and(|owner| *owner != key.0)
            {
                foreign(key, &fd, raw.to_str().unwrap())?;
            }
        }
        if stamp(&fd).ok().as_ref() != tree.leaves.get(key) {
            return Err(Owner::Pending);
        }
    }
    tree.recheck(competitors, end)?;
    // Filling an indexed empty file does not change its parent directory stamp.
    for (key, name) in empty {
        budget.take()?;
        if file(&tree.open(&key)?, &name).map_err(|_| Owner::Unavailable(key.0))? {
            return Err(Owner::Conflict);
        }
    }
    Ok(())
}
