use crate::types::{Error, Result};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn env_dir(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}
pub fn state_dir() -> Result<PathBuf> {
    env_dir("XDG_STATE_HOME")
        .or_else(|| env_dir("HOME").map(|p| p.join(".local/state")))
        .map(|p| p.join("cc-profile"))
        .ok_or_else(Error::denied)
}
pub fn runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = env_dir("XDG_RUNTIME_DIR") {
        Ok(dir.join("cc-profile"))
    } else {
        Ok(state_dir()?.join("runtime"))
    }
}
pub fn profile_root(tool: &str) -> Result<PathBuf> {
    let tool = crate::types::tool(tool)?;
    let data = env_dir("XDG_DATA_HOME")
        .or_else(|| env_dir("HOME").map(|p| p.join(".local/share")))
        .ok_or_else(Error::denied)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&data)?;
    Ok(data.canonicalize()?.join("cc-profile/profiles").join(tool))
}
pub fn private_dir(p: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)?;
    let m = fs::symlink_metadata(p)?;
    if !m.is_dir() || p.canonicalize()? != p || m.uid() != uid() || m.mode() & 0o077 != 0 {
        return Err(Error::bad(
            "unsafe_directory",
            format!("Expected a private user-owned directory: {}", p.display()),
        ));
    }
    Ok(())
}
pub fn locked(p: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(Error::conflict(
            "already_running",
            "A ccpd instance already owns this store or listener.",
        ));
    }
    Ok(f)
}
pub fn secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn read_limited(path: &Path, max: usize) -> Result<Vec<u8>> {
    let f = File::open(path)?;
    if !f.metadata()?.is_file() {
        return Err(Error::bad("bad_file", "Expected a regular file."));
    }
    let mut bytes = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(Error::bad("too_large", "File exceeds size limit."));
    }
    Ok(bytes)
}
pub fn replace(path: &Path, data: &[u8], expected: Option<&[u8]>) -> Result<()> {
    let tmp = path.with_file_name(format!(".ccp-{}", Uuid::new_v4()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        f.write_all(data)?;
        f.sync_all()?;
        match fs::symlink_metadata(path) {
            Ok(m)
                if m.is_file()
                    && expected.is_some()
                    && Some(read_limited(path, 1 << 20)?.as_slice()) == expected =>
            {
                fs::rename(&tmp, path)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && expected.is_none() => {
                fs::hard_link(&tmp, path)?;
            }
            _ => {
                return Err(Error::conflict(
                    "file_changed",
                    "Target changed, is a symlink, or is not a regular file.",
                ));
            }
        }
        File::open(path.parent().ok_or_else(Error::denied)?)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(tmp);
    result
}
