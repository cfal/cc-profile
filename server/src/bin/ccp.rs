use ccpd::{
    native::{MARKERS, PROXIES, executable},
    paths,
    protocol::{Failure, Plan, Request, VERSION},
    socket,
    types::{Error, Result},
};
use serde::Deserialize;
use std::{
    env,
    ffi::{CString, OsString},
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    process::ExitCode,
    time::Duration,
};
use tokio::time::{Instant, timeout_at};
use uuid::Uuid;

const HELP: &str = "Usage: ccp <claude|codex> [native arguments...]

  ccp claude --model opus
  ccp codex exec --json 'review these changes'
  ccp claude --resume <session-uuid>
  ccp codex resume <session-uuid>

New chats rotate; exact resumes use their original config. No transfers,
pickers or --last. Manage accounts and Current in the ccpd web panel
(default http://127.0.0.1:8743).";
const CLAUDE_OVERRIDES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FD",
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
];
const CODEX_OVERRIDES: &[&str] = &[
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "OPENAI_BASE_URL",
    "CODEX_SQLITE_HOME",
    "CODEX_MANAGED_CONFIG_PATH",
    "CODEX_CHATGPT_BASE_URL",
];

fn invalid() -> Error {
    Error::bad("invalid_plan", "Invalid launch plan; refusing to execute.")
}
fn utf8() -> Error {
    Error::bad("encoding", "Arguments and working directory must be UTF-8.")
}
fn validate<'a>(plan: &'a Plan, req: &Request) -> Result<&'a str> {
    let home = if req.tool == "codex" {
        "CODEX_HOME"
    } else {
        "CLAUDE_CONFIG_DIR"
    };
    let allowed = |k: &str| k == home || PROXIES.contains(&k) || MARKERS.contains(&k);
    let path = plan.env_set.get(home).ok_or_else(invalid)?;
    if plan.v != VERSION
        || plan.request_id != req.request_id
        || plan.tool != req.tool
        || !Path::new(path).is_absolute()
        || path.chars().any(char::is_control)
        || plan.label.chars().any(char::is_control)
        || plan.argv.iter().any(|s| s.contains('\0'))
        || plan
            .env_set
            .iter()
            .any(|(k, v)| !allowed(k) || v.contains('\0'))
        || plan
            .env_unset
            .iter()
            .any(|k| !allowed(k) || plan.env_set.contains_key(k))
    {
        return Err(invalid());
    }
    if MARKERS.iter().any(|k| plan.env_set.contains_key(*k)) {
        if req.tool != "codex"
            || plan.env_set.get(MARKERS[0]) != Some(&req.request_id)
            || plan.env_set.get(MARKERS[1]) != Some(&req.ticket)
        {
            return Err(invalid());
        }
    } else if !MARKERS
        .iter()
        .all(|k| plan.env_unset.iter().any(|v| v == k))
    {
        return Err(invalid());
    }
    Ok(path)
}

#[derive(Deserialize)]
struct Denial {
    message: String,
}

async fn request_plan(r: &Request) -> Result<Plan> {
    let body = serde_json::to_vec(r).map_err(|_| utf8())?;
    if body.len() > 256 * 1024 {
        return Err(Error::bad(
            "request_size",
            "Launch request too large; use stdin for a long prompt.",
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    for attempt in 0..2 {
        // Retry transport failures only, with identical bytes and the original deadline.
        let (status, reply) = match timeout_at(deadline, socket::post("/v1/launch", &body)).await {
            Ok(Err(e)) if e.1 == "transport" && attempt == 0 => continue,
            Ok(result) => result?,
            Err(_) => return Err(socket::unreachable()),
        };
        if status != axum::http::StatusCode::OK {
            let msg = serde_json::from_slice::<Denial>(&reply)
                .map(|v| v.message)
                .unwrap_or_else(|_| format!("Router refused the launch ({status})."));
            return Err(Error(status, "refused", msg));
        }
        return serde_json::from_slice(&reply).map_err(|_| invalid());
    }
    Err(socket::unreachable())
}

fn exec_native(binary: &Path, r: &Request, p: &Plan) -> io::Error {
    let mut env = env::vars_os().collect::<std::collections::BTreeMap<_, _>>();
    for k in &p.env_unset {
        env.remove(std::ffi::OsStr::new(k));
    }
    for (k, v) in &p.env_set {
        env.insert(k.into(), v.into());
    }
    let vars: Vec<_> = env
        .into_iter()
        .map(|(mut k, v)| {
            k.push("=");
            k.push(v);
            CString::new(k.as_bytes()).unwrap()
        })
        .collect();
    let args: Vec<_> = std::iter::once(&r.tool)
        .chain(&p.argv)
        .map(|s| CString::new(s.as_bytes()).unwrap())
        .collect();
    let pointers = |s: &[CString]| {
        s.iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect::<Vec<_>>()
    };
    let args_ptr = pointers(&args);
    let env_ptr = pointers(&vars);
    let path = CString::new(binary.as_os_str().as_bytes()).unwrap();
    // Use execve directly: no implicit shell fallback for ENOEXEC. All strings
    // above remain alive and the pointer arrays are null-terminated.
    unsafe {
        libc::execve(path.as_ptr(), args_ptr.as_ptr(), env_ptr.as_ptr());
    }
    io::Error::last_os_error()
}
async fn run() -> Result<ExitCode> {
    let mut args = env::args_os().skip(1);
    let first = args.next();
    let tool = match first
        .as_deref()
        .map(|s| s.to_str().ok_or_else(utf8))
        .transpose()?
    {
        None | Some("-h" | "--help") => {
            println!("{HELP}");
            return Ok(ExitCode::SUCCESS);
        }
        Some("--version" | "-V") => {
            println!("ccp {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Some(t @ ("claude" | "codex")) => t,
        _ => {
            eprintln!("{HELP}");
            return Ok(ExitCode::from(2));
        }
    };
    let argv = args
        .map(OsString::into_string)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| utf8())?;
    let binary = executable(tool);
    if let Some(path) = &binary {
        let own = fs::metadata(env::current_exe()?)?;
        if fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == (own.dev(), own.ino())) {
            eprintln!("ccp: native command resolves to the launcher.");
            return Ok(ExitCode::from(2));
        }
        path.to_str().ok_or_else(utf8)?;
    }
    let cwd = env::current_dir()?;
    cwd.to_str().ok_or_else(utf8)?;
    let names = if tool == "claude" {
        CLAUDE_OVERRIDES
    } else {
        CODEX_OVERRIDES
    };
    let mut override_names: Vec<String> = names
        .iter()
        .filter(|k| env::var_os(k).is_some_and(|v| !v.is_empty()))
        .map(|k| (*k).into())
        .collect();
    override_names.sort_unstable();
    let req = Request {
        request_id: Uuid::new_v4().to_string(),
        ticket: paths::secret()?,
        tool: tool.into(),
        argv,
        cwd,
        binary,
        override_names,
    };
    let plan = request_plan(&req).await?;
    let home = validate(&plan, &req)?;
    let binary = req
        .binary
        .as_deref()
        .ok_or_else(|| Error::bad("binary", "Native executable not found on PATH."))?;
    eprintln!("ccp: {tool}/{} -> {home}", plan.label);
    let error = exec_native(binary, &req, &plan);
    let errno = error.raw_os_error().unwrap_or(libc::EIO);
    let failure = Failure {
        request_id: req.request_id,
        ticket: req.ticket,
        errno,
    };
    if let Ok(body) = serde_json::to_vec(&failure) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            socket::post("/v1/exec-failure", &body),
        )
        .await;
    }
    eprintln!("ccp: exec failed: {error}");
    Ok(ExitCode::from(if errno == libc::ENOENT {
        127
    } else {
        126
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    run().await.unwrap_or_else(|e| {
        eprintln!("ccp: {e}");
        ExitCode::FAILURE
    })
}
