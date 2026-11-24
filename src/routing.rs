use crate::{
    invocation::{Intent, Invocation, classify, native_id},
    native::{Build, env_patch, validate_binary},
    protocol::{Plan, RoutingMode, VERSION, profile_login_args},
    quota::{WindowExpiry::Unknown, cached_allowance},
    store::{Config, Store, config, configs, routing_mode},
    types::{Error, Result, now, tool},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub use crate::protocol::{Failure, Request};
fn digest(v: impl AsRef<[u8]>) -> Vec<u8> {
    Sha256::digest(v.as_ref()).to_vec()
}
fn check_req(r: &Request) -> Result<()> {
    tool(&r.tool)?;
    if let Some(id) = r.login_profile
        && (id <= 0
            || !(r.tool == "agy" && r.argv == crate::agy::PROFILE_USAGE)
                && ![false, true].into_iter().any(|status| {
                    profile_login_args(&r.tool, status)
                        .is_some_and(|args| r.argv.as_slice() == args)
                }))
    {
        return Err(Error::bad(
            "bad_request",
            "Unsupported profile setup command.",
        ));
    }
    if Uuid::parse_str(&r.request_id).is_err()
        || r.ticket.len() != 64
        || !r.ticket.bytes().all(|b| b.is_ascii_hexdigit())
        || !r.cwd.is_absolute()
        || r.cwd.to_str().is_none_or(|s| s.contains('\0'))
        || r.argv.len() > 4096
        || r.argv.iter().any(|s| s.contains('\0'))
        || r.override_names.len() > 64
        || r.override_names.iter().any(|s| {
            s.is_empty()
                || s.len() > 80
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
    {
        return Err(Error::bad("bad_request", "Invalid launch envelope."));
    }
    Ok(())
}
fn denial(code: &str, c: Option<&Config>, id: Option<&str>) -> Error {
    match code {
        "unknown_session" => Error::missing(
            "unknown_session",
            "No managed mapping for this session ID. See Sessions; ccp never guesses its owner.",
        ),
        "conversation_missing" => Error::missing(
            "conversation_missing",
            "No registered Antigravity profile has this saved conversation.",
        ),
        "conversation_pruned" => Error::missing(
            "conversation_pruned",
            format!(
                "The saved conversation is missing or was pruned from its recorded profile ({}). No other profile was selected.",
                c.map_or("unknown", |c| &c.label)
            ),
        ),
        "conversation_ambiguous" => Error::conflict(
            "conversation_ambiguous",
            "Conflicting Antigravity conversation owners. No profile was selected.",
        ),
        "rollout_pruned" => Error::missing(
            "rollout_pruned",
            "The saved Codex thread is missing from its recorded profile. No other profile was selected.",
        ),
        "rollout_conflict" => Error::conflict(
            "rollout_conflict",
            "Conflicting Codex thread owners. No profile was selected.",
        ),
        "rollout_pending" => Error::unavailable(
            "rollout_pending",
            "Codex could not verify this thread within the lookup budget. Retry after indexing; no other profile was selected.",
        ),
        "rollout_limit" => Error::conflict(
            "rollout_limit",
            "The registered Codex history exceeds the supported discovery limits. No profile was selected.",
        ),
        "rollout_unavailable" => Error::conflict(
            "rollout_unavailable",
            "Cannot verify the saved threads in a registered Codex profile. No owner was assumed.",
        ),
        "unsafe_conversation" => Error::bad(
            "unsafe_conversation",
            "Cannot verify the native conversation path.",
        ),
        "owner_disabled" | "owner_missing" => {
            let name = c
                .map(|c| format!("{}/{}", c.tool, c.label))
                .unwrap_or_default();
            if code == "owner_disabled" {
                Error::conflict(
                    "owner_disabled",
                    format!(
                        "{name} owns {} and is disabled. Re-enable it in Accounts to resume.",
                        id.unwrap_or("")
                    ),
                )
            } else {
                Error::conflict(
                    "owner_missing",
                    format!(
                        "{name}'s config directory is missing or replaced. Restore it before resuming."
                    ),
                )
            }
        }
        "no_current" => Error::conflict("no_current", "Set current for this tool in Accounts."),
        "login_profile" => Error::conflict(
            "login_profile",
            "This profile is missing, replaced, or belongs to another provider.",
        ),
        "current_missing" => Error::conflict(
            "current_missing",
            "The Current config directory is missing or replaced. Restore it or set another Current in Accounts.",
        ),
        "no_enabled_config" => Error::conflict(
            "no_enabled_config",
            "Enable a config for this tool in Accounts.",
        ),
        "no_ready_config" => Error::conflict(
            "no_ready_config",
            "No enabled config is ready. Check the profile directories in Accounts.",
        ),
        "quota_exhausted" => Error::conflict(
            "quota_exhausted",
            "All eligible accounts have exhausted their account-wide quota. Wait for a reset or refresh quotas in the Dashboard.",
        ),
        "unsupported_build" => crate::native::unsupported_build(),
        "identity_override" => Error::bad(
            "identity_override",
            "Unset inherited authentication or storage overrides before launching.",
        ),
        "exact_id_required" => native_id("").unwrap_err(),
        "git_required" => crate::invocation::git_required(),
        "bad_cwd" => Error::bad("bad_cwd", "Working directory is unavailable."),
        _ => Error::bad(
            "unsupported_invocation",
            "Unsupported or ambiguous invocation. See ccp --help.",
        ),
    }
}
fn plan(r: &Request, c: &Config, i: &Invocation, session: Option<&str>, reason: &str) -> Plan {
    let (set, unset) = env_patch(c);
    let mut args = i.intent.argv(&r.argv).to_vec();
    if i.intent == Intent::New && r.tool == "claude" {
        args.splice(0..0, ["--session-id".into(), session.unwrap().into()]);
    }
    Plan {
        v: VERSION,
        request_id: r.request_id.clone(),
        tool: r.tool.clone(),
        config_id: c.id,
        label: c.label.clone(),
        decision: reason.into(),
        env_set: set,
        env_unset: unset,
        argv: args,
    }
}
fn insert_binding(
    db: &Connection,
    t: &str,
    id: &str,
    c: i64,
    ws: &str,
    e: &str,
    ms: i64,
) -> Result<()> {
    db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,bound_ms,last_routed_ms) VALUES(?,?,?,?,?,?,?)",params![t,id,c,e,ws,ms,ms])?;
    Ok(())
}
enum Selection {
    Allow(Config, &'static str),
    Refuse(Option<Config>, Error),
}

fn choose(
    db: &Connection,
    build: &Build,
    req: &Request,
    i: &Invocation,
    mode: RoutingMode,
    rollout: Option<&crate::rollout::Proof>,
) -> Result<Selection> {
    let reject = |code| Selection::Refuse(None, denial(code, None, i.intent.id()));
    if !req.override_names.is_empty() {
        return Ok(Selection::Refuse(None, override_error(req)));
    }
    if let Err(e) = validate_binary(build, req.binary.as_deref()) {
        return Ok(Selection::Refuse(None, e));
    }
    if let Some(id) = req.login_profile {
        return Ok(
            match configs(db, &req.tool)?
                .into_iter()
                .find(|c| c.id == id && c.present())
            {
                Some(c) if i.intent == Intent::Misc => Selection::Allow(c, "profile_login"),
                _ => reject("login_profile"),
            },
        );
    }
    match &i.intent {
        Intent::New | Intent::Print(_) => {
            let all = configs(db, &req.tool)?;
            if !all.iter().any(|c| c.enabled) {
                return Ok(reject("no_enabled_config"));
            }
            let cursor: i64 =
                db.query_row("SELECT cursor FROM tools WHERE tool=?", [&req.tool], |r| {
                    r.get(0)
                })?;
            let ms = now();
            let check_quota =
                req.tool != "codex" || crate::quota::no_codex_project_config(&i.workspace);
            let mut exhausted = false;
            let next = all
                .into_iter()
                .filter(|c| c.enabled && c.present())
                .filter_map(|c| {
                    let allowance = if check_quota {
                        cached_allowance(db, &c, ms)
                    } else {
                        None
                    };
                    let remaining = allowance.map(|q| q.remaining);
                    if remaining == Some(0.) {
                        exhausted = true;
                        return None;
                    }
                    // Lower sorts first. Unknown is behind any measured nonzero score.
                    let (rank, reset) = match mode {
                        RoutingMode::RoundRobin => (0., Unknown),
                        RoutingMode::Priority => (-(c.priority as f64), Unknown),
                        RoutingMode::MostRemaining => {
                            (remaining.map(|r| -r).unwrap_or(f64::INFINITY), Unknown)
                        }
                        RoutingMode::LeastRemaining => {
                            (remaining.unwrap_or(f64::INFINITY), Unknown)
                        }
                        RoutingMode::EarliestExpiry => {
                            (0., allowance.map_or(Unknown, |q| q.expiry))
                        }
                    };
                    Some((c, rank, reset))
                })
                .min_by(|(a, x, ta), (b, y, tb)| {
                    x.total_cmp(y)
                        .then_with(|| ta.cmp(tb))
                        .then_with(|| (a.id <= cursor, a.id).cmp(&(b.id <= cursor, b.id)))
                });
            Ok(match next {
                Some((c, _, _)) => Selection::Allow(c, mode.name()),
                None if exhausted => reject("quota_exhausted"),
                None => reject("no_ready_config"),
            })
        }
        Intent::Resume(uid) => {
            let binding: Option<(i64, String)> = db
                .query_row(
                    "SELECT owner,evidence FROM sessions WHERE tool=? AND native_id=?",
                    params![req.tool, uid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let owner = binding.as_ref().map(|(id, _)| *id);
            let owner = if i.agy_tui {
                let uuid =
                    Uuid::parse_str(uid).map_err(|_| denial("exact_id_required", None, None))?;
                let mut found = None;
                for c in configs(db, "agy")?
                    .into_iter()
                    .filter(Config::directory_matches)
                {
                    match crate::agy::has_conversation(&c.home, uuid) {
                        Ok(true) if found.is_some() || owner.is_some_and(|id| id != c.id) => {
                            return Ok(reject("conversation_ambiguous"));
                        }
                        Ok(true) => found = Some(c.id),
                        Ok(false) => {}
                        Err(_) => return Ok(reject("unsafe_conversation")),
                    }
                }
                if found.is_none() {
                    if let Some(owner) = owner {
                        let c = config(db, owner)?;
                        let err = denial("conversation_pruned", Some(&c), None);
                        return Ok(Selection::Refuse(Some(c), err));
                    }
                    return Ok(reject("conversation_missing"));
                }
                found
            } else if req.tool == "codex" {
                use crate::rollout::Owner;
                let Some(proof) = rollout else {
                    return Ok(reject("rollout_pending"));
                };
                match proof.resolve(db)? {
                    Owner::Unique(id) if owner.is_none_or(|old| old == id) => Some(id),
                    Owner::Unique(_) | Owner::Conflict => return Ok(reject("rollout_conflict")),
                    Owner::Pending => return Ok(reject("rollout_pending")),
                    Owner::Limit => return Ok(reject("rollout_limit")),
                    Owner::Unavailable(id) => {
                        let c = config(db, id)?;
                        let code = if Some(id) == owner && !c.present() {
                            "owner_missing"
                        } else {
                            "rollout_unavailable"
                        };
                        let err = denial(code, Some(&c), Some(uid));
                        return Ok(Selection::Refuse(Some(c), err));
                    }
                    Owner::Missing => match binding {
                        Some((id, evidence)) if evidence == "saved_file" => {
                            let c = config(db, id)?;
                            let code = if !c.present() {
                                "owner_missing"
                            } else {
                                "rollout_pruned"
                            };
                            let err = denial(code, Some(&c), Some(uid));
                            return Ok(Selection::Refuse(Some(c), err));
                        }
                        _ => owner, // Existing protocol evidence survives file-layout changes.
                    },
                }
            } else if binding.is_some_and(|(_, e)| e == "saved_file") {
                None // AgY ACP requires an actual protocol observation.
            } else {
                owner
            };
            let Some(owner) = owner else {
                return Ok(reject("unknown_session"));
            };
            let c = config(db, owner)?;
            let problem = if !c.enabled {
                Some("owner_disabled")
            } else if !c.present() {
                Some("owner_missing")
            } else {
                None
            };
            if let Some(code) = problem {
                let error = denial(code, Some(&c), Some(uid));
                return Ok(Selection::Refuse(Some(c), error));
            }
            Ok(Selection::Allow(c, "sticky"))
        }
        Intent::Misc => Ok(
            match configs(db, &req.tool)?
                .into_iter()
                .find(|c| c.current && c.enabled)
            {
                Some(c) if c.present() => Selection::Allow(c, "current"),
                Some(c) => Selection::Refuse(Some(c), denial("current_missing", None, None)),
                None => reject("no_current"),
            },
        ),
    }
}

fn override_error(req: &Request) -> Error {
    Error::bad(
        "identity_override",
        format!(
            "Unset inherited authentication or storage overrides: {}.",
            req.override_names.join(", ")
        ),
    )
}

fn assign_claude(db: &Connection, c: &Config, workspace: &str, ms: i64) -> Result<String> {
    loop {
        let id = Uuid::new_v4().to_string();
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE tool='claude' AND native_id=?)",
            [&id],
            |r| r.get(0),
        )?;
        if !exists {
            insert_binding(db, "claude", &id, c.id, workspace, "assigned", ms)?;
            return Ok(id);
        }
    }
}

struct Prior {
    digest: Vec<u8>,
    state: String,
    reason: String,
    native_id: Option<String>,
    config: Option<i64>,
    revision: Option<i64>,
    created: i64,
    workspace: std::path::PathBuf,
}
impl Prior {
    fn find(db: &Connection, request_id: &str) -> Result<Option<Self>> {
        Ok(db.query_row(
            "SELECT digest,state,reason,native_id,config_id,revision,created_ms,workspace FROM launches WHERE request_id=?",
            [request_id], |s| Ok(Self {
                digest:s.get(0)?, state:s.get(1)?, reason:s.get(2)?, native_id:s.get(3)?,
                config:s.get(4)?, revision:s.get(5)?, created:s.get(6)?, workspace:s.get::<_,String>(7)?.into(),
            })
        ).optional()?)
    }
    fn replay(
        self,
        db: &Connection,
        build: &Build,
        req: &Request,
        fingerprint: &[u8],
        parsed: Result<&Invocation>,
        rollout: Option<&crate::rollout::Proof>,
    ) -> Result<Plan> {
        if self.digest != fingerprint {
            return Err(Error::conflict(
                "request_conflict",
                "Request ID already used for different inputs.",
            ));
        }
        let c = self.config.map(|id| config(db, id)).transpose()?;
        if self.state == "denied" {
            if self.reason == "identity_override" {
                return Err(override_error(req));
            }
            return Err(denial(&self.reason, c.as_ref(), self.native_id.as_deref()));
        }
        let i = parsed?;
        let valid = c.as_ref().is_some_and(|c| {
            c.present()
                && (c.enabled || req.login_profile == Some(c.id))
                && Some(c.revision) == self.revision
        });
        if !valid
            || self.state != "routed"
            || now() - self.created > 30_000
            || validate_binary(build, req.binary.as_deref()).is_err()
            || self.workspace != i.workspace
            || ((i.agy_conversation().is_some()
                || req.tool == "codex" && matches!(i.intent, Intent::Resume(_)))
                && !matches!(choose(db, build, req, i, RoutingMode::RoundRobin, rollout)?,
                    Selection::Allow(owner, _) if Some(owner.id) == self.config))
        {
            return Err(Error::conflict(
                "stale_plan",
                "Prior plan is no longer valid. No second config was chosen.",
            ));
        }
        Ok(plan(
            req,
            c.as_ref().unwrap(),
            i,
            self.native_id.as_deref(),
            &self.reason,
        ))
    }
}

pub struct Admission {
    pub plan: Result<Plan>,
    pub refresh_quota: bool,
}
pub fn launch(store: &Store, build: &Build, req: &Request) -> Result<Plan> {
    admit(store, build, req)?.plan
}
pub fn admit(store: &Store, build: &Build, req: &Request) -> Result<Admission> {
    check_req(req)?;
    let fingerprint =
        digest(serde_json::to_vec(req).map_err(|_| Error::bad("bad_request", "Invalid request."))?);
    let parsed = if req.login_profile.is_some() {
        Ok(Invocation {
            intent: Intent::Misc,
            agy_tui: false,
            agy_text_stdin: false,
            workspace: req.cwd.clone(),
        })
    } else {
        classify(&req.tool, &req.argv, &req.cwd)
    };
    // A slow profile filesystem must not hold the router's write transaction.
    let rollout = if req.tool == "codex"
        && req.override_names.is_empty()
        && let Ok(i) = &parsed
        && let Intent::Resume(id) = &i.intent
        && validate_binary(build, req.binary.as_deref()).is_ok()
    {
        let id = Uuid::parse_str(id).map_err(|_| denial("exact_id_required", None, None))?;
        Some(crate::rollout::probe(store, id)?)
    } else {
        None
    };
    store.write(|db| {
        if let Some(prior) = Prior::find(db, &req.request_id)? {
            return Ok(Admission {
                plan: prior.replay(db, build, req, &fingerprint, parsed.as_ref().map_err(Clone::clone), rollout.as_ref()),
                refresh_quota: false,
            });
        }
        let invocation = parsed.as_ref().ok();
        let workspace = invocation.map(|i| &i.workspace).unwrap_or(&req.cwd).to_string_lossy();
        let intent = invocation.map(|i| match (req.tool.as_str(), &i.intent) {
            ("agy", Intent::New) if i.agy_tui => "tui",
            ("agy", Intent::Resume(_)) if i.agy_tui => "tui_resume",
            ("agy", Intent::New | Intent::Resume(_)) => "acp",
            ("codex", Intent::New | Intent::Resume(_)) if req.argv.first().is_some_and(|s| s == "app-server") => "app_server",
            _ => i.intent.name(),
        }).unwrap_or("unsupported");
        let mut id = invocation.and_then(|i| i.intent.id()).map(str::to_owned);
        let mode = if invocation.is_some_and(|i| i.intent.rotates()) {
            routing_mode(db)?
        } else {
            RoutingMode::RoundRobin
        };
        let selection = match &parsed {
            Ok(i) => choose(db, build, req, i, mode, rollout.as_ref())?,
            Err(e) => Selection::Refuse(None, e.clone()),
        };
        let ms = now();
        // Refusals commit an audit row. SQL errors propagate out of the transaction.
        let (config_id, revision, state, reason, response) = match selection {
            Selection::Refuse(c, e) => (c.as_ref().map(|c| c.id), c.as_ref().map(|c| c.revision), "denied", e.1, Err(e)),
            Selection::Allow(c, reason) => {
                let i = invocation.unwrap();
                match &i.intent {
                    Intent::New => {
                        if req.tool == "claude" { id = Some(assign_claude(db, &c, &workspace, ms)?); }
                    }
                    Intent::Print(_) => {}
                    Intent::Resume(uid) => {
                        if i.agy_tui {
                            db.execute("INSERT INTO sessions(tool,native_id,owner,evidence,workspace,tui_workspace,bound_ms,last_routed_ms,saved_file_seen_ms)
                                VALUES('agy',?1,?2,'saved_file',?3,?3,?4,?4,?4)
                                ON CONFLICT(tool,native_id) DO UPDATE SET last_routed_ms=excluded.last_routed_ms,tui_workspace=excluded.tui_workspace,
                                workspace=coalesce(sessions.workspace,excluded.workspace),saved_file_seen_ms=coalesce(sessions.saved_file_seen_ms,excluded.saved_file_seen_ms)
                                WHERE owner=excluded.owner", params![uid,c.id,workspace,ms])?;
                        } else {
                            db.execute("UPDATE sessions SET last_routed_ms=? WHERE tool=? AND native_id=?", params![ms, req.tool, uid])?;
                        }
                    }
                    Intent::Misc => {}
                }
                if i.intent.rotates() {
                    db.execute("UPDATE tools SET cursor=? WHERE tool=?", params![c.id, req.tool])?;
                }
                (Some(c.id), Some(c.revision), "routed", reason, Ok(plan(req, &c, i, id.as_deref(), reason)))
            }
        };
        db.execute(
            "INSERT INTO launches(request_id,tool,intent,state,reason,native_id,config_id,revision,workspace,digest,ticket_digest,created_ms)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            params![req.request_id,req.tool,intent,state,reason,id,config_id,revision,workspace,fingerprint,digest(&req.ticket),ms],
        )?;
        Ok(Admission {
            refresh_quota: req.tool == "codex" && mode.uses_remaining()
                && (response.is_ok() || reason == "quota_exhausted"),
            plan: response,
        })
    })
}
#[derive(Deserialize)]
pub struct Observation {
    pub request_id: String,
    pub ticket: String,
    pub event: String,
    pub source: String,
    pub native_id: String,
    pub workspace: Option<String>,
}
pub fn acp_owner(db: &Connection, request_id: &str, ticket: &str) -> Result<i64> {
    db.query_row(
        "SELECT config_id FROM launches WHERE tool='agy' AND intent='acp' AND state='routed'
             AND request_id=? AND ticket_digest=?",
        params![request_id, digest(ticket)],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(Error::denied)
}
fn acp_event(store: &Store, o: &Observation) -> Result<()> {
    store.write(|db| {
        let owner = acp_owner(db, &o.request_id, &o.ticket)?;
        if o.event == "admit_new" {
            let c = config(db, owner)?;
            return if c.enabled && c.present() {
                Ok(())
            } else {
                Err(Error::denied())
            };
        }
        let id = crate::agy::session_id(&o.native_id)?;
        let existing: Option<(i64, String)> = db
            .query_row(
                "SELECT owner,evidence FROM sessions WHERE tool='agy' AND native_id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match o.event.as_str() {
            "session/new" => {
                let ws = o
                    .workspace
                    .as_deref()
                    .filter(|w| {
                        std::path::Path::new(w).is_absolute()
                            && w.len() <= 4096
                            && !w.chars().any(char::is_control)
                    })
                    .ok_or_else(|| Error::bad("bad_cwd", "Expected an absolute ACP workspace."))?;
                match existing {
                    None => insert_binding(db, "agy", id, owner, ws, "observed", now()),
                    Some((n, evidence)) if n == owner => {
                        if evidence == "saved_file" || evidence == "acp_listed" {
                            db.execute("UPDATE sessions SET evidence='observed',workspace=?,last_routed_ms=? WHERE tool='agy' AND native_id=?",
                                params![ws,now(),id])?;
                        }
                        Ok(())
                    }
                    _ => Err(Error::conflict(
                        "observation_conflict",
                        "Session already belongs to another profile.",
                    )),
                }
            }
            "session/load" | "session/resume" => {
                let c = config(db, owner)?;
                if !existing.is_some_and(|(n,e)| n == owner && matches!(e.as_str(),"observed"|"acp_listed")) || !c.enabled || !c.present() {
                    return Err(Error::denied());
                }
                db.execute(
                    "UPDATE sessions SET last_routed_ms=? WHERE tool='agy' AND native_id=?",
                    params![now(), id],
                )?;
                Ok(())
            }
            _ => Err(Error::bad("wrong_event", "Unsupported ACP operation.")),
        }
    })
}
pub fn observe(store: &Store, o: &Observation) -> Result<()> {
    if o.source == "acp" {
        return acp_event(store, o);
    }
    let uid = native_id(&o.native_id)?;
    if o.event != "thread/start" || o.source != "app-server" {
        return Err(Error::bad(
            "wrong_event",
            "Not a managed app-server thread/start reply.",
        ));
    }
    if o.workspace.as_ref().is_some_and(|s| {
        !std::path::Path::new(s).is_absolute() || s.len() > 4096 || s.chars().any(char::is_control)
    }) {
        return Err(Error::bad("bad_cwd", "Invalid thread workspace."));
    }
    store.write(|db| {
        let (owner, ws): (i64, String) = db
            .query_row(
                "SELECT config_id,workspace FROM launches WHERE tool='codex' AND state='routed'
             AND intent='app_server' AND request_id=? AND ticket_digest=?",
                params![o.request_id, digest(&o.ticket)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(Error::denied)?;
        let ws = o.workspace.as_deref().unwrap_or(&ws);
        let existing: Option<(i64, String)> = db
            .query_row(
                "SELECT owner,evidence FROM sessions WHERE tool='codex' AND native_id=?",
                [&uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match existing {
            None => insert_binding(db, "codex", &uid, owner, ws, "observed", now()),
            Some((id, _)) if id != owner => Err(Error::conflict(
                "observation_conflict",
                "Thread already belongs to a different profile.",
            )),
            Some((_, e)) if e == "saved_file" => {
                db.execute(
                    "UPDATE sessions SET evidence='observed',workspace=?,last_routed_ms=?
                            WHERE tool='codex' AND native_id=?",
                    params![ws, now(), uid],
                )?;
                Ok(())
            }
            _ => Ok(()),
        }
    })
}
pub fn exec_failure(store: &Store, x: &Failure) -> Result<()> {
    let reason = match x.errno {
        libc::ENOENT => "exec_not_found",
        libc::EACCES => "exec_permission",
        _ => "exec_failed",
    };
    store.write(|db|{
        if db.execute("UPDATE launches SET state='exec_failed',reason=? WHERE request_id=? AND ticket_digest=? AND state IN('pending','routed','exec_failed')",params![reason,x.request_id,digest(&x.ticket)])?==0{return Err(Error::denied())}Ok(())
    })
}
