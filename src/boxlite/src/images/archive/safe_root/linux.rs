//! Linux: pathrs resolve with walk fallback for non-existent paths.

use super::path;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};

pub(super) struct Backend {
    pathrs_root: Option<pathrs::Root>,
    root_path: PathBuf,
}

impl Backend {
    pub(super) fn open(root: &Path) -> BoxliteResult<Self> {
        Self::open_with_faccessat2(root, faccessat2_available())
    }

    fn open_with_faccessat2(root: &Path, is_available: bool) -> BoxliteResult<Self> {
        // pathrs 0.2.x recursively formats its procfs dirfd when faccessat2 is
        // unavailable. Use the existing userspace resolver on older kernels.
        let inner = if is_available {
            Some(pathrs::Root::open(root).map_err(|e| {
                BoxliteError::Storage(format!("pathrs open {}: {}", root.display(), e))
            })?)
        } else {
            tracing::debug!("faccessat2 unavailable; using userspace path resolver");
            None
        };
        Ok(Self {
            pathrs_root: inner,
            root_path: root.to_path_buf(),
        })
    }

    pub(super) fn resolve(&self, rel: &Path) -> BoxliteResult<PathBuf> {
        let Some(root) = &self.pathrs_root else {
            return path::resolve_walk(&self.root_path, rel);
        };

        match root.resolve(rel) {
            Ok(handle) => {
                let fd = handle.as_fd();
                let proc_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
                std::fs::read_link(&proc_path).map_err(|e| {
                    BoxliteError::Storage(format!(
                        "readlink /proc/self/fd for {}: {}",
                        rel.display(),
                        e
                    ))
                })
            }
            Err(e)
                if e.kind() == pathrs::error::ErrorKind::OsError(Some(libc::ENOENT))
                    || e.kind() == pathrs::error::ErrorKind::OsError(Some(libc::ENOTDIR)) =>
            {
                // Path doesn't fully exist — fall back to walk algorithm.
                path::resolve_walk(&self.root_path, rel)
            }
            Err(e) => Err(BoxliteError::Storage(format!(
                "pathrs resolve {}: {}",
                rel.display(),
                e
            ))),
        }
    }
}

fn faccessat2_available() -> bool {
    const DOT: &[u8] = b".\0";

    // SAFETY: DOT is a valid NUL-terminated path and the remaining arguments
    // are constants accepted by faccessat2(2). No memory is written.
    unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            libc::AT_FDCWD,
            DOT.as_ptr().cast::<libc::c_char>(),
            libc::F_OK,
            libc::AT_SYMLINK_NOFOLLOW,
        ) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_fallback_reanchors_absolute_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root_path = tmp.path().join("root");
        std::fs::create_dir_all(root_path.join("real")).unwrap();
        std::os::unix::fs::symlink("/real", root_path.join("link")).unwrap();

        let backend = Backend::open_with_faccessat2(&root_path, false).unwrap();

        assert_eq!(
            backend.resolve(Path::new("link/file")).unwrap(),
            root_path.join("real/file")
        );
    }
}
