use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::{BrowserError, Result};

pub(crate) struct ProfileLock {
    _file: File,
    path: PathBuf,
}

impl std::fmt::Debug for ProfileLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileLock")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
    }
}

impl ProfileLock {
    pub(crate) fn acquire(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir)
            .map_err(|e| BrowserError::Launch(format!("create profile directory: {e}")))?;
        let path = data_dir.join(".browser-rs.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| BrowserError::Launch(format!("open {}: {e}", path.display())))?;
        file.try_lock_exclusive()
            .map_err(|e| BrowserError::ProfileBusy(format!("{} ({e})", data_dir.display())))?;
        Ok(Self { _file: file, path })
    }
}

pub(crate) fn prepare_chrome_profile(data_dir: &Path) -> Result<()> {
    ensure_no_live_singleton(data_dir)?;

    // DevToolsActivePort is not Chrome's ownership lock. Once profile
    // ownership is established it is safe to remove a stale endpoint file.
    remove_if_exists(&data_dir.join("DevToolsActivePort"))?;

    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(BrowserError::Launch(format!(
            "remove stale {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
fn ensure_no_live_singleton(data_dir: &Path) -> Result<()> {
    let path = data_dir.join("SingletonLock");
    let target = match fs::read_link(&path) {
        Ok(target) => target,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(BrowserError::ProfileBusy(format!(
                "cannot verify {}: {e}",
                path.display()
            )))
        }
    };
    let target = target.to_str().ok_or_else(|| {
        BrowserError::ProfileBusy(format!("{} has a non-UTF-8 target", path.display()))
    })?;
    let pid = target
        .rsplit_once('-')
        .and_then(|(_, pid)| pid.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            BrowserError::ProfileBusy(format!(
                "{} has an unrecognized target {target:?}",
                path.display()
            ))
        })?;

    if process_is_alive(pid) {
        return Err(BrowserError::ProfileBusy(format!(
            "{} is held by Chrome pid {pid}",
            data_dir.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

#[cfg(not(unix))]
fn ensure_no_live_singleton(data_dir: &Path) -> Result<()> {
    let path = data_dir.join("SingletonLock");
    if path.exists() {
        return Err(BrowserError::ProfileBusy(format!(
            "cannot safely verify {} on this platform",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn profile_lock_is_exclusive_and_released_on_drop() {
        let dir = tempdir().unwrap();
        let first = ProfileLock::acquire(dir.path()).unwrap();
        assert!(matches!(
            ProfileLock::acquire(dir.path()),
            Err(BrowserError::ProfileBusy(_))
        ));
        drop(first);
        ProfileLock::acquire(dir.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn live_chrome_singleton_is_never_removed() {
        let dir = tempdir().unwrap();
        let singleton = dir.path().join("SingletonLock");
        symlink(format!("testhost-{}", std::process::id()), &singleton).unwrap();

        assert!(matches!(
            prepare_chrome_profile(dir.path()),
            Err(BrowserError::ProfileBusy(_))
        ));
        assert!(fs::symlink_metadata(singleton).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn stale_chrome_ownership_files_are_left_for_chrome_to_recover() {
        let dir = tempdir().unwrap();
        symlink("testhost-2147483647", dir.path().join("SingletonLock")).unwrap();
        File::create(dir.path().join("SingletonCookie")).unwrap();
        File::create(dir.path().join("DevToolsActivePort")).unwrap();

        prepare_chrome_profile(dir.path()).unwrap();

        assert!(fs::symlink_metadata(dir.path().join("SingletonLock")).is_ok());
        assert!(dir.path().join("SingletonCookie").exists());
        assert!(!dir.path().join("DevToolsActivePort").exists());
    }
}
