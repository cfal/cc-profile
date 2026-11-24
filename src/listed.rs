use crate::{
    native::{Build, validate_binary},
    protocol::{CODEX_BROWSE_MAX_PROFILES, CODEX_BROWSE_MAX_THREADS, CodexListed},
    routing::codex_admission,
    saved_files::open_home,
    store::{Store, config, enabled_configs, session_binding},
    types::{Error, Result, now},
};
use rusqlite::{Connection, params};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub enum Source {
    Acp,
    Codex,
}

impl Source {
    /// Validate even filtered-out rows against immutable historical bindings.
    pub fn record(
        self,
        db: &Connection,
        owner: i64,
        id: &str,
        cwd: &str,
        publish: bool,
    ) -> Result<()> {
        let (tool, listed) = match self {
            Self::Acp => ("agy", "acp_listed"),
            Self::Codex => ("codex", "codex_listed"),
        };
        match session_binding(db, tool, id)? {
            Some((old, _)) if old != owner => {
                return Err(Error::conflict(
                    "ambiguous_session",
                    "Session conflicts with its recorded owner; no browse results were published.",
                ));
            }
            None if publish => {
                db.prepare_cached(
                    "INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms)
                    VALUES(?,?,?,?,?,?)",
                )?
                .execute(params![tool, id, owner, listed, cwd, now()])?;
            }
            Some((_, e))
                if publish && (e == "saved_file" || matches!(self, Self::Codex) && e == listed) =>
            {
                db.prepare_cached(
                    "UPDATE sessions SET evidence=?1,workspace=?2 WHERE tool=?3 AND native_id=?4
                    AND (evidence!=?1 OR workspace IS NOT ?2)",
                )?
                .execute(params![listed, cwd, tool, id])?;
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn codex(store: &Store, build: &Build, mut list: CodexListed) -> Result<usize> {
    let end = Instant::now() + Duration::from_secs(5);
    let stale = || {
        Error::conflict(
            "stale_roster",
            "Codex profiles changed. List threads again.",
        )
    };
    let invalid = || {
        Error::bad(
            "invalid_list",
            "Invalid Codex thread list; no results were published.",
        )
    };
    if list.profiles.len() > CODEX_BROWSE_MAX_PROFILES
        || list.threads.len() > CODEX_BROWSE_MAX_THREADS
    {
        return Err(Error::bad(
            "browse_limit",
            "Codex thread list exceeds the browsing limit.",
        ));
    }
    list.profiles.sort_unstable();
    if list.profiles.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid());
    }
    let mut claims = BTreeMap::new();
    for (owner, id, cwd) in &list.threads {
        let uuid = Uuid::parse_str(id).map_err(|_| invalid())?;
        if uuid.to_string() != *id || !crate::paths::workspace(cwd) {
            return Err(invalid());
        }
        if claims.insert(uuid, *owner).is_some() {
            return Err(Error::conflict(
                "ambiguous_session",
                "Duplicate Codex thread ID; no results were published.",
            ));
        }
    }

    let roster = |db: &Connection| {
        if let Some((request_id, ticket)) = &list.bound {
            let (owner, revision, _) = codex_admission(db, request_id, ticket)?;
            let c = config(db, owner)?;
            if c.tool != "codex" || !c.enabled || Some(c.revision) != revision {
                return Err(stale());
            }
            Ok(vec![c])
        } else {
            enabled_configs(db, "codex", CODEX_BROWSE_MAX_PROFILES + 1)
        }
    };
    let before = roster(&*store.db()?)?;
    if (list.bound.is_none() || !list.profiles.is_empty())
        && !before.iter().map(|c| (c.id, c.revision)).eq(list.profiles)
    {
        return Err(stale());
    }
    if list
        .threads
        .iter()
        .any(|(owner, _, _)| before.binary_search_by_key(owner, |c| c.id).is_err())
    {
        return Err(invalid());
    }
    let disk = if claims.is_empty() {
        None
    } else {
        Some(crate::rollout::listed::probe(store, &claims, end)?)
    };
    // Never stat user profile paths while holding the global database mutex.
    for c in &before {
        if !open_home(&c.home).is_ok_and(|fd| c.matches_open_home(&fd)) {
            return Err(stale());
        }
    }
    validate_binary(build, Some(&list.binary))?;
    store.write(|db| {
        if Instant::now() >= end || roster(db)? != before {
            return Err(stale());
        }
        if let Some(proof) = disk {
            proof.recheck(db, end)?;
        }
        for (owner, id, cwd) in &list.threads {
            Source::Codex.record(db, *owner, id, cwd, true)?;
        }
        if Instant::now() >= end {
            return Err(Error::unavailable(
                "browse_timeout",
                "Codex thread listing timed out.",
            ));
        }
        Ok(list.threads.len())
    })
}
