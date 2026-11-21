use crate::{
    store::Config,
    types::{Error, Result},
};
use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::MetadataExt,
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
        env::split_paths(&env::var_os("PATH")?)
            .map(|p| p.join(name))
            .find(|p| p.is_file() && fs::metadata(p).is_ok_and(|m| m.mode() & 0o111 != 0))
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
            "{}|{}|{}|{}|{}",
            self.fingerprint,
            stamp(&c.home.join("hooks.json")),
            stamp(&c.home.join("config.toml")),
            stamp(&PathBuf::from("/etc/codex/requirements.toml")),
            env::current_exe().map(|p| stamp(&p)).unwrap_or_default()
        )
    }
    pub fn ready(&self, c: &Config) -> &'static str {
        if !c.present() {
            "path_missing"
        } else if !self.unchanged() || !self.supported {
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
