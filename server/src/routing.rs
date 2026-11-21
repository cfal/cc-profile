use crate::{
    invocation::{Intent, Invocation, classify, native_id},
    native::{Build, env_patch, validate_binary},
    store::{Config, Store, config, configs},
    types::{Error, Result, now, tool},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: String,
    pub ticket: String,
    pub tool: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub binary: Option<PathBuf>,
    pub override_names: Vec<String>,
}
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
fn plan(r: &Request, c: &Config, i: &Invocation, session: Option<&str>, reason: &str) -> Value {
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
    if i.interactive_codex {
        args.insert(0, "--no-daemon".into());
    }
    json!({"v":1,"request_id":r.request_id,"tool":r.tool,"config_id":c.id,"label":c.label,"decision":reason,"env_set":set,"env_unset":unset,"argv":args})
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
pub fn launch(store: &Store, build: &Build, r: &Request) -> Result<Value> {
    check_req(r)?;
    let fingerprint =
        digest(serde_json::to_vec(r).map_err(|_| Error::bad("bad_request", "Invalid request."))?);
    let parsed = classify(&r.tool, &r.argv, &r.cwd);
    store.write(|db|{
        let prior=db.query_row("SELECT digest,state,reason,native_id,config_id,revision,created_ms FROM launches WHERE request_id=?",[&r.request_id],|s|
            Ok((s.get::<_,Vec<u8>>(0)?,s.get::<_,String>(1)?,s.get::<_,String>(2)?,s.get::<_,Option<String>>(3)?,s.get::<_,Option<i64>>(4)?,s.get::<_,Option<i64>>(5)?,s.get::<_,i64>(6)?))).optional()?;
        if let Some((d,state,reason,id,cid,revision,at))=prior{
            if d!=fingerprint{return Ok(Err(Error::conflict("request_conflict","Request ID already used for different inputs.")))}
            let c=cid.map(|id|config(db,id)).transpose()?;
            if state=="denied"{return Ok(Err(denial(&reason,c.as_ref(),id.as_deref())))}
            let i=parsed.as_ref().map_err(Clone::clone)?;
            if !matches!(state.as_str(),"routed"|"pending")||now()-at>30_000||!build.unchanged()||!c.as_ref().is_some_and(|c|c.present()&&c.enabled&&Some(c.revision)==revision&&(i.intent!=Intent::New||build.ready(c)=="ready")){
                return Ok(Err(Error::conflict("stale_plan","Prior plan is no longer valid. No second config was chosen.")))
            }
            return Ok(Ok(plan(r,c.as_ref().unwrap(),i,id.as_deref(),&reason)))
        }
        let ms=now();let mut selected=None;let mut id=None;let mut intent="unsupported";
        let mut ws=r.cwd.as_path();let mut state="denied";
        let choice=(||->Result<(Value,&str)>{
            let i=parsed.as_ref().map_err(Clone::clone)?;ws=&i.workspace;intent=i.intent.name();
            id=i.intent.id().map(str::to_owned);
            if !r.override_names.is_empty(){return Err(denial("identity_override",None,None))}
            validate_binary(build,r.binary.as_deref())?;
            if !build.supported && i.intent!=Intent::Misc{return Err(crate::native::unsupported_build())}
            let reason=match &i.intent{
                Intent::New=>{
                    let all=configs(db,&r.tool)?;
                    if !all.iter().any(|c|c.enabled){return Err(denial("no_enabled_config",None,None))}
                    let last:i64=db.query_row("SELECT cursor FROM tools WHERE tool=?",[&r.tool],|r|r.get(0))?;
                    selected=all.into_iter().filter(|c|c.enabled&&build.ready(c)=="ready").min_by_key(|c|(c.id<=last,c.id));
                    let c=selected.as_ref().ok_or_else(||denial("no_ready_config",None,None))?;
                    if r.tool=="claude"{
                        loop{let uid=Uuid::new_v4().to_string();
                            if !db.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE tool='claude' AND native_id=?)",[&uid],|r|r.get::<_,bool>(0))?{
                                insert_binding(db,&r.tool,&uid,c.id,&ws.to_string_lossy(),"assigned",ms)?;id=Some(uid);break;
                            }}
                    }
                    db.execute("UPDATE tools SET cursor=? WHERE tool=?",params![c.id,r.tool])?;"round_robin"
                }
                Intent::Resume(uid)=>{
                    let owner=db.query_row("SELECT owner FROM sessions WHERE tool=? AND native_id=?",params![r.tool,uid],|r|r.get(0)).optional()?
                      .ok_or_else(||denial("unknown_session",None,Some(uid)))?;
                    selected=Some(config(db,owner)?);let c=selected.as_ref().unwrap();
                    if !c.enabled{return Err(denial("owner_disabled",Some(c),Some(uid)))}
                    if !c.present(){return Err(denial("owner_missing",Some(c),Some(uid)))}
                    db.execute("UPDATE sessions SET last_routed_ms=? WHERE tool=? AND native_id=?",params![ms,r.tool,uid])?;"sticky"
                }
                Intent::Misc=>{
                    selected=configs(db,&r.tool)?.into_iter().find(|c|c.current&&c.enabled);
                    if !selected.as_ref().is_some_and(Config::present){return Err(denial("no_current",None,None))}"current"
                }
            };
            state=if i.intent==Intent::New && r.tool=="codex"{"pending"}else{"routed"};
            Ok((plan(r,selected.as_ref().unwrap(),i,id.as_deref(),reason),reason))
        })();
        // Operational write failures must roll back cursor and owner together, not become admissions.
        if let Err(e)=&choice {if matches!(e.1,"database"|"conflict"){return Err(e.clone())}}
        let reason=choice.as_ref().map(|(_,reason)|*reason).unwrap_or_else(|e|e.1);
        db.execute("INSERT INTO launches(request_id,tool,intent,state,reason,native_id,config_id,revision,workspace,digest,ticket_digest,created_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
          params![r.request_id,r.tool,intent,state,reason,id,selected.as_ref().map(|c|c.id),selected.as_ref().map(|c|c.revision),ws.to_string_lossy(),fingerprint,digest(&r.ticket),ms])?;
        Ok(choice.map(|(v,_)|v))
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
        if state=="observed" || (state=="pending" && db.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE tool='codex' AND native_id=?)",[&uid],|r|r.get::<_,bool>(0))?){
            db.execute("UPDATE launches SET reason='observation_conflict',state=CASE WHEN state='pending' THEN 'conflict' ELSE state END WHERE request_id=?",[&o.request_id])?;
            return Ok(Err(Error::conflict("observation_conflict","Conflicting session observation; existing owner preserved.")))
        }
        if state!="pending"||now()-created>86_400_000{return Ok(Err(Error::conflict("ticket_expired","Observation ticket has expired or is no longer pending.")))}
        let ms=now();insert_binding(db,"codex",&uid,owner,&ws,"observed",ms)?;
        db.execute("UPDATE launches SET native_id=?,state='observed',observed_ms=? WHERE request_id=?",params![uid,ms,o.request_id])?;Ok(Ok(()))
    })?
}
#[derive(Deserialize)]
pub struct Failure {
    pub request_id: String,
    pub ticket: String,
    pub errno: i32,
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
