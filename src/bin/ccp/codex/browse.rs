use super::{Connection, deadline, reject, request};
use ccpd::{
    paths,
    protocol::{CODEX_LISTED, CodexListed, Plan},
    types::{Error, Result},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};
use tokio::time::{Instant, timeout};
use uuid::Uuid;

const NATIVE_PAGE: usize = 100;
const MAX_PUBLIC_PAGE: u32 = 500;
const MAX_ROWS: usize = 2048;
const MAX_PAGES: usize = 128;
const MAX_BYTES: usize = 8 << 20;
const LIST_TIMEOUT: Duration = Duration::from_secs(35);
const CURSOR_TTL: Duration = Duration::from_secs(60);

fn limit_error() -> Error {
    reject("Codex thread listing exceeds the managed browse limit. Narrow the native filters.")
}
fn invalid_page() -> Error {
    reject("A Codex profile returned an invalid or incomplete list; no results were released.")
}
fn stale() -> Error {
    reject("This Codex browse cursor is no longer valid. Restart the list on this connection.")
}

fn roster(plans: &[(Plan, i64)]) -> Vec<(i64, i64)> {
    plans.iter().map(|(p, rev)| (p.config_id, *rev)).collect()
}

struct Query {
    filters: Map<String, Value>,
    cursor: Option<String>,
    limit: usize,
    key: &'static str,
    ascending: bool,
}
impl Query {
    fn parse(params: &Value) -> Result<Self> {
        if serde_json::to_vec(params)
            .map_err(|_| invalid_page())?
            .len()
            > 64 * 1024
        {
            return Err(limit_error());
        }
        let mut filters = match params {
            Value::Null => Map::new(),
            Value::Object(obj) => obj.clone(),
            _ => return Err(reject("Expected Codex thread/list parameters.")),
        };
        let cursor = match filters.remove("cursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.len() == 36 => Some(s),
            _ => return Err(stale()),
        };
        let limit = match filters.remove("limit") {
            None | Some(Value::Null) => 25,
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| reject("Invalid Codex thread/list limit."))?
                .clamp(1, MAX_PUBLIC_PAGE) as usize,
        };
        let key = match filters.get("sortKey") {
            None | Some(Value::Null) => "createdAt",
            Some(v) if v == "created_at" => "createdAt",
            Some(v) if v == "updated_at" => "updatedAt",
            _ => {
                return Err(reject(
                    "Managed Codex browse supports created_at or updated_at sorting.",
                ));
            }
        };
        let ascending = match filters.get("sortDirection") {
            None | Some(Value::Null) => false,
            Some(v) if v == "asc" => true,
            Some(v) if v == "desc" => false,
            _ => return Err(reject("Invalid Codex thread/list sort direction.")),
        };
        if filters
            .get("archived")
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
            || filters
                .get("useStateDbOnly")
                .is_some_and(|v| !v.is_boolean())
        {
            return Err(reject("Invalid Codex thread/list filters."));
        }
        Ok(Self {
            filters,
            cursor,
            limit,
            key,
            ascending,
        })
    }
}

struct Snapshot {
    profiles: Vec<(i64, i64)>,
    filters: Map<String, Value>,
    rows: VecDeque<Row>,
    read_states_available: bool,
    cursor: String,
    expires: Instant,
}
// Charge the bytes we actually retain between pages, not a much larger decoded JSON tree.
struct Row {
    sort: i64,
    id: Uuid,
    data: Box<[u8]>,
    receipt: Option<Box<[u8]>>,
}

#[derive(Default)]
pub(super) struct Browse(Option<Snapshot>);
impl Browse {
    pub(super) async fn list(&mut self, conn: &Connection, call: &Value) -> Result<Value> {
        // A replacement query or any interrupted continuation invalidates this single-use cursor.
        let previous = self.0.take();
        let query = Query::parse(&call["params"])?;
        let mut snapshot = timeout(LIST_TIMEOUT, async {
            if let Some(cursor) = &query.cursor {
                let plans = conn.plans(true).await?;
                return previous
                    .filter(|s| {
                        &s.cursor == cursor
                            && s.filters == query.filters
                            && s.profiles == roster(&plans)
                            && Instant::now() < s.expires
                    })
                    .ok_or_else(stale);
            }
            drop(previous);
            let runtime = paths::runtime_dir()?;
            paths::private_dir(&runtime)?;
            let _slot = paths::locked(&runtime.join("codex-browse.lock")).map_err(|err| {
                if err.1 == "already_running" {
                    Error::conflict(
                        "browse_busy",
                        "Another Codex thread listing is in progress. Retry shortly.",
                    )
                } else {
                    err
                }
            })?;
            let plans = conn.plans(true).await?;
            collect(conn, &query, &plans).await
        })
        .await
        .map_err(|_| reject("Codex thread listing timed out. No results were released."))??;

        let mut data = Vec::new();
        let mut receipts = snapshot.read_states_available.then(Map::new);
        for row in snapshot.rows.drain(..query.limit.min(snapshot.rows.len())) {
            data.push(serde_json::from_slice::<Value>(&row.data).map_err(|_| invalid_page())?);
            if let Some(map) = &mut receipts
                && let Some(receipt) = row.receipt
            {
                map.insert(
                    row.id.to_string(),
                    serde_json::from_slice(&receipt).map_err(|_| invalid_page())?,
                );
            }
        }
        let next = if snapshot.rows.is_empty() {
            None
        } else {
            let token = Uuid::new_v4().to_string();
            snapshot.cursor.clone_from(&token);
            self.0 = Some(snapshot);
            Some(token)
        };
        Ok(json!({"data":data, "readStates":receipts, "nextCursor":next, "backwardsCursor":null}))
    }
}

async fn collect(conn: &Connection, query: &Query, plans: &[(Plan, i64)]) -> Result<Snapshot> {
    let profiles = roster(plans);
    let mut evidence = Vec::new();
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut read_states_available = !plans.is_empty();
    let mut count_bytes = 0;
    let mut pages = 0;
    for (index, (plan, revision)) in plans.iter().enumerate() {
        // A preceding worker may have been slow. Don't start the next child
        // using a selection captured before that wait.
        if index > 0 {
            let current = conn.plans(true).await?;
            if roster(&current) != profiles
                || !current.iter().any(|(p, r)| p == plan && r == revision)
            {
                return Err(stale());
            }
        }
        let mut server = conn.connected(plan).await?;
        let mut cursors = HashSet::new();
        let mut params = query.filters.clone();
        params.insert("limit".into(), json!(NATIVE_PAGE));
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(limit_error());
            }
            let call = json!({"id":pages,"method":"thread/list","params":params});
            let mut response = server.exchange(&call).await?;
            let result = response
                .get_mut("result")
                .and_then(Value::as_object_mut)
                .ok_or_else(invalid_page)?;
            let Some(Value::Array(data)) = result.remove("data") else {
                return Err(invalid_page());
            };
            if data.len() > NATIVE_PAGE || rows.len() + data.len() > MAX_ROWS {
                return Err(limit_error());
            }
            let mut states = match result.remove("readStates") {
                None | Some(Value::Null) => {
                    read_states_available = false;
                    Map::new()
                }
                Some(Value::Object(map)) => map,
                _ => return Err(invalid_page()),
            };
            for row in data {
                let id = row["id"].as_str().ok_or_else(invalid_page)?;
                let uuid = Uuid::parse_str(id).map_err(|_| invalid_page())?;
                let sort = row[query.key].as_i64().ok_or_else(invalid_page)?;
                if uuid.to_string() != id || row["ephemeral"] != false {
                    return Err(invalid_page());
                }
                let cwd = row["cwd"]
                    .as_str()
                    .filter(|s| ccpd::paths::workspace(s))
                    .ok_or_else(invalid_page)?;
                if !seen.insert(uuid) {
                    return Err(reject(
                        "A duplicate Codex thread ID makes this list ambiguous. No results were released.",
                    ));
                }
                let data = serde_json::to_vec(&row)
                    .map_err(|_| invalid_page())?
                    .into_boxed_slice();
                let receipt = states
                    .remove(id)
                    .map(|v| serde_json::to_vec(&v).map(Vec::into_boxed_slice))
                    .transpose()
                    .map_err(|_| invalid_page())?;
                count_bytes += data.len() + receipt.as_ref().map_or(0, |r| r.len());
                if count_bytes > MAX_BYTES {
                    return Err(limit_error());
                }
                evidence.push((plan.config_id, id.to_owned(), cwd.to_owned()));
                rows.push(Row {
                    sort,
                    id: uuid,
                    data,
                    receipt,
                });
            }
            if !states.is_empty() {
                return Err(invalid_page());
            }
            match result.remove("nextCursor") {
                Some(Value::Null) => break,
                Some(Value::String(token))
                    if !token.is_empty()
                        && token.len() <= 4096
                        && cursors.insert(token.clone()) =>
                {
                    params.insert("cursor".into(), Value::String(token));
                }
                _ => return Err(invalid_page()),
            }
        }
        server.child.kill().await?;
    }

    rows.sort_unstable_by(|a, b| {
        let order = (a.sort, a.id).cmp(&(b.sort, b.id));
        if query.ascending {
            order
        } else {
            order.reverse()
        }
    });
    // Publication checks the complete roster and all owners atomically; no titles or history cross IPC.
    let packet = serde_json::to_vec(&CodexListed {
        binary: conn.base.binary.clone().ok_or_else(invalid_page)?,
        bound: None,
        profiles: profiles.clone(),
        threads: evidence,
    })
    .map_err(|_| limit_error())?;
    if packet.len() > 256 * 1024 {
        return Err(limit_error());
    }
    let confirmed: usize = request(CODEX_LISTED, &packet, deadline()).await?;
    if confirmed != rows.len() {
        return Err(invalid_page());
    }
    Ok(Snapshot {
        profiles,
        filters: query.filters.clone(),
        rows: rows.into(),
        read_states_available,
        cursor: String::new(),
        expires: Instant::now() + CURSOR_TTL,
    })
}
