use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Dep;
use crate::exec;
use crate::git;
use crate::state::BayState;

/// Links the untracked files of the main tree the stack needs (env files, secrets) into the worktree.
pub fn link_untracked(root: &Path, worktree: &Path, paths: &[impl AsRef<Path>]) -> Result<()> {
    for path in paths {
        let source = root.join(path);
        let target = worktree.join(path);
        if !source.exists() || target.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::os::unix::fs::symlink(&source, &target)
            .with_context(|| format!("cannot link {}", target.display()))?;
    }
    Ok(())
}

/// Copies the untracked files of the main tree read inside containers into the worktree, keeping those
/// already there.
pub fn copy_untracked(root: &Path, worktree: &Path, paths: &[impl AsRef<Path>]) -> Result<()> {
    for path in paths {
        let source = root.join(path);
        let target = worktree.join(path);
        if source.exists() && target.symlink_metadata().is_err() {
            cow_copy(root, &source, &target)?;
        }
    }
    Ok(())
}

/// Copies the dependency directories of the main tree into the worktree.
pub fn clone(root: &Path, worktree: &Path, deps: &[Dep], state: &mut BayState) -> Result<()> {
    for dep in deps {
        let source = root.join(&dep.path);
        let target = worktree.join(&dep.path);
        if !source.exists() || target.exists() {
            continue;
        }
        cow_copy(root, &source, &target)?;
        let lockfile = root.join(&dep.lockfile);
        if lockfile.exists() {
            state.lockfiles.insert(key(dep), git::hash_file(&lockfile)?);
        }
    }
    Ok(())
}

/// Dependencies whose lockfile in the worktree differs from the one their content was installed from.
pub fn outdated<'a>(worktree: &Path, deps: &'a [Dep], state: &BayState) -> Result<Vec<&'a Dep>> {
    let mut outdated = Vec::new();
    for dep in deps {
        let lockfile = worktree.join(&dep.lockfile);
        if !lockfile.exists() {
            continue;
        }
        if state.lockfiles.get(&key(dep)) != Some(&git::hash_file(&lockfile)?) {
            outdated.push(dep);
        }
    }
    Ok(outdated)
}

pub fn mark_installed(worktree: &Path, dep: &Dep, state: &mut BayState) -> Result<()> {
    let hash = git::hash_file(&worktree.join(&dep.lockfile))?;
    state.lockfiles.insert(key(dep), hash);
    Ok(())
}

/// Copies a file or directory with its permissions, as a copy-on-write clone when the filesystem allows it.
fn cow_copy(root: &Path, source: &Path, target: &Path) -> Result<()> {
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let flags = if cfg!(target_os = "macos") {
        "-cpR"
    } else {
        "-a --reflink=auto"
    };
    let mut args: Vec<&std::ffi::OsStr> = flags.split(' ').map(AsRef::as_ref).collect();
    args.extend([source.as_os_str(), target.as_os_str()]);
    exec::run(root, "cp", args)
}

fn key(dep: &Dep) -> String {
    dep.path.to_string_lossy().into_owned()
}
