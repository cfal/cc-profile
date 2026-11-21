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
    time::Duration,
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
pub const MARKERS: [&str; 2] = ["CCP_LAUNCH_ID", "CCP_OBSERVE_TICKET"];
pub fn env_patch(c: &Config) -> (BTreeMap<String, String>, Vec<String>) {
    let mut set = BTreeMap::from([(
        if c.tool == "codex" {
            "CODEX_HOME"
        } else {
            "CLAUDE_CONFIG_DIR"
        }
        .to_owned(),
        c.home.to_string_lossy().into_owned(),
    )]);
    let mut unset = MARKERS.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if c.mode != "inherit" {
        for key in PROXIES {
            let val = match key.to_ascii_uppercase().as_str() {
                "HTTP_PROXY" if c.mode == "custom" => c.http.as_deref(),
                "HTTPS_PROXY" if c.mode == "custom" => c.https.as_deref(),
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
pub fn executable(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.components().count() > 1 {
        p.canonicalize().ok()
    } else {
        let search = env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into());
        if search.is_empty() {
            return None;
        }
        env::split_paths(&search)
            .map(|p| p.join(name))
            .find(|p| {
                p.is_file()
                    && CString::new(p.as_os_str().as_bytes()).is_ok_and(|path| unsafe {
                        libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS)
                            == 0
                    })
            })
            .and_then(|p| p.canonicalize().ok())
    }
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
impl Build {
    pub async fn detect(tool: &str, name: &str, cwd: &Path) -> Self {
        let Some(bin) = executable(name) else {
            return Self::default();
        };
        let fingerprint = stamp(&bin);
        let version = match tokio::time::timeout(
            Duration::from_secs(5),
            clean_command(&bin, cwd, None).arg("--version").output(),
        )
        .await
        {
            Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .trim()
                .chars()
                .take(120)
                .collect(),
            _ => String::new(),
        };
        if version.is_empty() {
            eprintln!(
                "ccpd: {tool} found at {} but --version failed. Check execute permission and the service PATH (npm installations need node).",
                bin.display()
            );
        }
        let supported = match tool {
            "claude" => version.split_whitespace().next() == Some("2.1.329"),
            _ => version == "codex-cli 0.171.0",
        };
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
    pub fn observer_stamp(&self, c: &Config) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.fingerprint,
            stamp(&c.home.join("hooks.json")),
            stamp(&c.home.join("config.toml")),
            stamp(Path::new("/etc/codex/config.toml")),
            stamp(Path::new("/etc/codex/managed_config.toml")),
            stamp(Path::new("/etc/codex/requirements.toml")),
            crate::observer::binary()
                .map(|b| b.signature())
                .unwrap_or_default()
        )
    }
    pub fn ready(&self, c: &Config) -> &'static str {
        if !c.present() {
            "path_missing"
        } else if !self.unchanged()
            || !self.supported
            || (c.tool == "codex" && !crate::observer::binary().is_ok_and(|b| b.unchanged()))
        {
            "unsupported_build"
        } else if c.tool == "codex" && c.observer.as_ref() != Some(&self.observer_stamp(c)) {
            "observer_required"
        } else {
            "ready"
        }
    }
}
pub fn unsupported_build() -> Error {
    Error::conflict(
        "unsupported_build",
        "Native tool is missing, changed, or not a verified version. Check Accounts and restart ccpd after updating.",
    )
}
pub fn validate_binary(build: &Build, reported: Option<&Path>) -> Result<()> {
    if reported.is_none() || reported != Some(build.bin.as_path()) || !build.unchanged() {
        return Err(unsupported_build());
    }
    Ok(())
}
