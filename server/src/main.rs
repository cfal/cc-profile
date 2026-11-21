mod auth;
mod callback;
mod password;
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
            "Usage: ccpd [--port PORT] [--claude-bin PATH] [--codex-bin PATH]\n       ccpd set-password",
        )
    };
    while let Some(k) = args.next() {
        match k.as_str() {
            "set-password" if args.len() == 0 => return password::prompt(),
            "-V" | "--version" => {
                println!("ccpd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
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
    ccpd::observer::binary()?;
    let runtime = paths::runtime_dir()?;
    paths::private_dir(&state)?;
    paths::private_dir(&runtime)?;
    let _state_lock = paths::locked(&state.join("service.lock"))?;
    let _listener_lock = paths::locked(&runtime.join("listener.lock"))?;
    let store = Store::open_user(&state)?;
    store.prune()?;
    for t in ["claude", "codex"] {
        if ccpd::links::repair(&store, t).is_err() {
            eprintln!("ccpd: {t} compatibility link needs repair (Accounts).");
        }
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
        auth: auth::Auth::new(),
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
        "ccpd {}: http://127.0.0.1:{port}\npanel database: {}\nlaunch socket: {}",
        env!("CARGO_PKG_VERSION"),
        Store::user_db_path(&app.state_dir).display(),
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
