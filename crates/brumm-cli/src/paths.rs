//! Where profiles, the daemon socket and its lock live.
//!
//! The data root is the desktop app's (`~/.local/share/brumm` on Linux), so `--profile bot`
//! names the same directory in both, and the profile lock keeps them from running it at once.
//! Profiles live in `<root>/profiles/<name>`. The socket is `$XDG_RUNTIME_DIR/brumm/daemon.sock`
//! for the default root (fallback: the root), and gets a short hash of the root otherwise, so
//! daemons for different roots never share a socket.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const DEFAULT_PROFILE: &str = "default";
const PROFILES_DIR: &str = "profiles";
const DAEMON_LOCK: &str = "daemon.lock";
const DAEMON_LOG: &str = "daemon.log";
const SOCKET_DIR: &str = "brumm";
const SOCKET_NAME: &str = "daemon";
/// Hex digits of the root's hash in a non-default socket name.
const ROOT_TAG_LEN: usize = 12;
const MAX_PROFILE_NAME: usize = 64;

/// The app's data root.
pub fn default_root() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("chat", "brumm", "brumm").context("no home directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Profile names become directory names: letters, digits, `-` and `_` only.
pub fn check_profile_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_PROFILE_NAME
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!("invalid profile name {name:?}: use 1-{MAX_PROFILE_NAME} letters, digits, '-' or '_'");
    }
    Ok(())
}

pub fn profiles_dir(root: &Path) -> PathBuf {
    root.join(PROFILES_DIR)
}

pub fn profile_dir(root: &Path, name: &str) -> Result<PathBuf> {
    check_profile_name(name)?;
    Ok(profiles_dir(root).join(name))
}

pub fn daemon_lock(root: &Path) -> PathBuf {
    root.join(DAEMON_LOCK)
}

pub fn daemon_log(root: &Path) -> PathBuf {
    root.join(DAEMON_LOG)
}

/// The daemon's socket (Unix) or pipe name (Windows) for `root`.
pub fn socket_path(root: &Path, is_default_root: bool) -> PathBuf {
    let name = if is_default_root {
        SOCKET_NAME.to_owned()
    } else {
        let hash = blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex();
        format!("{SOCKET_NAME}-{}", &hash[..ROOT_TAG_LEN])
    };
    platform_socket(root, &name)
}

#[cfg(unix)]
fn platform_socket(root: &Path, name: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|runtime| PathBuf::from(runtime).join(SOCKET_DIR))
        .unwrap_or_else(|| root.to_path_buf());
    dir.join(format!("{name}.sock"))
}

#[cfg(windows)]
fn platform_socket(_root: &Path, name: &str) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\brumm-{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_cannot_escape_the_root() {
        for bad in ["", "..", "a/b", "a\\b", "x y", &"n".repeat(65)] {
            assert!(check_profile_name(bad).is_err(), "{bad:?}");
        }
        for good in ["default", "bot-1", "Agent_7"] {
            check_profile_name(good).unwrap();
        }
    }

    #[test]
    fn roots_get_their_own_socket() {
        let a = socket_path(Path::new("/srv/a"), false);
        let b = socket_path(Path::new("/srv/b"), false);
        assert_ne!(a, b);
        assert_eq!(a, socket_path(Path::new("/srv/a"), false));
    }
}
