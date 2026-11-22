use super::{quotas, service::Shared};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};
use ccpd::{
    native::unsupported_build,
    quota::inputs,
    rpc::Codex,
    store::{Config, config},
    types::{Error, Result, now},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};
use tokio::time::timeout;
use uuid::Uuid;

const RPC_TIMEOUT: Duration = Duration::from_secs(15);

fn stale_config() -> Error {
    Error::conflict(
        "stale_config",
        "Could not verify the same Codex profile and signed-in account. List resets again.",
    )
}
fn stale_grant() -> Error {
    Error::conflict(
        "stale_grant",
        "This reset is no longer available as confirmed. List resets again.",
    )
}
fn unknown() -> Error {
    Error::unavailable(
        "outcome_unknown",
        "Codex did not confirm the outcome. Check the account before retrying this reset.",
    )
}
fn busy() -> Error {
    Error::conflict("busy", "Another account operation is in progress.")
}
fn digest(fields: Value) -> String {
    format!("{:x}", Sha256::digest(fields.to_string()))
}
fn identifier(v: &Value) -> Option<&str> {
    v.as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
}
fn spent(db: &Connection, credit: &str) -> Result<bool> {
    Ok(db
        .prepare(
            "SELECT 1 FROM reset_grants WHERE credit=? AND outcome IN ('reset','alreadyRedeemed')",
        )?
        .exists([credit])?)
}
fn profile(s: &Shared, db: &Connection, id: i64) -> Result<(Config, String)> {
    let c = config(db, id)?;
    if c.tool != "codex" || !c.present() {
        return Err(stale_config());
    }
    if !s.codex.supported || !s.codex.unchanged() {
        return Err(unsupported_build());
    }
    let stamp = digest(json!([
        id,
        c.revision,
        inputs(&c).ok_or_else(stale_config)?,
        s.codex.fingerprint
    ]));
    Ok((c, stamp))
}
struct Grant {
    id: String,
    credit: String,
    details: String,
    title: String,
    expires: Option<i64>,
}
struct Listing {
    account: String,
    count: Option<i64>,
    detailed: bool,
    grants: Vec<Grant>,
}
impl Listing {
    fn read(usage: &Value, login: &Value) -> Result<Self> {
        let account = identifier(&usage["accountId"]).ok_or_else(stale_config)?;
        let user = identifier(&login["userId"]).or_else(|| identifier(&login["account"]["email"]));
        if login["account"]["type"] != "chatgpt"
            || user.is_none()
            || (!login["workspaceRouting"].is_null()
                && login["workspaceRouting"]["chatgptAccountId"] != account)
        {
            return Err(stale_config());
        }
        let summary = &usage["rateLimitResetCredits"];
        let count = summary["availableCount"].as_i64().filter(|v| *v >= 0);
        let rows = summary["credits"].as_array();
        let mut grants = Vec::new();
        if let Some(rows) = rows.filter(|_| count.is_some_and(|n| n > 0)) {
            // An ambiguous provider ID is never a selectable credit, even if only one row looks usable.
            let mut seen = HashSet::new();
            let dupes: HashSet<_> = rows
                .iter()
                .filter_map(|v| identifier(&v["id"]))
                .filter(|id| !seen.insert(*id))
                .collect();
            for row in rows {
                let Some(id) = identifier(&row["id"]).filter(|id| !dupes.contains(id)) else {
                    continue;
                };
                if row["resetType"] != "codexRateLimits"
                    || row["status"] != "available"
                    || !row["grantedAt"]
                        .as_i64()
                        .is_some_and(|t| (0..=now() / 1000).contains(&t))
                {
                    continue;
                }
                let expires = match row.get("expiresAt") {
                    Some(Value::Null) => None,
                    Some(v)
                        if let Some(ms) = v
                            .as_i64()
                            .and_then(|t| t.checked_mul(1000))
                            .filter(|ms| *ms > now()) =>
                    {
                        Some(ms)
                    }
                    _ => continue,
                };
                let title = row["title"]
                    .as_str()
                    .filter(|s| {
                        !s.trim().is_empty()
                            && s.len() <= 256
                            && !s.chars().any(char::is_control)
                            && !s.contains(account)
                            && !s.contains(id)
                    })
                    .unwrap_or("Codex rate limit reset");
                grants.push(Grant {
                    id: id.to_owned(),
                    credit: digest(json!([account, id])),
                    details: digest(json!([
                        row["grantedAt"],
                        expires,
                        row["title"],
                        row["description"]
                    ])),
                    title: title.to_owned(),
                    expires,
                });
                if grants.len() >= 100 {
                    break;
                }
            }
        }
        Ok(Self {
            account: digest(json!([account, user])),
            count,
            detailed: rows.is_some(),
            grants,
        })
    }
}

async fn inspect(s: &Shared, c: &Config) -> Result<(Codex, Listing)> {
    timeout(RPC_TIMEOUT, async {
        let mut rpc = Codex::open(&s.codex, c, &s.state_dir).await?;
        let usage = rpc
            .call(
                2,
                "account/rateLimits/read",
                json!({"excludeResetCreditDetails":false}),
            )
            .await?;
        let login = rpc
            .call(3, "account/read", json!({"refreshToken":false}))
            .await?;
        Ok((rpc, Listing::read(&usage, &login)?))
    })
    .await
    .unwrap_or_else(|_| {
        Err(Error::unavailable(
            "native_rpc",
            "Timed out checking Codex reset grants.",
        ))
    })
}

type Body = std::result::Result<Json<Value>, JsonRejection>;

pub async fn list(State(s): State<Shared>, Path(id): Path<i64>, body: Body) -> Result<Json<Value>> {
    if !body.is_ok_and(|v| v.as_object().is_some_and(|m| m.is_empty())) {
        return Err(Error::bad("bad_request", "Expected an empty JSON object."));
    }
    let _slot = s.identity_slots.try_acquire().map_err(|_| busy())?;
    let (c, stamp) = profile(&s, &*s.store.db()?, id)?;
    let (rpc, listing) = inspect(&s, &c).await?;
    rpc.close().await;
    s.store.write(|db| {
        if profile(&s, db, id)?.1 != stamp { return Err(stale_config()); }
        let mut views = Vec::new();
        for grant in listing.grants {
            if spent(db, &grant.credit)? {
                continue;
            }
            let handle: String = db.query_row(
                "SELECT handle FROM reset_grants WHERE config_id=? AND profile=? AND account=? AND credit=? AND details=? AND outcome IS NULL",
                params![id, stamp, listing.account, grant.credit, grant.details], |r| r.get(0)
            ).optional()?.unwrap_or_else(|| Uuid::new_v4().to_string());
            db.execute("INSERT INTO reset_grants(handle,config_id,profile,account,credit,details,seen_ms) VALUES(?,?,?,?,?,?,?)
                ON CONFLICT(handle) DO UPDATE SET seen_ms=excluded.seen_ms",
                params![handle,id,stamp,listing.account,grant.credit,grant.details,now()])?;
            views.push(json!({"handle":handle,"title":grant.title,"expires_at_ms":grant.expires}));
        }
        Ok(Json(json!({"available_count":listing.count,"details_available":listing.detailed,"grants":views})))
    })
}

struct Offer {
    profile: String,
    account: String,
    credit: String,
    details: String,
    outcome: Option<String>,
}
fn offer(db: &Connection, id: i64, handle: &str) -> Result<Offer> {
    let (owner, offer) = db.query_row("SELECT config_id,profile,account,credit,details,outcome FROM reset_grants WHERE handle=?", [handle],
        |r| Ok((r.get::<_, i64>(0)?, Offer { profile:r.get(1)?, account:r.get(2)?, credit:r.get(3)?, details:r.get(4)?, outcome:r.get(5)? })))
        .optional()?.ok_or_else(stale_grant)?;
    if owner != id {
        return Err(stale_config());
    }
    Ok(offer)
}

async fn consume(s: Shared, id: i64, handle: String) -> Result<Json<Value>> {
    let _slot = s.reset_write.try_lock().map_err(|_| busy())?;
    let (c, stamp, offer) = {
        let db = s.store.db()?;
        let (c, stamp) = profile(&s, &db, id)?;
        (c, stamp, offer(&db, id, &handle)?)
    };
    if stamp != offer.profile {
        return Err(stale_config());
    }
    let (mut rpc, listing) = inspect(&s, &c).await.map_err(|e| {
        if e.1 == "sign_in_required" {
            stale_config()
        } else {
            e
        }
    })?;
    if offer.account != listing.account || profile(&s, &*s.store.db()?, id)?.1 != stamp {
        return Err(stale_config());
    }
    if let Some(outcome) = offer.outcome {
        return Ok(Json(json!({"outcome":outcome})));
    }
    let grant = listing
        .grants
        .iter()
        .find(|g| g.credit == offer.credit && g.details == offer.details)
        .ok_or_else(stale_grant)?;
    let key: String = s.store.write(|db| {
        if profile(&s, db, id)?.1 != stamp { return Err(stale_config()); }
        if grant.expires.is_some_and(|t| t <= now()) || spent(db, &offer.credit)? {
            return Err(stale_grant());
        }
        // Profiles can share a native account. An uncertain write owns this account+credit's key even on restart.
        let key = db.query_row("SELECT attempt FROM reset_grants WHERE credit=? AND attempt IS NOT NULL AND outcome IS NULL LIMIT 1",
            [&offer.credit], |r| r.get(0)).optional()?.unwrap_or_else(|| Uuid::new_v4().to_string());
        if db.execute("UPDATE reset_grants SET attempt=?,seen_ms=? WHERE handle=? AND outcome IS NULL", params![key,now(),handle])? != 1 {
            return Err(stale_grant());
        }
        Ok(key)
    })?;
    let reply = timeout(
        RPC_TIMEOUT,
        rpc.call(
            4,
            "account/rateLimitResetCredit/consume",
            json!({"creditId":grant.id,"idempotencyKey":key}),
        ),
    )
    .await;
    let result = reply.ok().and_then(Result::ok).ok_or_else(unknown)?;
    let outcome = result["outcome"]
        .as_str()
        .filter(|o| {
            matches!(
                *o,
                "reset" | "alreadyRedeemed" | "nothingToReset" | "noCredit"
            )
        })
        .ok_or_else(unknown)?;
    let affected = s.store
        .write(|db| {
            db.execute(
                "UPDATE reset_grants SET outcome=?,seen_ms=? WHERE credit=? AND attempt=?",
                params![outcome, now(), offer.credit, key],
            )?;
            let mut affected = vec![id];
            let mut stmt = db.prepare("SELECT DISTINCT config_id,profile FROM reset_grants WHERE credit=? AND config_id<>?")?;
            for row in stmt.query_map(params![offer.credit,id], |r| Ok((r.get::<_, i64>(0)?,r.get::<_, String>(1)?)))? {
                let (other, stamp) = row?;
                if profile(&s, db, other).is_ok_and(|(_, current)| stamp == current) {
                    affected.push(other);
                }
            }
            for id in &affected { s.quotas.invalidate(db, *id)?; }
            Ok(affected)
        })
        .map_err(|_| unknown())?;
    tokio::spawn(quotas::after_reset(s.clone(), id, affected));
    rpc.close().await;
    Ok(Json(json!({"outcome":outcome})))
}
pub async fn redeem(
    State(s): State<Shared>,
    Path(id): Path<i64>,
    body: Body,
) -> Result<Json<Value>> {
    let invalid = || Error::bad("bad_request", "Expected only the selected reset handle.");
    let Json(v) = body.map_err(|_| invalid())?;
    let handle = v
        .as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.get("handle")?.as_str())
        .ok_or_else(invalid)?
        .to_owned();
    // Finish recording the provider response even if the browser disconnects.
    tokio::spawn(consume(s, id, handle))
        .await
        .map_err(|_| unknown())?
}
