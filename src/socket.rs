use crate::{
    paths,
    types::{Error, Result},
};
use axum::{
    body::{Body, Bytes, to_bytes},
    http::StatusCode,
};
use hyper::{Request, client::conn::http1};
use hyper_util::rt::TokioIo;
use std::{error::Error as _, os::unix::fs::MetadataExt, time::Duration};
use tokio::net::UnixStream;

pub const SOCKET_NAME: &str = "ccpd.sock";

pub fn unreachable() -> Error {
    Error::unavailable("transport", "Cannot reach ccpd on the private socket.")
}

pub async fn post(route: &'static str, body: &[u8]) -> Result<(StatusCode, Bytes)> {
    let dir = paths::ipc_dir()?;
    let m = dir.symlink_metadata().map_err(|_| unreachable())?;
    if !m.is_dir() || dir.canonicalize()? != dir || m.uid() != paths::uid() || m.mode() & 0o077 != 0
    {
        return Err(Error::bad("socket", "Unsafe router socket directory."));
    }
    let socket = tokio::time::timeout(
        Duration::from_secs(2),
        UnixStream::connect(dir.join(SOCKET_NAME)),
    )
    .await
    .map_err(|_| unreachable())?
    .map_err(|_| unreachable())?;
    if socket.peer_cred()?.uid() != paths::uid() {
        return Err(Error::bad("peer", "Router is not running as this user."));
    }
    let (mut sender, conn) = http1::handshake(TokioIo::new(socket))
        .await
        .map_err(|_| unreachable())?;
    tokio::spawn(conn);
    let req = Request::post(route)
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_owned()))
        .map_err(|_| Error::bad("request", "Invalid local request."))?;
    let res = sender.send_request(req).await.map_err(|_| unreachable())?;
    let status = res.status();
    let bytes = to_bytes(Body::new(res.into_body()), 512 * 1024)
        .await
        .map_err(|e| {
            if e.source()
                .is_some_and(|s| s.is::<http_body_util::LengthLimitError>())
            {
                Error::bad("reply_size", "Router reply too large.")
            } else {
                unreachable()
            }
        })?;
    Ok((status, bytes))
}
