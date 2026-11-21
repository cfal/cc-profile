use crate::{
    invocation::{Intent, Invocation, classify, native_id},
    native::{Build, env_patch, validate_binary},
    protocol::{Plan, VERSION},
    store::{Config, Store, config, configs},
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
            "No managed mapping for this UUID. See Sessions; ccp never guesses its owner.",
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
            "No enabled config is ready. Check its directory and observer in Accounts.",
        ),
        "unsupported_build" => crate::native::unsupported_build(),
        "identity_override" => Error::bad(
            "identity_override",
            "Unset inherited authentication or storage overrides before launching.",
        ),
        "exact_id_required" => native_id("").unwrap_err(),
        _ => Error::bad(
            "unsupported_invocation",
            "Unsupported or ambiguous invocation. See ccp --help.",
        ),
    }
}
fn plan(r: &Request, c: &Config, i: &Invocation, session: Option<&str>, reason: &str) -> Plan {
    let (mut set, mut unset) = env_patch(c);
    let mut args = r.argv.clone();
    if i.intent == Intent::New {
        if r.tool == "claude" {
            args.splice(0..0, ["--session-id".into(), session.unwrap().into()]);
        } else {
            set.insert("CCP_LAUNCH_ID".into(), r.request_id.clone());
            set.insert("CCP_OBSERVE_TICKET".into(), r.ticket.clone());
            unset.retain(|k| !set.contains_key(k));
        }
    }
    if i.interactive_codex
        && !args
            .iter()
            .take_while(|s| s.as_str() != "--")
            .any(|s| s == "--no-daemon")
    {
        args.insert(0, "--no-daemon".into());
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
    workspace_ready: &dyn Fn(&Config) -> bool,
) -> Result<Selection> {
    let reject = |code| Selection::Refuse(None, denial(code, None, i.intent.id()));
    if !req.override_names.is_empty() {
        return Ok(Selection::Refuse(None, override_error(req)));
    }
    if let Err(e) = validate_binary(build, req.binary.as_deref()) {
        return Ok(Selection::Refuse(None, e));
    }
    if !build.supported && i.intent != Intent::Misc {
        return Ok(reject("unsupported_build"));
    }
    match &i.intent {
        Intent::New => {
            let all = configs(db, &req.tool)?;
            if !all.iter().any(|c| c.enabled) {
                return Ok(reject("no_enabled_config"));
            }
            let cursor: i64 =
                db.query_row("SELECT cursor FROM tools WHERE tool=?", [&req.tool], |r| {
                    r.get(0)
                })?;
            let next = all
                .into_iter()
                .filter(|c| c.enabled && build.ready(c) == "ready" && workspace_ready(c))
                .min_by_key(|c| (c.id <= cursor, c.id));
            Ok(next
                .map(|c| Selection::Allow(c, "round_robin"))
                .unwrap_or_else(|| reject("no_ready_config")))
        }
        Intent::Resume(uid) => {
            let owner = db
                .query_row(
                    "SELECT owner FROM sessions WHERE tool=? AND native_id=?",
                    params![req.tool, uid],
                    |r| r.get(0),
                )
                .optional()?;
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
}
impl Prior {
    fn find(db: &Connection, request_id: &str) -> Result<Option<Self>> {
        Ok(db.query_row(
            "SELECT digest,state,reason,native_id,config_id,revision,created_ms FROM launches WHERE request_id=?",
            [request_id], |s| Ok(Self {
                digest:s.get(0)?, state:s.get(1)?, reason:s.get(2)?, native_id:s.get(3)?,
                config:s.get(4)?, revision:s.get(5)?, created:s.get(6)?,
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
        workspace_ready: &dyn Fn(&Config) -> bool,
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
                && c.enabled
                && Some(c.revision) == self.revision
                && (i.intent != Intent::New || (build.ready(c) == "ready" && workspace_ready(c)))
        });
        if !valid
            || !matches!(self.state.as_str(), "routed" | "pending")
            || now() - self.created > 30_000
            || !build.unchanged()
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

pub fn launch(store: &Store, build: &Build, req: &Request) -> Result<Plan> {
    launch_in(store, build, req, &|_| true)
}
pub fn launch_in(
    store: &Store,
    build: &Build,
    req: &Request,
    workspace_ready: &dyn Fn(&Config) -> bool,
) -> Result<Plan> {
    check_req(req)?;
    let fingerprint =
        digest(serde_json::to_vec(req).map_err(|_| Error::bad("bad_request", "Invalid request."))?);
    let parsed = classify(&req.tool, &req.argv, &req.cwd);
    store.write(|db| {
        if let Some(prior) = Prior::find(db, &req.request_id)? {
            return Ok(prior.replay(db, build, req, &fingerprint, parsed.as_ref().map_err(Clone::clone),workspace_ready));
        }
        let invocation = parsed.as_ref().ok();
        let workspace = invocation.map(|i| &i.workspace).unwrap_or(&req.cwd).to_string_lossy();
        let intent = invocation.map(|i| i.intent.name()).unwrap_or("unsupported");
        let mut id = invocation.and_then(|i| i.intent.id()).map(str::to_owned);
        let selection = match &parsed {
            Ok(i) => choose(db, build, req, i,workspace_ready)?,
            Err(e) => Selection::Refuse(None, e.clone()),
        };
        let ms = now();
        // Refusals commit an audit row. SQL errors propagate out of the transaction.
        let (config_id, revision, state, reason, response) = match selection {
            Selection::Refuse(c, e) => (c.as_ref().map(|c| c.id), c.as_ref().map(|c| c.revision), "denied", e.1, Err(e)),
            Selection::Allow(c, reason) => {
                let i = invocation.unwrap();
                let mut state = "routed";
                match &i.intent {
                    Intent::New => {
                        if req.tool == "claude" { id = Some(assign_claude(db, &c, &workspace, ms)?); }
                        else { state = "pending"; }
                        db.execute("UPDATE tools SET cursor=? WHERE tool=?", params![c.id, req.tool])?;
                    }
                    Intent::Resume(uid) => {
                        db.execute("UPDATE sessions SET last_routed_ms=? WHERE tool=? AND native_id=?", params![ms, req.tool, uid])?;
                    }
                    Intent::Misc => {}
                }
                (Some(c.id), Some(c.revision), state, reason, Ok(plan(req, &c, i, id.as_deref(), reason)))
            }
        };
        db.execute(
            "INSERT INTO launches(request_id,tool,intent,state,reason,native_id,config_id,revision,workspace,digest,ticket_digest,created_ms)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            params![req.request_id,req.tool,intent,state,reason,id,config_id,revision,workspace,fingerprint,digest(&req.ticket),ms],
        )?;
        Ok(response)
    })?
}
#[derive(Deserialize)]
pub struct Observation {
    pub request_id: String,
    pub ticket: String,
    pub event: String,
    pub source: String,
    pub native_id: String,
}
pub fn observe(store: &Store, o: &Observation) -> Result<()> {
    let uid = native_id(&o.native_id)?;
    if o.event != "SessionStart" || o.source != "startup" {
        return Err(Error::bad("wrong_event", "Not a root startup event."));
    }
    store.write(|db|{
        let (state,old,owner,ws,created):(String,Option<String>,i64,String,i64)=db.query_row("SELECT state,native_id,config_id,workspace,created_ms FROM launches WHERE tool='codex' AND intent='new' AND request_id=? AND ticket_digest=?",params![o.request_id,digest(&o.ticket)],|s|Ok((s.get(0)?,s.get(1)?,s.get(2)?,s.get(3)?,s.get(4)?))).optional()?.ok_or_else(Error::denied)?;
        if state=="observed"&&old.as_deref()==Some(&uid){return Ok(Ok(()))}
        if state=="conflict" || state=="observed" || (state=="pending" && db.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE tool='codex' AND native_id=?)",[&uid],|r|r.get::<_,bool>(0))?){
            db.execute("UPDATE launches SET reason='observation_conflict',native_id=COALESCE(native_id,?),state=CASE WHEN state='pending' THEN 'conflict' ELSE state END WHERE request_id=?", params![uid,o.request_id])?;
            return Ok(Err(Error::conflict("observation_conflict","Conflicting session observation; existing owner preserved.")))
        }
        if state!="pending"||now()-created>86_400_000{return Ok(Err(Error::conflict("ticket_expired","Observation ticket has expired or is no longer pending.")))}
        let ms=now();insert_binding(db,"codex",&uid,owner,&ws,"observed",ms)?;
        db.execute("UPDATE launches SET native_id=?,state='observed',observed_ms=? WHERE request_id=?",params![uid,ms,o.request_id])?;Ok(Ok(()))
    })?
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
