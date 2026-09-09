use crate::{Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString, OsString},
    fs::{File, Metadata},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::MetadataExt,
        },
    },
    path::{Component, Path, PathBuf},
};

/// Resolve aliases in the explicitly supplied root's parent once (for example,
/// macOS /var -> /private/var). The root itself and all discovered descendants
/// are subsequently opened with NOFOLLOW; a symlink root is never accepted.
pub(crate) fn resolve_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::invalid("Expected an absolute source root"));
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok(std::fs::canonicalize(parent)?.join(name)),
        _ => Ok(std::fs::canonicalize(path)?),
    }
}

/// Open every path component relative to its already-open parent with NOFOLLOW.
/// NONBLOCK prevents a raced FIFO/device substitution from blocking before fstat.
pub(crate) fn open_secure(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(Error::invalid("Expected an absolute path"));
    }
    let components: Vec<_> = path
        .components()
        .filter(|c| !matches!(c, Component::RootDir | Component::CurDir))
        .collect();
    let mut dir = File::open("/")?;
    for (i, c) in components.iter().enumerate() {
        let name =
            CString::new(c.as_os_str().as_bytes()).map_err(|_| Error::invalid("NUL in path"))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if i + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: the parent descriptor is live, name is NUL-terminated, and openat
        // returns a newly owned descriptor; O_CREAT is not used.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fd was freshly returned by successful openat and is uniquely owned.
        dir = unsafe { File::from_raw_fd(fd) };
    }
    Ok(dir)
}

pub(crate) struct SecureDir {
    ptr: *mut libc::DIR,
}
impl SecureDir {
    pub fn open(path: &Path) -> Result<Self> {
        let file = open_secure(path)?;
        if !file.metadata()?.is_dir() {
            return Err(Error::new(
                ErrorCode::SourceChanged,
                "discovery",
                "Directory changed type",
            ));
        }
        let fd = file.into_raw_fd();
        // SAFETY: fd is an owned readable directory descriptor. fdopendir takes
        // ownership on success; on failure we close it below.
        let ptr = unsafe { libc::fdopendir(fd) };
        if ptr.is_null() {
            let e = std::io::Error::last_os_error();
            // SAFETY: fdopendir failed, so fd remains uniquely owned here.
            unsafe { libc::close(fd) };
            return Err(e.into());
        }
        Ok(Self { ptr })
    }
}
impl Iterator for SecureDir {
    type Item = Result<OsString>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // SAFETY: errno_location returns this thread's valid errno pointer.
            unsafe {
                *errno_location() = 0;
            }
            // SAFETY: ptr is an open DIR exclusively borrowed through &mut self.
            let entry = unsafe { libc::readdir(self.ptr) };
            if entry.is_null() {
                // SAFETY: errno_location returns this thread's valid errno pointer.
                let errno = unsafe { *errno_location() };
                return if errno == 0 {
                    None
                } else {
                    Some(Err(std::io::Error::from_raw_os_error(errno).into()))
                };
            }
            // SAFETY: readdir returned a live dirent with a NUL-terminated d_name;
            // the name is copied before another readdir call can invalidate it.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                return Some(Ok(OsString::from_vec(name.to_vec())));
            }
        }
    }
}
impl Drop for SecureDir {
    fn drop(&mut self) {
        // SAFETY: this object owns the DIR and drops it exactly once.
        unsafe { libc::closedir(self.ptr) };
    }
}
#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: platform libc exposes the current thread's errno location.
    unsafe { libc::__error() }
}
#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: platform libc exposes the current thread's errno location.
    unsafe { libc::__errno_location() }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Revision {
    pub device: u64,
    pub inode: u64,
    pub bytes: u64,
    pub mtime: i64,
    pub mtime_ns: i64,
    pub ctime: i64,
    pub ctime_ns: i64,
    pub birth: Option<String>,
}
impl Revision {
    pub fn of(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            bytes: meta.len(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_ns: meta.ctime_nsec(),
            birth: meta
                .created()
                .ok()
                .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|v| format!("{}:{}", v.as_secs(), v.subsec_nanos())),
        }
    }
    pub fn key(&self) -> String {
        serde_json::to_string(self).expect("Revision is serializable")
    }
}

pub(crate) fn still_current(file: &File, path: &Path, before: &Revision) -> bool {
    file.metadata()
        .ok()
        .filter(|m| m.is_file())
        .is_some_and(|m| Revision::of(&m).key() == before.key())
        && open_secure(path)
            .and_then(|f| Ok(f.metadata()?))
            .ok()
            .is_some_and(|m| m.is_file() && Revision::of(&m).key() == before.key())
}
