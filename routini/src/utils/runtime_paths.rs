//! Locations for the PID file and the graceful upgrade socket that other local users cannot
//! tamper with.
//!
//! On SIGQUIT the old process connects to the upgrade socket and hands it its *listening
//! sockets*. If another user can create that socket first (e.g. in `/tmp`), they receive the
//! proxy's ports. Likewise a PID file in a world-writable directory can be swapped for a symlink
//! between rename and create, making a root process overwrite an arbitrary file. Hence:
//! - defaults live in `$XDG_RUNTIME_DIR` (per user, mode 0700) or `/run/routini`
//!   (systemd `RuntimeDirectory=`), never `/tmp`
//! - both paths are refused if their directory is writable by other users
//! - before handing sockets over, the upgrade socket must be a socket owned by this user

use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, eyre};

use crate::utils::constants::DEFAULT_RUNTIME_DIR;

/// `$XDG_RUNTIME_DIR` if set, otherwise [DEFAULT_RUNTIME_DIR].
pub fn default_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNTIME_DIR))
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail
    unsafe { libc::geteuid() }
}

/// The directory containing `path` must exist, be owned by this user or root, and must not be
/// writable by other users.
pub fn check_private_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let meta = std::fs::metadata(parent).map_err(|err| {
        eyre!(
            "'{}': cannot access directory '{}': {err}",
            path.display(),
            parent.display()
        )
    })?;

    let mode = meta.permissions().mode();
    if mode & 0o002 != 0 {
        return Err(eyre!(
            "'{}' is in '{}', which other users can write to (mode {:o}). Use a private \
             directory such as $XDG_RUNTIME_DIR or /run/routini",
            path.display(),
            parent.display(),
            mode & 0o7777
        ));
    }
    let owner = meta.uid();
    if owner != effective_uid() && owner != 0 {
        return Err(eyre!(
            "'{}' is in '{}', which is owned by another user (uid {owner})",
            path.display(),
            parent.display()
        ));
    }
    Ok(())
}

/// Check the upgrade socket before handing the listening sockets to whoever listens on it: it
/// must be in a private directory and be a socket created by this user.
pub fn check_upgrade_socket(path: &Path) -> Result<()> {
    check_private_parent(path)?;
    let meta = std::fs::symlink_metadata(path).map_err(|err| {
        eyre!(
            "'{}': {err}. Start the new process with --upgrade before sending SIGQUIT",
            path.display()
        )
    })?;
    if !meta.file_type().is_socket() {
        return Err(eyre!("'{}' is not a socket", path.display()));
    }
    if meta.uid() != effective_uid() {
        return Err(eyre!(
            "'{}' is owned by uid {}, not by this process (uid {})",
            path.display(),
            meta.uid(),
            effective_uid()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    fn temp_dir(name: &str, mode: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("routini-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        dir
    }

    #[test]
    fn private_directory_is_accepted() {
        let dir = temp_dir("private", 0o700);
        check_private_parent(&dir.join("routini.pid")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn world_writable_directory_is_rejected() {
        let dir = temp_dir("shared", 0o1777);
        let err = check_private_parent(&dir.join("routini.pid"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("other users can write"), "{err}");
        assert!(check_private_parent(Path::new("/tmp/routini.pid")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_directory_is_rejected() {
        assert!(check_private_parent(Path::new("/nonexistent-routini/x.pid")).is_err());
    }

    #[test]
    fn upgrade_socket_must_be_our_socket() {
        let dir = temp_dir("sock", 0o700);
        let sock = dir.join("upgrade.sock");

        let err = check_upgrade_socket(&sock).unwrap_err().to_string();
        assert!(err.contains("--upgrade"), "{err}");

        std::fs::write(&sock, "").unwrap();
        let err = check_upgrade_socket(&sock).unwrap_err().to_string();
        assert!(err.contains("not a socket"), "{err}");
        std::fs::remove_file(&sock).unwrap();

        let _listener = UnixListener::bind(&sock).unwrap();
        check_upgrade_socket(&sock).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
