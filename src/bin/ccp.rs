use ccpd::{
    invocation::{Intent, Invocation, classify},
    native::{PROXIES, executable},
    paths,
    protocol::{Failure, Plan, Request, RoutingMode, VERSION, profile_login_args},
    socket,
    types::{Error, Result},
};
use serde::{Deserialize, de::DeserializeOwned};
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

#[path = "ccp/acp.rs"]
mod acp;
#[path = "ccp/child.rs"]
mod child;
#[path = "ccp/codex.rs"]
mod codex;

const HELP: &str = "Usage: ccp <claude|codex|agy> [native arguments...]

  ccp claude --model opus
  ccp codex exec --json 'review these changes'
  ccp claude --resume <session-uuid>
  ccp codex resume <session-uuid>
  ccp codex app-server [--for-session <uuid>]
  ccp agy acp [--for-session <session-id>]
  ccp login <profile-id> [--status]
  ccp usage <antigravity-profile-id>

New chats follow the routing policy; exact resumes use their original config. No transfers,
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
fn validate<'a>(plan: &'a Plan, req: &Request) -> Result<(&'a str, Option<Invocation>)> {
    let home = if req.tool == "agy" {
        "HOME"
    } else if req.tool == "codex" {
        "CODEX_HOME"
    } else {
        "CLAUDE_CONFIG_DIR"
    };
    let allowed = |k: &str| k == home || PROXIES.contains(&k);
    let path = plan.env_set.get(home).ok_or_else(invalid)?;
    if plan.v != VERSION
        || plan.request_id != req.request_id
        || plan.tool != req.tool
        || req.login_profile.is_some_and(|id| {
            id != plan.config_id || plan.argv != req.argv || plan.decision != "profile_login"
        })
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
    let mut managed = None;
    if matches!(req.tool.as_str(), "codex" | "agy") && req.login_profile.is_none() {
        let i = classify(&req.tool, &req.argv, &req.cwd)?;
        let valid_decision = match i.intent {
            Intent::New => RoutingMode::parse(&plan.decision).is_some(),
            Intent::Resume(_) => plan.decision == "sticky",
            Intent::Misc => plan.decision == "current",
        };
        if plan.argv != req.argv || !valid_decision || plan.config_id <= 0 {
            return Err(invalid());
        }
        if i.intent != Intent::Misc {
            managed = Some(i);
        }
    }
    Ok((path, managed))
}

#[derive(Deserialize)]
struct Denial {
    message: String,
}

async fn request<T: DeserializeOwned>(
    route: &'static str,
    body: &[u8],
    deadline: Instant,
) -> Result<T> {
    if body.len() > 256 * 1024 {
        return Err(Error::bad(
            "request_size",
            "Launch request too large; use stdin for a long prompt.",
        ));
    }
    for attempt in 0..2 {
        // Retry transport failures only, with identical bytes and the original deadline.
        let (status, reply) = match timeout_at(deadline, socket::post(route, body)).await {
            Ok(Err(e)) if e.1 == "transport" && attempt == 0 => continue,
            Ok(result) => result?,
            Err(_) => {
                return Err(Error::unavailable(
                    "timeout",
                    "Timed out waiting for a routing decision. Nothing was executed.",
                ));
            }
        };
        if status != axum::http::StatusCode::OK {
            let msg = serde_json::from_slice::<Denial>(&reply)
                .map(|v| v.message)
                .unwrap_or_else(|_| format!("Router refused the request ({status}). Check that ccp and ccpd are up to date."));
            return Err(Error(status, "refused", msg));
        }
        return serde_json::from_slice(&reply).map_err(|_| invalid());
    }
    Err(socket::unreachable())
}

struct Strings {
    _values: Vec<CString>,
    pointers: Vec<*const libc::c_char>,
}
// Pointers refer only to immutable CString allocations owned by this list.
unsafe impl Send for Strings {}
unsafe impl Sync for Strings {}
impl Strings {
    fn new(values: Vec<CString>) -> Self {
        let pointers = values
            .iter()
            .map(|v| v.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        Self {
            _values: values,
            pointers,
        }
    }
}
struct NativeExec {
    path: CString,
    args: Strings,
    vars: Strings,
}
impl NativeExec {
    fn new(binary: &Path, tool: &str, argv: &[String], p: &Plan) -> Self {
        let mut env = env::vars_os().collect::<std::collections::BTreeMap<_, _>>();
        // A shell spawned by a pre-v2 chat can still carry the old hook ticket.
        for k in ["CCP_LAUNCH_ID", "CCP_OBSERVE_TICKET"] {
            env.remove(std::ffi::OsStr::new(k));
        }
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
        let args: Vec<_> = std::iter::once(tool)
            .chain(argv.iter().map(String::as_str))
            .map(|s| CString::new(s.as_bytes()).unwrap())
            .collect();
        let path = CString::new(binary.as_os_str().as_bytes()).unwrap();
        Self {
            path,
            args: Strings::new(args),
            vars: Strings::new(vars),
        }
    }
    fn exec(&self) -> io::Error {
        // Also used after fork: no allocation, PATH lookup, or shell fallback.
        unsafe {
            libc::execve(
                self.path.as_ptr(),
                self.args.pointers.as_ptr(),
                self.vars.pointers.as_ptr(),
            );
        }
        io::Error::last_os_error()
    }
}
async fn run() -> Result<ExitCode> {
    let mut args = env::args_os().skip(1);
    let first = args.next();
    let mut login_profile = None;
    let login_tool: String;
    let deadline = Instant::now() + Duration::from_secs(5);
    let (tool, argv) = match first
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
        Some(t @ ("claude" | "codex" | "agy")) => (
            t,
            args.map(OsString::into_string)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| utf8())?,
        ),
        Some(op @ ("login" | "usage")) => {
            let syntax = if op == "login" {
                "Usage: ccp login <profile-id> [--status]"
            } else {
                "Usage: ccp usage <antigravity-profile-id>"
            };
            login_profile = args
                .next()
                .and_then(|s| s.to_str()?.parse::<i64>().ok())
                .filter(|id| *id > 0);
            let Some(id) = login_profile else {
                eprintln!("{syntax}");
                return Ok(ExitCode::from(2));
            };
            let status = match args.next() {
                None => false,
                Some(s) if op == "login" && s == "--status" && args.next().is_none() => true,
                _ => {
                    eprintln!("{syntax}");
                    return Ok(ExitCode::from(2));
                }
            };
            login_tool = request("/v2/profile-tool", id.to_string().as_bytes(), deadline).await?;
            let native = if op == "usage" {
                if login_tool != "agy" {
                    return Err(Error::bad(
                        "unsupported_tool",
                        "Use the Dashboard quota meters for Claude and Codex.",
                    ));
                }
                ccpd::agy::USAGE
            } else {
                profile_login_args(&login_tool, status).ok_or_else(invalid)?
            };
            (
                login_tool.as_str(),
                native.iter().map(|s| s.to_string()).collect(),
            )
        }
        _ => {
            eprintln!("{HELP}");
            return Ok(ExitCode::from(2));
        }
    };
    let binary = executable(tool);
    if let Some(path) = &binary {
        if let (Ok(own), Ok(native)) = (fs::metadata("/proc/self/exe"), fs::metadata(path))
            && (native.dev(), native.ino()) == (own.dev(), own.ino())
        {
            eprintln!("ccp: native command resolves to the launcher.");
            return Ok(ExitCode::from(2));
        }
        path.to_str().ok_or_else(utf8)?;
    }
    let cwd = env::current_dir()?;
    cwd.to_str().ok_or_else(utf8)?;
    let names = if tool == "agy" {
        ccpd::agy::OVERRIDES
    } else if tool == "claude" {
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
        login_profile,
        argv,
        cwd,
        binary,
        override_names,
    };
    let body = serde_json::to_vec(&req).map_err(|_| utf8())?;
    let plan = request("/v2/launch", &body, deadline).await?;
    let (home, managed) = validate(&plan, &req)?;
    let binary = req
        .binary
        .as_deref()
        .ok_or_else(|| Error::bad("binary", "Native executable not found on PATH."))?;
    eprintln!("ccp: {tool}/{} -> {home}", plan.label);
    if tool == "agy" {
        let result = acp::run(binary, Path::new(home), &req, &plan).await;
        if result.is_err() {
            record_failure(&req, libc::EIO).await;
        }
        return result;
    }
    if let Some(i) = managed {
        match codex::run(binary, &req, &plan, &i).await {
            Ok(code) => return Ok(code),
            Err(e) => {
                record_failure(&req, libc::EIO).await;
                return Err(e);
            }
        }
    }
    let error = NativeExec::new(binary, tool, &plan.argv, &plan).exec();
    let errno = error.raw_os_error().unwrap_or(libc::EIO);
    record_failure(&req, errno).await;
    eprintln!("ccp: exec failed: {error}");
    Ok(ExitCode::from(if errno == libc::ENOENT {
        127
    } else {
        126
    }))
}
async fn record_failure(req: &Request, errno: i32) {
    let failure = Failure {
        request_id: req.request_id.clone(),
        ticket: req.ticket.clone(),
        errno,
    };
    if let Ok(body) = serde_json::to_vec(&failure) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            socket::post("/v2/exec-failure", &body),
        )
        .await;
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    run().await.unwrap_or_else(|e| {
        eprintln!("ccp: {e}");
        ExitCode::FAILURE
    })
}
