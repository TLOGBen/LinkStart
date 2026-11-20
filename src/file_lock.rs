//! An exclusive lock on a file that is released when the guard drops.
//!
//! On Unix the lock is an `flock` on the open file description. A child that
//! another thread forks (PTY, `Command`) inherits that description until its
//! `exec` closes it, so merely closing our handle can leave the lock held for
//! as long as the scheduler delays that child. Unlocking first releases it
//! for every copy at once (hypothesis load-deadlines-2, P1: up to ~7 ms under
//! load, unbounded in principle). On Windows the lock follows our handle and
//! the explicit unlock is merely redundant.
use std::fs::{File, TryLockError};

#[derive(Debug)]
pub struct FileLock(File);

impl FileLock {
    /// Takes the lock if it is free; `Ok(None)` when another holder has it.
    pub fn try_acquire(file: File) -> std::io::Result<Option<Self>> {
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn released_lock_is_free_at_once_while_another_thread_forks() {
        // P1: a fork in another thread shares the lock's open file description
        // until exec; without the unlock in Drop, about a quarter of immediate
        // re-acquisitions found it busy.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("job.lock");
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let forking = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(mut child) = std::process::Command::new("true").spawn() {
                        let _ = child.wait();
                    }
                }
            })
        };
        let mut busy = 0;
        for _ in 0..2000 {
            drop(FileLock::try_acquire(open()).unwrap().expect("free before"));
            if FileLock::try_acquire(open()).unwrap().is_none() {
                busy += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        forking.join().unwrap();
        assert_eq!(busy, 0, "a released lock was still held by a forked child");
    }
}
