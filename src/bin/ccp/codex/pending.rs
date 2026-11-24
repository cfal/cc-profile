use super::{reject, wire};
use ccpd::types::Result;
use serde_json::Value;
use std::collections::HashMap;

pub(super) const CAP: usize = 1024;
pub(super) const EPHEMERAL_FULL: &str = "Too many ephemeral threads on this connection.";

#[derive(serde::Deserialize)]
pub(super) struct Envelope {
    #[serde(default)]
    pub id: Value,
    pub method: Option<String>,
}

pub(super) enum Kind {
    Ordinary,
    List,
    Claim {
        source: Option<String>,
        ephemeral: bool,
    },
}

#[derive(Default)]
pub(super) struct Requests(HashMap<Value, Kind>);

pub(super) fn valid_id(id: &Value) -> bool {
    id.is_i64() || id.as_str().is_some_and(|s| s.len() <= 256)
}
pub(super) fn error_id(call: &Value) -> &Value {
    call.get("id")
        .filter(|id| valid_id(id))
        .unwrap_or(&Value::Null)
}

impl Requests {
    pub(super) fn admit(&mut self, call: &Value, ephemeral_count: usize) -> Result<()> {
        let Some(method) = call.get("method") else {
            return Ok(()); // Reply to a native request; its ID is in the opposite direction.
        };
        let method = method.as_str().ok_or_else(wire::bad)?;
        let id = &call["id"];
        if call.get("id").is_none()
            && !matches!(method, "thread/list" | "thread/start" | "thread/fork")
        {
            return Ok(());
        }
        if !valid_id(id) {
            return Err(reject(
                "Expected an integer request ID or a string of at most 256 bytes.",
            ));
        }
        if self.0.contains_key(id) {
            return Err(reject("Request ID is already pending on this connection."));
        }
        if self.0.len() >= CAP {
            return Err(reject("Too many pending requests on this connection."));
        }
        let kind = match method {
            "thread/list" => Kind::List,
            "thread/start" | "thread/fork" => {
                let ephemeral = call["params"]["ephemeral"] == true;
                if ephemeral {
                    let reserved = self
                        .0
                        .values()
                        .filter(|k| {
                            matches!(
                                k,
                                Kind::Claim {
                                    ephemeral: true,
                                    ..
                                }
                            )
                        })
                        .count();
                    if ephemeral_count + reserved >= CAP {
                        return Err(reject(EPHEMERAL_FULL));
                    }
                }
                let source = if method == "thread/fork" {
                    Some(
                        call["params"]["threadId"]
                            .as_str()
                            .ok_or_else(wire::bad)?
                            .to_owned(),
                    )
                } else {
                    None
                };
                Kind::Claim { source, ephemeral }
            }
            _ => Kind::Ordinary,
        };
        self.0.insert(id.clone(), kind);
        Ok(())
    }

    pub(super) fn remove(&mut self, id: &Value) -> Option<Kind> {
        self.0.remove(id)
    }

    pub(super) fn may_stream(&self) -> bool {
        self.0.values().all(|k| matches!(k, Kind::Ordinary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pending_caps_do_not_evict_claims_or_overbook_ephemeral_threads() {
        let mut pending = Requests::default();
        let start = |id| json!({"id":id, "method":"thread/start", "params":{"ephemeral":true}});
        pending.admit(&start(0), CAP - 1).unwrap();
        assert!(pending.admit(&start(1), CAP - 1).is_err());
        for id in 1..CAP {
            pending
                .admit(&json!({"id":id, "method":"turn/start"}), 0)
                .unwrap();
        }
        assert!(
            pending
                .admit(&json!({"id":CAP, "method":"thread/list"}), 0)
                .is_err()
        );
        assert!(!pending.may_stream());
        assert!(matches!(
            pending.remove(&json!(0)),
            Some(Kind::Claim { .. })
        ));
        assert!(pending.may_stream());
        assert!(pending.admit(&start(0), CAP).is_err());
        pending.admit(&start(0), CAP - 1).unwrap();
    }
}
