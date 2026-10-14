//! One Pour at a time per prefix: an advisory lock on `var/pour/db/lock`.

use std::fs::File;
use std::path::Path;

pub struct Lock {
    _file: File,
}

impl Lock {
    /// Block until the lock is ours (or fail if the prefix is unwritable).
    pub fn acquire(path: &Path) -> std::io::Result<Lock> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: fd is valid for the lifetime of `file`.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Lock { _file: file })
    }

    /// Try once; `None` when another pour holds it.
    pub fn try_acquire(path: &Path) -> std::io::Result<Option<Lock>> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: fd is valid for the lifetime of `file`.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                return if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    Ok(None)
                } else {
                    Err(e)
                };
            }
        }
        Ok(Some(Lock { _file: file }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn second_holder_waits() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db/lock");
        let a = Lock::acquire(&p).unwrap();
        assert!(Lock::try_acquire(&p).unwrap().is_none());
        drop(a);
        assert!(Lock::try_acquire(&p).unwrap().is_some());
    }
}
