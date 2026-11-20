use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Exclusive right to change bays, released when dropped. It holds the pid of its owner, so that a lock left
/// by a killed command is taken over instead of blocking every later one.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        for _ in 0..2 {
            match std::fs::File::create_new(path) {
                Ok(mut file) => {
                    write!(file, "{}", std::process::id())?;
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner = std::fs::read_to_string(path).unwrap_or_default();
                    if is_alive(owner.trim()) {
                        bail!(
                            "another bp command (pid {}) is changing bays: wait for it to finish",
                            owner.trim()
                        );
                    }
                    std::fs::remove_file(path).with_context(|| {
                        format!("cannot remove the stale lock {}", path.display())
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot create {}", path.display()));
                }
            }
        }
        bail!("cannot take the lock {}", path.display())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn is_alive(pid: &str) -> bool {
    !pid.is_empty()
        && pid.chars().all(|c| c.is_ascii_digit())
        && std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_exclusive_and_released_when_dropped() {
        let dir = std::env::temp_dir().join(format!("bayplan-lock-{}", std::process::id()));
        let path = dir.join("lock");
        let lock = Lock::acquire(&path).unwrap();
        assert!(Lock::acquire(&path).is_err());
        drop(lock);
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn takes_over_a_lock_left_by_a_dead_process() {
        let dir = std::env::temp_dir().join(format!("bayplan-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lock");
        std::fs::write(&path, "999999999").unwrap();
        assert!(Lock::acquire(&path).is_ok());
        std::fs::remove_dir_all(dir).ok();
    }
}
