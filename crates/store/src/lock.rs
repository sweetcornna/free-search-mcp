//! Cross-process advisory file locking (`fd-lock`) for background sync leases
//! (`<cache_dir>/rw-sync.lock`, I15) and P1 browser `user_data_dir` leases.

use fd_lock::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use fsm_core::FsmError;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

pub use fsm_core::ext::w0c::RW_SYNC_LOCK_FILENAME;

/// Open (or create without truncating) a lockfile, creating parent directories as needed.
fn open_lock_file(path: &Path) -> Result<File, FsmError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            FsmError::internal(
                "store_lock_dir",
                format!("failed to create lock dir {}: {e}", parent.display()),
            )
        })?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            FsmError::internal(
                "store_lock_open",
                format!("failed to open lock file {}: {e}", path.display()),
            )
        })
}

/// Cross-process advisory lock handle wrapping `fd_lock::RwLock<File>`.
///
/// Use [`FileLock`] for scoped synchronous locking with zero extra threads, or
/// [`FileLease`] when an owned `'static` lock guard must be held across `.await`
/// points or stored inside a struct.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
    inner: RwLock<File>,
}

/// Exclusive write guard returned by [`FileLock::try_write`] or [`FileLock::write_blocking`].
#[derive(Debug)]
pub struct FileLockGuard<'a> {
    _guard: RwLockWriteGuard<'a, File>,
}

/// Shared read guard returned by [`FileLock::try_read`].
#[derive(Debug)]
pub struct FileReadGuard<'a> {
    _guard: RwLockReadGuard<'a, File>,
}

impl FileLock {
    /// Open or create the lockfile at `path` (does not acquire the lock yet).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, FsmError> {
        let path = path.as_ref().to_path_buf();
        let file = open_lock_file(&path)?;
        Ok(Self {
            path,
            inner: RwLock::new(file),
        })
    }

    /// Path of the underlying lockfile.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Non-blocking exclusive lock attempt.
    ///
    /// Returns `Ok(Some(guard))` if acquired, `Ok(None)` if another process or
    /// open descriptor already holds the lock (`WouldBlock`).
    pub fn try_write(&mut self) -> Result<Option<FileLockGuard<'_>>, FsmError> {
        match self.inner.try_write() {
            Ok(guard) => Ok(Some(FileLockGuard { _guard: guard })),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(FsmError::internal(
                "store_lock_acquire",
                format!("try_write lock on {} failed: {e}", self.path.display()),
            )),
        }
    }

    /// Blocking exclusive lock acquisition.
    pub fn write_blocking(&mut self) -> Result<FileLockGuard<'_>, FsmError> {
        match self.inner.write() {
            Ok(guard) => Ok(FileLockGuard { _guard: guard }),
            Err(e) => Err(FsmError::internal(
                "store_lock_acquire",
                format!("write lock on {} failed: {e}", self.path.display()),
            )),
        }
    }

    /// Non-blocking shared read lock attempt.
    pub fn try_read(&self) -> Result<Option<FileReadGuard<'_>>, FsmError> {
        loop {
            match self.inner.try_read() {
                Ok(guard) => return Ok(Some(FileReadGuard { _guard: guard })),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(FsmError::internal(
                        "store_lock_acquire",
                        format!("try_read lock on {} failed: {e}", self.path.display()),
                    ));
                }
            }
        }
    }
}

/// Owned, `'static`, `Send` cross-process exclusive file lease backed by `fd-lock`.
///
/// Safe to hold across `.await` points (for example during Retraction Watch CSV
/// streaming sync in `fsm-scholar-graph`). Dropping the lease (or calling
/// [`FileLease::release`]) deterministically unlocks the OS file descriptor
/// before returning.
#[derive(Debug)]
pub struct FileLease {
    path: PathBuf,
    release_tx: Option<mpsc::SyncSender<()>>,
    holder: Option<JoinHandle<()>>,
}

impl FileLease {
    /// Attempt a non-blocking exclusive cross-process lease on `path`.
    ///
    /// Returns `Ok(Some(lease))` when acquired, or `Ok(None)` when another
    /// holder owns the lock (concurrent processes skip sync and fall back).
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Option<Self>, FsmError> {
        Self::spawn_acquire(path.as_ref().to_path_buf(), false)
    }

    /// Acquire an exclusive cross-process lease on `path`, blocking until available.
    pub fn acquire_blocking(path: impl AsRef<Path>) -> Result<Self, FsmError> {
        Self::spawn_acquire(path.as_ref().to_path_buf(), true)?
            .ok_or_else(|| FsmError::internal("store_lock_acquire", "blocking lease returned None"))
    }

    /// Convenience helper: attempt a non-blocking lease on `<cache_dir>/rw-sync.lock`.
    pub fn try_acquire_rw_sync(cache_dir: impl AsRef<Path>) -> Result<Option<Self>, FsmError> {
        Self::try_acquire(cache_dir.as_ref().join(RW_SYNC_LOCK_FILENAME))
    }

    /// Path of the locked file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Explicitly release the lease and wait until the OS lock is unlocked.
    pub fn release(mut self) {
        self.unlock_inner();
    }

    fn spawn_acquire(path: PathBuf, blocking: bool) -> Result<Option<Self>, FsmError> {
        // Open synchronously first so path/permission errors surface directly.
        let mut lock = FileLock::open(&path)?;
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<bool, FsmError>>(1);
        let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
        let thread_path = path.clone();

        let holder = std::thread::Builder::new()
            .name("fsm-fd-lock".into())
            .spawn(move || {
                let res = if blocking {
                    lock.write_blocking().map(Some)
                } else {
                    lock.try_write()
                };
                match res {
                    Ok(Some(guard)) => {
                        if ready_tx.send(Ok(true)).is_ok() {
                            // Hold the OS file lock until signaled or dropped.
                            let _ = release_rx.recv();
                        }
                        drop(guard);
                    }
                    Ok(None) => {
                        let _ = ready_tx.send(Ok(false));
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| {
                FsmError::internal(
                    "store_lock_thread",
                    format!(
                        "failed to spawn lock thread for {}: {e}",
                        thread_path.display()
                    ),
                )
            })?;

        match ready_rx.recv() {
            Ok(Ok(true)) => Ok(Some(Self {
                path,
                release_tx: Some(release_tx),
                holder: Some(holder),
            })),
            Ok(Ok(false)) => {
                let _ = holder.join();
                Ok(None)
            }
            Ok(Err(e)) => {
                let _ = holder.join();
                Err(e)
            }
            Err(_) => {
                let _ = holder.join();
                Err(FsmError::internal(
                    "store_lock_thread",
                    "lock holder thread exited unexpectedly",
                ))
            }
        }
    }

    fn unlock_inner(&mut self) {
        if let Some(tx) = self.release_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.holder.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for FileLease {
    fn drop(&mut self) {
        self.unlock_inner();
    }
}
