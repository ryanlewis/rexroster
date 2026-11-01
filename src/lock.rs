//! Lock files, so only one process at a time does a thing.
//!
//! The locks are `flock` locks on files in `$XDG_RUNTIME_DIR/rexroster`, or
//! in `rexroster-<uid>` under the temp directory when that is unset. The
//! kernel drops a lock when the last open file description holding it
//! closes, so a crashed holder never leaves a stale lock behind.

use std::fs::{File, OpenOptions, TryLockError};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A held lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

fn open(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        check_private(dir)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// The directory must be ours and closed to everyone else. In a shared temp
/// directory another user could make it first and hold our locks.
fn check_private(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir).with_context(|| format!("checking {}", dir.display()))?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        anyhow::bail!(
            "{} must be owned by uid {uid} with no access for others",
            dir.display()
        );
    }
    Ok(())
}

impl Lock {
    /// Take the lock, or `None` if another process holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<Lock>> {
        let file = open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Lock { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    /// Take the lock, waiting for it if another process holds it.
    pub fn acquire(path: &Path) -> Result<Lock> {
        let file = open(path)?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Lock { file })
    }

    /// Keep the lock's file open across `exec`, so the new program holds the
    /// lock for as long as it runs.
    pub fn keep_across_exec(&self) -> Result<()> {
        // SAFETY: fcntl on a file descriptor this struct owns.
        let fd = self.file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error()).context("keeping the lock across exec");
        }
        Ok(())
    }
}

/// The directory the lock files live in.
pub fn dir() -> PathBuf {
    dir_from(std::env::var_os("XDG_RUNTIME_DIR"), std::env::temp_dir(), {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    })
}

fn dir_from(runtime: Option<std::ffi::OsString>, tmp: PathBuf, uid: u32) -> PathBuf {
    match runtime.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(dir) => dir.join("rexroster"),
        None => tmp.join(format!("rexroster-{uid}")),
    }
}

/// A short stable name for `parts`: FNV-1a, so it is the same in every build.
pub fn key(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.bytes().chain([0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// The Rex server this process talks to, as `rex` picks it: `$REX_SERVER`,
/// else the default local server.
pub fn server() -> String {
    std::env::var("REX_SERVER").unwrap_or_default()
}

#[cfg(test)]
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rexroster-lock-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_acquirer_fails_while_the_first_holds_it() {
        let dir = temp_dir("second");
        let path = dir.join("watch.lock");
        let first = Lock::try_acquire(&path).unwrap().expect("first gets it");
        // A separate open file description, as another process would have.
        assert!(Lock::try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(Lock::try_acquire(&path).unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_lock_dir_others_can_use() {
        // Review finding: a pre-made world-writable dir was used as is.
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("open-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = Lock::try_acquire(&dir.join("w.lock")).unwrap_err();
        assert!(
            format!("{err:#}").contains("no access for others"),
            "{err:#}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn acquire_waits_for_the_holder() {
        let dir = temp_dir("wait");
        let path = dir.join("open.lock");
        let held = Lock::acquire(&path).unwrap();
        let p = path.clone();
        let t = std::thread::spawn(move || {
            let _l = Lock::acquire(&p).unwrap();
            std::time::Instant::now()
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        let released = std::time::Instant::now();
        drop(held);
        assert!(t.join().unwrap() >= released);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keep_across_exec_clears_close_on_exec() {
        let dir = temp_dir("exec");
        let lock = Lock::try_acquire(&dir.join("x.lock")).unwrap().unwrap();
        lock.keep_across_exec().unwrap();
        let flags = unsafe { libc::fcntl(lock.file.as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dir_prefers_xdg_runtime_dir() {
        let tmp = PathBuf::from("/tmp");
        assert_eq!(
            dir_from(Some("/run/user/7".into()), tmp.clone(), 7),
            PathBuf::from("/run/user/7/rexroster")
        );
        assert_eq!(
            dir_from(Some("rel".into()), tmp.clone(), 7),
            PathBuf::from("/tmp/rexroster-7")
        );
        assert_eq!(dir_from(None, tmp, 7), PathBuf::from("/tmp/rexroster-7"));
    }

    #[test]
    fn key_is_stable_and_separates_parts() {
        assert_eq!(key(&["a"]), key(&["a"]));
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
        assert_eq!(key(&[]).len(), 16);
    }
}
