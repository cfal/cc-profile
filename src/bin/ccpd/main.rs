mod auth;
mod device_login;
mod identity;
mod password;
mod quotas;
mod service;
use ccpd::{
    native::Build,
    paths,
    store::Store,
    types::{Error, Result},
};
use std::{
    env, fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
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
    let mut listen = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mut port = 8743;
    let mut claude = None;
    let mut agy = None;
    let mut codex = None;
    let bad = || {
        Error::bad(
            "arguments",
            "Usage: ccpd [--listen IP] [--port PORT] [--claude-bin PATH] [--codex-bin PATH] [--agy-bin PATH]\n       ccpd set-password",
        )
    };
    while let Some(k) = args.next() {
        match k.as_str() {
            "set-password" if args.len() == 0 => return password::prompt(),
            "-V" | "--version" => {
                println!("ccpd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!("{}", bad());
                return Ok(());
            }
            "--listen" => listen = args.next().ok_or_else(bad)?.parse().map_err(|_| bad())?,
            "--port" => port = args.next().ok_or_else(bad)?.parse().map_err(|_| bad())?,
            "--claude-bin" => claude = Some(args.next().ok_or_else(bad)?),
            "--agy-bin" => agy = Some(args.next().ok_or_else(bad)?),
            "--codex-bin" => codex = Some(args.next().ok_or_else(bad)?),
            _ => return Err(bad()),
        }
    }
    let state = paths::state_dir()?;
    let runtime = paths::runtime_dir()?;
    paths::private_dir(&state)?;
    paths::private_dir(&runtime)?;
    let _state_lock = paths::locked(&state.join("service.lock"))?;
    let _listener_lock = paths::locked(&runtime.join("listener.lock"))?;
    let store = Store::open_user(&state)?;
    store.prune()?;
    let addr = SocketAddr::new(listen, port);
    let tcp = TcpListener::bind(addr).await.map_err(|e| {
        Error::unavailable("listener", format!("Could not bind panel to {addr}: {e}"))
    })?;
    let bound = tcp.local_addr()?;
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
    let claude = Build::detect("claude", claude.as_deref(), &state).await;
    let codex = Build::detect("codex", codex.as_deref(), &state).await;
    let agy = Build::detect("agy", agy.as_deref(), &state).await;
    let app = Arc::new(service::App {
        store,
        claude,
        codex,
        agy,
        state_dir: state,
        auth: auth::Auth::new(),
        quotas: quotas::Jobs::new(),
        logins: device_login::Logins::default(),
        identity_slots: tokio::sync::Semaphore::new(2),
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
        "ccpd {}: http://{bound}\npanel database: {}\nlaunch socket: {}",
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
