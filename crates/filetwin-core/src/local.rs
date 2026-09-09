use crate::{Error, ErrorCode, Result, api::Filters, profile::digest_hex};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    pub fn file_id(&self, namespace: &str) -> String {
        let incarnation = self
            .birth
            .clone()
            .unwrap_or_else(|| format!("ctime:{}:{}", self.ctime, self.ctime_ns));
        format!(
            "file_{}",
            digest_hex(
                format!("{namespace}:{}:{}:{incarnation}", self.device, self.inode).as_bytes()
            )
        )
    }
    pub fn identity_kind(&self) -> &str {
        if self.birth.is_some() {
            "device_inode_birthtime"
        } else {
            "device_inode_ctime_fallback"
        }
    }
}

pub(crate) fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}
pub(crate) fn from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes))
}
pub(crate) fn locator(path: &Path) -> Value {
    match path.to_str() {
        Some(p) => json!({"provider":"local","root":p}),
        None => {
            json!({"provider":"local","display_path":path.to_string_lossy(),"local_path":{"encoding":"posix_bytes","base64":STANDARD.encode(path_bytes(path))}})
        }
    }
}
pub(crate) fn location_id(path: &Path) -> String {
    format!("location_{}", digest_hex(path_bytes(path)))
}
pub(crate) fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    if pattern.contains(['[', ']', '{', '}', '\\']) {
        return Err(Error::invalid(
            "The glob dialect supports only literals, *, ?, and **",
        ));
    }
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|e| Error::invalid(e.to_string()))?
        .compile_matcher())
}
pub(crate) struct FilterSet {
    include: Vec<GlobMatcher>,
    exclude: Vec<GlobMatcher>,
    filters: Filters,
}
impl FilterSet {
    pub fn new(filters: Filters) -> Result<Self> {
        Ok(Self {
            include: filters
                .include_globs
                .iter()
                .map(|g| compile_glob(g))
                .collect::<Result<_>>()?,
            exclude: filters
                .exclude_globs
                .iter()
                .map(|g| compile_glob(g))
                .collect::<Result<_>>()?,
            filters,
        })
    }
    pub fn hidden_excluded(&self, path: &Path) -> bool {
        !self.filters.include_hidden
            && path
                .components()
                .any(|c| c.as_os_str().as_bytes().starts_with(b"."))
    }
    pub fn matches(&self, relative: &Path, bytes: u64) -> Result<bool> {
        if self.hidden_excluded(relative)
            || self.filters.min_bytes.is_some_and(|v| bytes < v)
            || self.filters.max_bytes.is_some_and(|v| bytes > v)
        {
            return Ok(false);
        }
        if (!self.include.is_empty()
            || !self.exclude.is_empty()
            || !self.filters.extensions.is_empty())
            && relative.to_str().is_none()
        {
            return Err(Error::new(
                ErrorCode::UnsupportedCapability,
                "filter",
                "Name filter cannot evaluate a non-Unicode path losslessly",
            ));
        }
        if self.exclude.iter().any(|g| g.is_match(relative))
            || (!self.include.is_empty() && !self.include.iter().any(|g| g.is_match(relative)))
        {
            return Ok(false);
        }
        Ok(self.filters.extensions.is_empty()
            || relative
                .extension()
                .and_then(|v| v.to_str())
                .is_some_and(|ext| {
                    self.filters
                        .extensions
                        .iter()
                        .any(|v| v.eq_ignore_ascii_case(ext))
                }))
    }
}

pub(crate) fn excluded_artifact(path: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| path.starts_with(d))
        || path.file_name().is_some_and(|n| {
            n.as_bytes().ends_with(b".filetwin.json")
                || n.as_bytes().starts_with(b".filetwin-export-")
        })
        || is_report_directory(path)
}

fn is_report_directory(path: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = open_secure(&path.join("filetwin-export.json")) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(65537).read_to_end(&mut bytes).is_err() || bytes.len() > 65536 {
        return false;
    }
    serde_json::from_slice::<Value>(&bytes)
        .is_ok_and(|v| v["product"] == "filetwin" && v["artifact_kind"] == "report")
}

/// Atomically publish a report without replacing an existing destination,
/// including one created after the initial destination check.
pub(crate) fn rename_new(from: &Path, to: &Path) -> Result<()> {
    let from = CString::new(path_bytes(from)).map_err(|_| Error::invalid("NUL in export path"))?;
    let to = CString::new(path_bytes(to)).map_err(|_| Error::invalid("NUL in export path"))?;
    #[cfg(target_os = "macos")]
    // SAFETY: both paths are valid NUL-terminated strings; RENAME_EXCL forbids replacement.
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    // SAFETY: both paths are absolute, valid C strings; RENAME_NOREPLACE forbids replacement.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
