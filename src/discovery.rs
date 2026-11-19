//! Shared daemon locator operations. Lifecycle authorization stays in the Runtime.
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// One short discovery transaction. The persistent sidecar is never unlinked:
/// every independently opened handle must refer to the same OS lock object.
/// File handles are not cloned or passed to spawned processes. Drop/crash closes
/// the handle and releases ownership; a leftover sidecar is not a held lock.
pub(crate) struct Owner {
    state_dir: PathBuf,
    _file: File,
}

impl Owner {
    pub(crate) fn acquire(state_dir: &Path) -> io::Result<Self> {
        Self::acquire_impl(state_dir, || {})
    }

    fn acquire_impl(state_dir: &Path, mut on_contention: impl FnMut()) -> io::Result<Self> {
        fs::create_dir_all(state_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir.join(".daemon-discovery.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Self {
                        state_dir: state_dir.to_owned(),
                        _file: file,
                    })
                }
                Err(TryLockError::Error(error)) => return Err(error),
                Err(TryLockError::WouldBlock) => {
                    on_contention();
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "daemon_discovery_lock_timeout",
                        ));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    pub(crate) fn read(&self) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.state_dir.join("daemon.json")) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn publish(&self, bytes: &[u8]) -> io::Result<()> {
        let staged = self
            .state_dir
            .join(format!("daemon.json.pending-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&staged)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            // Same-directory replacement never exposes truncated JSON. Windows
            // and Unix std::fs::rename both replace an existing file here.
            fs::rename(&staged, self.state_dir.join("daemon.json"))?;
            #[cfg(unix)]
            File::open(&self.state_dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(staged);
        }
        result
    }
}

pub(crate) fn read(state_dir: &Path) -> io::Result<Option<Vec<u8>>> {
    // Publication is a complete-file atomic rename. Readers need no mutation
    // lock and therefore never wait behind potentially slow Runtime startup.
    match fs::read(state_dir.join("daemon.json")) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(crate) fn publish(state_dir: &Path, bytes: &[u8]) -> io::Result<()> {
    publish_impl(state_dir, bytes, || {})
}

#[cfg(test)]
fn publish_impl(state_dir: &Path, bytes: &[u8], on_contention: impl FnMut()) -> io::Result<()> {
    Owner::acquire_impl(state_dir, on_contention)?.publish(bytes)
}

pub(crate) fn remove_if_matches(state_dir: &Path, expected: &[u8]) -> io::Result<bool> {
    remove_if_matches_impl(state_dir, expected, || {})
}

fn remove_if_matches_impl(
    state_dir: &Path,
    expected: &[u8],
    before_remove: impl FnOnce(),
) -> io::Result<bool> {
    let owner = Owner::acquire(state_dir)?;
    if owner.read()?.as_deref() == Some(expected) {
        before_remove();
        fs::remove_file(state_dir.join("daemon.json"))?;
        return Ok(true);
    }
    Ok(false)
}

// The test build can pause the real operation at its mutation boundary. These
// callbacks and the subprocess protocol are absent from the shipped CLI surface.
#[cfg(test)]
pub(crate) fn publish_with_contention(
    state_dir: &Path,
    bytes: &[u8],
    on_contention: impl FnMut(),
) -> io::Result<()> {
    publish_impl(state_dir, bytes, on_contention)
}

#[cfg(test)]
pub(crate) fn remove_with_checkpoint(
    state_dir: &Path,
    expected: &[u8],
    before_remove: impl FnOnce(),
) -> io::Result<bool> {
    remove_if_matches_impl(state_dir, expected, before_remove)
}
