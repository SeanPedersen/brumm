//! One running core per profile directory (desktop app, CLI, daemon alike). Two processes on the same SQLite file would both
//! advance the same Olm ratchets and break the sessions for good. The lock is held for the whole
//! life of the core, including its shutdown drain, so a relaunch during the drain waits instead
//! of overlapping. The OS releases it when the process ends, however it ends.

use std::{
    fs::{self, File, TryLockError},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use brumm_core::node::SHUTDOWN_DRAIN_MAX;

const LOCK_FILE: &str = "brumm.lock";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// The previous process may still be draining; after this it counts as a live second instance.
const MAX_WAIT: Duration = Duration::from_secs(SHUTDOWN_DRAIN_MAX.as_secs() + 5);

/// Holds the exclusive lock until dropped.
pub struct ProfileLock {
    _file: File,
}

impl ProfileLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir).context("create data dir")?;
        let file = File::create(data_dir.join(LOCK_FILE)).context("open lock file")?;
        let started = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if started.elapsed() < MAX_WAIT => {
                    thread::sleep(POLL_INTERVAL);
                }
                Err(TryLockError::WouldBlock) => {
                    bail!("another brumm instance is already using this profile")
                }
                Err(TryLockError::Error(err)) => return Err(err).context("lock profile"),
            }
        }
    }
}
