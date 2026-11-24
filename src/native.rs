use crate::{
    store::Config,
    types::{Error, Result},
};
use std::{
    collections::BTreeMap,
    env,
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::Command;

pub const PROXIES: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];
pub fn env_patch(c: &Config) -> (BTreeMap<String, String>, Vec<String>) {
    let mut set = BTreeMap::from([(
        if c.tool == "agy" {
            "HOME"
        } else if c.tool == "codex" {
            "CODEX_HOME"
        } else {
            "CLAUDE_CONFIG_DIR"
        }
        .to_owned(),
        c.home.to_string_lossy().into_owned(),
    )]);
    let mut unset = Vec::new();
    if c.mode != "inherit" {
        for key in PROXIES {
            let val = match key.to_ascii_uppercase().as_str() {
                "HTTP_PROXY" | "HTTPS_PROXY" if c.mode == "custom" => c.proxy_url.as_deref(),
                "NO_PROXY" => Some(if c.mode == "direct" {
                    "*"
                } else {
                    "localhost,127.0.0.1,::1"
                }),
                _ => None,
            };
            if let Some(v) = val {
                set.insert(key.into(), v.into());
            } else {
                unset.push(key.into())
            }
        }
    }
    (set, unset)
}
pub fn stamp(path: &Path) -> String {
    fs::metadata(path)
        .map(|m| {
            format!(
                "{}:{}:{}:{}:{}",
                m.dev(),
                m.ino(),
                m.len(),
                m.ctime(),
                m.ctime_nsec()
            )
        })
        .unwrap_or_default()
}
fn executable_paths(name: &str) -> impl Iterator<Item = PathBuf> {
    let paths = if name.contains('/') {
        vec![PathBuf::from(name)]
    } else {
        let search = env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into());
        env::split_paths(&search)
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join(name))
            .collect()
    };
    paths.into_iter().filter_map(|p| {
        let p = p.canonicalize().ok()?;
        (p.is_file()
            && CString::new(p.as_os_str().as_bytes()).is_ok_and(|path| unsafe {
                libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0
            }))
        .then_some(p)
    })
}
type FileId = (u64, u64);
fn file_id(path: &Path) -> Option<FileId> {
    fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}
fn running_exe() -> Option<PathBuf> {
    // Keep the running inode even if an upgrade replaced or unlinked its pathname.
    #[cfg(target_os = "linux")]
    if Path::new("/proc/self/exe").exists() {
        return Some("/proc/self/exe".into());
    }
    env::current_exe().ok()
}
fn launcher_files() -> Result<Vec<FileId>> {
    let exe = running_exe();
    let own = exe.as_deref().and_then(file_id).ok_or_else(|| {
        Error::bad(
            "executable_identity",
            "Cannot identify the running executable; refusing native lookup.",
        )
    })?;
    let mut ids = vec![own];
    if let Some(path) = exe.and_then(|p| p.canonicalize().ok())
        && path.file_name().is_some_and(|n| n == "ccpd")
        && let Some(launcher) = file_id(&path.with_file_name("ccp"))
    {
        ids.push(launcher);
    }
    Ok(ids)
}
fn native_candidates(name: &str) -> Result<impl Iterator<Item = (PathBuf, bool)>> {
    let launchers = launcher_files()?;
    Ok(executable_paths(name).filter_map(move |path| {
        let alias = launchers.contains(&file_id(&path)?);
        Some((path, alias))
    }))
}
fn alias_error(tool: &str, p: &Path) -> Error {
    Error::bad(
        "launcher_alias",
        format!(
            "Found a launcher alias ({}), not a native {tool} executable. Check PATH or the configured binary.",
            p.display()
        ),
    )
}
pub fn native_executable(tool: &str) -> Result<Option<PathBuf>> {
    let mut alias = None;
    for (p, is_alias) in native_candidates(tool)? {
        if !is_alias {
            return Ok(Some(p));
        }
        alias = Some(p);
    }
    match alias {
        Some(p) => Err(alias_error(tool, &p)),
        None => Ok(None),
    }
}

const VERSION_PROBE: &str = "CCP_NATIVE_VERSION_PROBE";
const PROBE_REPLY: &str = "ccp launcher ";
pub fn version_probe_reply() -> Option<String> {
    (env::var_os(VERSION_PROBE).as_deref() == Some("1".as_ref())).then(|| {
        let id = running_exe().map(|p| stamp(&p)).unwrap_or_default();
        format!("{PROBE_REPLY}{id}")
    })
}
pub fn clean_command(bin: &Path, cwd: &Path, c: Option<&Config>) -> Command {
    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for k in [
        "HOME",
        "PATH",
        "USER",
        "LANG",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(v) = env::var_os(k) {
            cmd.env(k, v);
        }
    }
    if let Some(c) = c {
        if c.mode == "inherit" {
            for k in PROXIES {
                if let Some(v) = env::var_os(k) {
                    cmd.env(k, v);
                }
            }
        }
        cmd.envs(env_patch(c).0);
    }
    cmd
}
#[derive(Clone, Default)]
pub struct Build {
    pub bin: PathBuf,
    pub fingerprint: String,
    pub version: String,
    pub supported: bool,
}
enum Probe {
    Version(String),
    Launcher,
    Forwarder,
}
async fn probe(bin: &Path, cwd: &Path, fingerprint: &str) -> Probe {
    let Ok(Ok(out)) = tokio::time::timeout(
        Duration::from_secs(5),
        clean_command(bin, cwd, None)
            .env(VERSION_PROBE, "1")
            .arg("--version")
            .output(),
    )
    .await
    else {
        return Probe::Version(String::new());
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let version = stdout.trim();
    // A split installation can hit an unfamiliar launcher. Only skip the
    // probed file itself, never a script that called that launcher.
    if out.status.success()
        && !fingerprint.is_empty()
        && version.strip_prefix(PROBE_REPLY) == Some(fingerprint)
    {
        Probe::Launcher
    } else if version.starts_with("ccp ") || stdout.lines().any(|l| l.starts_with(PROBE_REPLY)) {
        Probe::Forwarder
    } else if out.status.success() {
        Probe::Version(version.chars().take(120).collect())
    } else {
        Probe::Version(String::new())
    }
}
type Candidates = Vec<(PathBuf, bool, String)>;

pub struct Installed {
    tool: &'static str,
    configured: Option<String>,
    cached: tokio::sync::Mutex<Option<(Candidates, Build, Instant)>>,
}
impl Installed {
    pub fn new(tool: &'static str, configured: Option<String>) -> Self {
        Self {
            tool,
            configured,
            cached: Default::default(),
        }
    }
    pub async fn current(&self, cwd: &Path) -> Build {
        self.resolve(cwd, false).await
    }
    pub async fn resolve(&self, cwd: &Path, force_probe: bool) -> Build {
        let mut cache = self.cached.lock().await;
        let candidates: Candidates =
            match native_candidates(self.configured.as_deref().unwrap_or(self.tool)) {
                Ok(paths) => paths
                    .map(|(p, alias)| {
                        let fp = stamp(&p);
                        (p, alias, fp)
                    })
                    .collect(),
                Err(e) => {
                    eprintln!("ccpd: {e}");
                    return Build::default();
                }
            };
        if !force_probe
            && let Some((previous, build, checked)) = &*cache
            && previous == &candidates
            && (build.supported || checked.elapsed() < Duration::from_secs(5))
        {
            return build.clone();
        }
        let build = self.detect(&candidates, cwd).await;
        *cache = Some((candidates, build.clone(), Instant::now()));
        build
    }
    async fn detect(&self, candidates: &Candidates, cwd: &Path) -> Build {
        let mut alias = None;
        let tool = self.tool;
        for (bin, known_alias, fingerprint) in candidates {
            if !*known_alias {
                match probe(bin, cwd, fingerprint).await {
                    Probe::Version(v) => {
                        return Build::from_version(tool, bin.clone(), fingerprint.clone(), v);
                    }
                    Probe::Forwarder => {
                        eprintln!(
                            "ccpd: {tool} at {} invokes the launcher, not a native executable.",
                            bin.display()
                        );
                        return Build::default();
                    }
                    Probe::Launcher => {}
                }
            }
            alias = Some(bin);
            if self.configured.is_some() {
                break;
            }
        }
        if let Some(p) = alias {
            eprintln!("ccpd: {}", alias_error(tool, p));
        }
        Build::default()
    }
}
impl Build {
    fn from_version(tool: &str, bin: PathBuf, fingerprint: String, version: String) -> Self {
        if version.is_empty() {
            eprintln!(
                "ccpd: {tool} found at {} but --version failed. Check execute permission and the service PATH (npm installations need node).",
                bin.display()
            );
        }
        // Identify the CLI's version report, not a particular release number.
        let release = match tool {
            // Some Claude packages report the bare version without the product label.
            "claude" => Some(version.strip_suffix(" (Claude Code)").unwrap_or(&version)),
            "codex" => version
                .strip_prefix("codex-cli ")
                .and_then(|s| s.split_whitespace().next()),
            "agy" => Some(version.strip_prefix("agy ").unwrap_or(&version)),
            _ => None,
        };
        let supported = release.is_some_and(|v| {
            let v = v.strip_prefix('v').unwrap_or(v);
            v.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && v.contains('.')
                && !v.contains(char::is_whitespace)
        });
        Self {
            bin,
            fingerprint,
            version,
            supported,
        }
    }
    pub fn unchanged(&self) -> bool {
        !self.fingerprint.is_empty() && stamp(&self.bin) == self.fingerprint
    }
    pub fn ready(&self, c: &Config) -> &'static str {
        if !c.present() {
            "path_missing"
        } else if !self.unchanged() || !self.supported {
            "unsupported_build"
        } else {
            "ready"
        }
    }
}
pub fn unsupported_build() -> Error {
    Error::conflict(
        "unsupported_build",
        "Native tool is unavailable or does not match the daemon's executable. Check Binary versions in Settings and the native binary on PATH.",
    )
}
pub fn validate_binary(build: &Build, reported: Option<&Path>) -> Result<()> {
    if !build.supported || reported != Some(build.bin.as_path()) || !build.unchanged() {
        return Err(unsupported_build());
    }
    Ok(())
}
