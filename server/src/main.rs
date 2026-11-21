mod callback;
mod pairing;
mod quotas;
mod service;
mod workspace;
use ccpd::{
    native::Build,
    paths,
    store::Store,
    types::{Error, Result},
};
use std::{
    env, fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    sync::Arc,
    time::Duration,
};
use tokio::net::{TcpListener, UnixListener};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ccpd: {e}");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    unsafe { libc::umask(0o077) };
    let mut args = env::args().skip(1);
    let mut port = 8743;
    let mut claude = "claude".to_owned();
    let mut codex = "codex".to_owned();
    let bad = || {
        Error::bad(
            "arguments",
            "Usage: ccpd [--port PORT] [--claude-bin PATH] [--codex-bin PATH]",
        )
    };
    while let Some(k) = args.next() {
        match k.as_str() {
            "observe" => return callback::run().await,
            "-h" | "--help" => {
                println!("{}", bad());
                return Ok(());
            }
            "--port" => port = args.next().ok_or_else(bad)?.parse().map_err(|_| bad())?,
            "--claude-bin" => claude = args.next().ok_or_else(bad)?,
            "--codex-bin" => codex = args.next().ok_or_else(bad)?,
            _ => return Err(bad()),
        }
    }
    let state = paths::state_dir()?;
    let runtime = paths::runtime_dir()?;
    paths::private_dir(&state)?;
    paths::private_dir(&runtime)?;
    let _state_lock = paths::locked(&state.join("service.lock"))?;
    let _listener_lock = paths::locked(&runtime.join("listener.lock"))?;
    let db = state.join("router.sqlite3");
    let initialized = state.join("initialized");
    if initialized.exists() && !db.exists() {
        return Err(Error::unavailable(
            "missing_database",
            "Database is missing; restore it before starting ccpd.",
        ));
    }
    if db.symlink_metadata().is_ok_and(|m| !m.is_file()) {
        return Err(Error::denied());
    }
    let store = Store::open(&db)?;
    store.prune()?;
    for t in ["claude", "codex"] {
        if ccpd::links::repair(&store, t).is_err() {
            eprintln!("ccpd: {t} compatibility link needs repair (Accounts).");
        }
    }
    if !initialized.exists() {
        paths::replace(&initialized, b"router schema 1\n", None)?;
    }
    let tokenfile = runtime.join("pairing-token");
    let key = if tokenfile.try_exists()? {
        let m = tokenfile.symlink_metadata()?;
        use std::os::unix::fs::MetadataExt;
        if !m.is_file() || m.uid() != paths::uid() || m.mode() & 0o077 != 0 {
            return Err(Error::denied());
        }
        String::from_utf8(paths::read_limited(&tokenfile, 128)?)
            .map_err(|_| Error::denied())?
            .trim()
            .to_owned()
    } else {
        let v = paths::secret()?;
        paths::replace(&tokenfile, format!("{v}\n").as_bytes(), None)?;
        v
    };
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::denied());
    }
    let tcp = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|_| Error::unavailable("listener", "Could not bind loopback panel port."))?;
    let port = tcp.local_addr()?.port();
    let sock = runtime.join("launch.sock");
    match fs::symlink_metadata(&sock) {
        Ok(m) if m.file_type().is_socket() => fs::remove_file(&sock)?,
        Ok(_) => {
            return Err(Error::conflict(
                "socket_path",
                "Refusing to replace non-socket at launch.sock.",
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let unix = UnixListener::bind(&sock)?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
    let claude = Build::detect("claude", &claude, &state).await;
    let codex = Build::detect("codex", &codex, &state).await;
    let app = Arc::new(service::App {
        store,
        claude,
        codex,
        state_dir: state,
        port,
        pairing: pairing::Pairing::new(&key),
        quotas: quotas::Jobs::new(),
        workspace: workspace::Checks::new(),
    });
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            let a = a.clone();
            let _ = service::blocking(move || a.store.prune()).await;
        }
    });
    eprintln!(
        "ccpd {}: http://127.0.0.1:{port}\npairing token file: {}\nlaunch socket: {}",
        env!("CARGO_PKG_VERSION"),
        tokenfile.display(),
        sock.display()
    );
    tokio::select! {
        result=axum::serve(unix,service::private(app.clone()).into_make_service_with_connect_info::<service::Peer>())=>result?,
        result=axum::serve(tcp,service::web(app))=>result?,
        _=tokio::signal::ctrl_c()=>{},
    }
    let _ = fs::remove_file(sock);
    Ok(())
}
