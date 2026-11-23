use rustix::fs::{CWD, Dir, Mode, OFlags, openat};
use std::{
    io,
    os::fd::OwnedFd,
    path::{Component, Path},
};

pub(crate) fn dir_at(parent: &OwnedFd, name: impl rustix::path::Arg) -> io::Result<OwnedFd> {
    Ok(openat(
        parent,
        name,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::PATH,
        Mode::empty(),
    )?)
}

pub(crate) fn open_home(home: &Path) -> io::Result<OwnedFd> {
    let mut parts = home.components();
    if parts.next() != Some(Component::RootDir) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut dir = openat(
        CWD,
        "/",
        OFlags::DIRECTORY | OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    for part in parts {
        let Component::Normal(name) = part else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        dir = dir_at(&dir, name)?;
    }
    Ok(dir)
}

pub(crate) fn entries(dir: &OwnedFd) -> rustix::io::Result<Dir> {
    Dir::new(openat(
        dir,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}
