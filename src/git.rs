use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::exec;

/// Root of the main working tree, even when called from inside one of its worktrees.
pub fn main_root(dir: &Path) -> Result<PathBuf> {
    let common_dir = exec::output(
        dir,
        "git",
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .context("not inside a git repository")?;
    let common_dir = PathBuf::from(common_dir.trim());
    common_dir
        .parent()
        .map(Path::to_path_buf)
        .context("the git directory has no parent")
}

/// Creates a worktree on a detached HEAD, so that no branch is held by it.
pub fn add_worktree(root: &Path, path: &Path, from: &str) -> Result<()> {
    exec::run(
        root,
        "git",
        [
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            path.as_os_str(),
            from.as_ref(),
        ],
    )
}

pub fn is_dirty(path: &Path) -> Result<bool> {
    Ok(!exec::output(path, "git", ["status", "--porcelain"])?
        .trim()
        .is_empty())
}

/// Whether the commit checked out in a worktree is reachable from no branch, tag or remote branch: removing
/// the worktree would then lose it, and the commits before it.
pub fn is_unreferenced(path: &Path) -> Result<bool> {
    Ok(exec::output(
        path,
        "git",
        [
            "for-each-ref",
            "--contains",
            "HEAD",
            "--count=1",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?
    .trim()
    .is_empty())
}

pub fn remove_worktree(root: &Path, path: &Path) -> Result<()> {
    exec::run(
        root,
        "git",
        [
            "worktree".as_ref(),
            "remove".as_ref(),
            "--force".as_ref(),
            path.as_os_str(),
        ],
    )?;
    exec::run(root, "git", ["worktree", "prune"])
}

pub fn short_head(path: &Path) -> Result<String> {
    Ok(exec::output(path, "git", ["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_owned())
}

/// Content hash of a file, stable across machines and tool versions.
pub fn hash_file(path: &Path) -> Result<String> {
    let dir = path.parent().context("file without a parent directory")?;
    Ok(
        exec::output(dir, "git", ["hash-object".as_ref(), path.as_os_str()])?
            .trim()
            .to_owned(),
    )
}
