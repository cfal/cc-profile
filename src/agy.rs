use crate::saved_files::{dir_at, open_home};
use crate::{
    paths,
    types::{Error, Result},
};
use rustix::fs::{AtFlags, FileType, statat};
use std::{
    fs, io,
    os::fd::OwnedFd,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};
use tokio::process::Command;
pub mod acp;
pub mod discovery;

pub const APP_DIR: &str = "antigravity-cli";
pub const STATUS: &[&str] = &["profile-status"];
pub const PROFILE_USAGE: &[&str] = &["profile-usage"];
pub const OVERRIDES: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GEMINI_BASE_URL",
    "AGY_ADC_AUTH",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_QUOTA_PROJECT",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "CLOUDSDK_CONFIG",
    "ANTIGRAVITY_CREDENTIAL_SCOPE",
    "ANTIGRAVITY_GEMINI_DIR",
    "ANTIGRAVITY_APP_DATA_DIR",
    "ANTIGRAVITY_CONFIG_REL_DIR",
];

pub fn usage_args(json: bool) -> Vec<String> {
    let mut args = Vec::new();
    if json {
        args.extend(["--output-format", "json"]);
    }
    args.extend([
        "--settings",
        r#"{"disableSlashCommands":false}"#,
        "-p",
        "/usage",
    ]);
    args.into_iter().map(String::from).collect()
}

pub fn session_id(id: &str) -> Result<&str> {
    if id.is_empty() || id.len() > 512 || id.trim() != id || id.chars().any(char::is_control) {
        return Err(Error::bad(
            "bad_acp_id",
            "Expected an exact ACP session ID (up to 512 bytes).",
        ));
    }
    Ok(id)
}

fn conversation_dir(home: &OwnedFd) -> io::Result<OwnedFd> {
    let gemini = dir_at(home, ".gemini")?;
    let app = dir_at(&gemini, APP_DIR)?;
    dir_at(&app, "conversations")
}
fn saved_in(dir: &OwnedFd, id: uuid::Uuid) -> io::Result<bool> {
    let mut found = false;
    for ext in ["db", "pb"] {
        match statat(dir, format!("{id}.{ext}"), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(m)
                if FileType::from_raw_mode(m.st_mode) == FileType::RegularFile
                    && m.st_uid == paths::uid()
                    && m.st_nlink == 1 =>
            {
                found |= m.st_size > 0;
            }
            Err(rustix::io::Errno::NOENT) => {}
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
    }
    Ok(found)
}
/// Exact resumes recheck both native suffixes without opening either file.
pub fn has_conversation(home: &Path, id: uuid::Uuid) -> Result<bool> {
    match open_home(home)
        .and_then(|home| conversation_dir(&home))
        .and_then(|dir| saved_in(&dir, id))
    {
        Ok(found) => Ok(found),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(Error::bad(
            "unsafe_conversation",
            "Cannot verify the native conversation path.",
        )),
    }
}

pub fn has_login(home: &Path) -> Result<bool> {
    Ok(login_stamp(home)?.is_some())
}

#[derive(PartialEq, Eq, serde::Serialize)]
pub struct LoginStamp(u64, u64, u64, i64, i64);

pub fn login_stamp(home: &Path) -> Result<Option<LoginStamp>> {
    let unsafe_home = || {
        Error::bad(
            "unsafe_agy_home",
            "Antigravity requires a private, user-owned profile home and token file without symlinks.",
        )
    };
    for dir in [
        home.to_owned(),
        home.join(".config"),
        home.join(".gemini"),
        home.join(".gemini/config"),
        home.join(".gemini").join(APP_DIR),
    ] {
        match fs::symlink_metadata(&dir) {
            Ok(m)
                if m.is_dir()
                    && m.uid() == paths::uid()
                    && m.mode() & 0o077 == 0
                    && dir.canonicalize()? == dir => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir != home => continue,
            _ => return Err(unsafe_home()),
        }
    }
    match fs::symlink_metadata(
        home.join(".gemini")
            .join(APP_DIR)
            .join("antigravity-oauth-token"),
    ) {
        Ok(m)
            if m.is_file()
                && m.nlink() == 1
                && m.uid() == paths::uid()
                && m.mode() & 0o077 == 0 =>
        {
            Ok((m.len() != 0)
                .then(|| LoginStamp(m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        _ => Err(unsafe_home()),
    }
}

/// Each native child needs a bus-less runtime, not just a distinct Gemini root.
pub struct Runtime(PathBuf);
impl Runtime {
    pub fn new() -> Result<Self> {
        let base = paths::scratch_dir()?;
        paths::private_dir(&base)?;
        let path = base.join(format!("agy-{}", uuid::Uuid::new_v4().simple()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
    pub fn configure(&self, cmd: &mut Command, home: &Path, args: &[String]) {
        for key in [
            "DBUS_SESSION_BUS_ADDRESS",
            "DBUS_STARTER_ADDRESS",
            "DBUS_STARTER_BUS_TYPE",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "CCP_LAUNCH_ID",
            "CCP_OBSERVE_TICKET",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("HOME", home)
            .env("XDG_RUNTIME_DIR", &self.0)
            // Declare the intended backend as well; 1.5.14 still needs the bus-less runtime.
            .env("AGY_ACP_FORCE_FILE_STORAGE", "1")
            .args(args);
        if args.first().is_some_and(|s| s == "acp") {
            cmd.arg("--gemini-dir")
                .arg(home.join(".gemini"))
                .arg("--app-data-dir")
                .arg(APP_DIR);
        }
        let parent = std::process::id();
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(0o077);
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() as u32 != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::EINTR));
                }
                Ok(())
            });
        }
        cmd.kill_on_drop(true);
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
