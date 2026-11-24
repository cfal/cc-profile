//! Deferred Codex stdio rejects client frames over 16 MiB without disconnecting.
//! The error carries the top-level request ID if it fits in the first 4 KiB,
//! otherwise null. Oversized native frames stream only after binding, with no
//! unconfirmed list, start or fork; private handshakes remain size-limited.
use super::{Incoming, Out, send, wire};
use ccpd::types::Result;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::{Deserializer, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::ChildStdout,
};

// Only recover correlation metadata, never routing selectors. The delimiter
// after an ID matters: a truncated number could match a different request.
#[derive(Default)]
struct Header {
    id: Option<Value>,
    method: bool,
    reply: bool,
}

fn header(line: &[u8]) -> Header {
    fn token<T: DeserializeOwned>(input: &mut &[u8]) -> Option<T> {
        let mut stream = Deserializer::from_slice(input).into_iter();
        let value = stream.next()?.ok()?;
        *input = input[stream.byte_offset()..].trim_ascii_start();
        Some(value)
    }
    fn fields(prefix: &[u8], h: &mut Header) -> Option<()> {
        let mut input = prefix.trim_ascii_start().strip_prefix(b"{")?;
        loop {
            let key: String = token(&mut input)?;
            input = input.strip_prefix(b":")?;
            if key == "id" {
                let id: Value = token(&mut input)?;
                if !matches!(input.first(), Some(b',' | b'}')) {
                    return None;
                }
                h.id = super::pending::valid_id(&id).then_some(id);
            } else {
                h.method |= key == "method";
                h.reply |= matches!(key.as_str(), "result" | "error");
                token::<IgnoredAny>(&mut input)?;
            }
            input = input.strip_prefix(b",")?;
        }
    }
    let mut h = Header::default();
    fields(&line[..line.len().min(4096)], &mut h);
    h
}

pub(super) async fn client(rx: &mut Incoming, out: &Out) -> Result<Option<Vec<u8>>> {
    while let Some(frame) = rx.recv().await.transpose()? {
        if frame.len() <= wire::LIMIT {
            return Ok(Some(frame));
        }
        let id = header(&frame).id.unwrap_or(Value::Null);
        let error = wire::error(
            &id,
            "Codex client frame exceeds 16 MiB; not forwarded (null id if unavailable in the first 4 KiB).",
        );
        send(out, &error).await?;
    }
    Ok(None)
}

pub(super) async fn bound_output(
    r: &mut BufReader<ChildStdout>,
    out: &Out,
    pending: &std::cell::RefCell<super::pending::Requests>,
) -> Result<Option<Vec<u8>>> {
    while let Some(prefix) = wire::prefix(r).await? {
        if prefix.len() <= wire::LIMIT {
            return Ok(Some(prefix));
        }
        if !pending.borrow().may_stream() {
            return Err(wire::bad());
        }
        // A native request may reuse a client ID in the opposite direction.
        // If the reply envelope isn't recognizable, leave its ID reserved.
        let h = header(&prefix);
        if h.reply
            && !h.method
            && let Some(id) = h.id
        {
            pending.borrow_mut().remove(&id);
        }
        // Commit to this native frame before exposing its first byte. Keep CCP
        // errors from the client pump out of the middle of the streamed line.
        let mut out = out.lock().await;
        out.write_all(&prefix).await?;
        let mut done = prefix.ends_with(b"\n");
        drop(prefix);
        while !done {
            let buf = r.fill_buf().await?;
            if buf.is_empty() {
                return Err(wire::bad());
            }
            let n = buf
                .iter()
                .position(|&b| b == b'\n')
                .map_or(buf.len(), |i| i + 1);
            done = buf[n - 1] == b'\n';
            out.write_all(&buf[..n]).await?;
            r.consume(n);
        }
        out.flush().await?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bounded_header_never_uses_nested_ids_or_truncated_numbers() {
        let h = header(
            br#"{"params":{"id":"not this one","result":1},"id":"a\u002db","method":"approve"}"#,
        );
        assert_eq!(h.id, Some(json!("a-b")));
        assert!(h.method && !h.reply);
        let partial = format!(r#"{{"id":{}123456789,"result":{{}}}}"#, " ".repeat(4088));
        assert!(header(partial.as_bytes()).id.is_none());
        let h = header(br#"{"id":17,"result":{"#);
        assert_eq!(h.id, Some(json!(17)));
        assert!(h.reply && !h.method);
    }
}
