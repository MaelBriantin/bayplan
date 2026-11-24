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

/// Creates a worktree on a new branch starting at `from`.
pub fn add_worktree_new_branch(root: &Path, path: &Path, branch: &str, from: &str) -> Result<()> {
    exec::run(
        root,
        "git",
        [
            "worktree".as_ref(),
            "add".as_ref(),
            "-b".as_ref(),
            branch.as_ref(),
            path.as_os_str(),
            from.as_ref(),
        ],
    )
}

/// Creates a worktree on an existing branch; git refuses when another worktree holds it.
pub fn add_worktree_on_branch(root: &Path, path: &Path, branch: &str) -> Result<()> {
    exec::run(
        root,
        "git",
        [
            "worktree".as_ref(),
            "add".as_ref(),
            path.as_os_str(),
            branch.as_ref(),
        ],
    )
}

pub fn branch_exists(root: &Path, branch: &str) -> bool {
    exec::output(
        root,
        "git",
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// Branch checked out in a worktree, `None` on a detached HEAD.
pub fn current_branch(path: &Path) -> Option<String> {
    exec::output(path, "git", ["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|branch| branch.trim().to_owned())
}

pub fn delete_branch(root: &Path, branch: &str) -> Result<()> {
    exec::run(root, "git", ["branch", "-D", branch])
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

/// Commits ahead of and behind the upstream branch, `None` without one.
pub fn ahead_behind(path: &Path) -> Option<(usize, usize)> {
    let counts = exec::output(
        path,
        "git",
        ["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    )
    .ok()?;
    let (ahead, behind) = counts.trim().split_once('\t')?;
    Some((ahead.parse().ok()?, behind.parse().ok()?))
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

#[cfg(test)]
pub mod tests {
    use std::process::Command;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=bayplan",
                "-c",
                "user.email=bayplan@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A fresh repository on `main` with one commit, unique to the test that names it.
    pub fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bayplan-git-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
        dir
    }

    pub fn create_branch(root: &Path, branch: &str) {
        git(root, &["branch", branch]);
    }

    #[test]
    fn tells_whether_a_branch_exists() {
        let root = repo("branch-exists");
        assert!(branch_exists(&root, "main"));
        assert!(!branch_exists(&root, "feat/x"));
        create_branch(&root, "feat/x");
        assert!(branch_exists(&root, "feat/x"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn reads_the_current_branch_or_none_when_detached() {
        let root = repo("current-branch");
        assert_eq!(current_branch(&root).as_deref(), Some("main"));
        let worktree = root.join("detached");
        add_worktree(&root, &worktree, "HEAD").unwrap();
        assert_eq!(current_branch(&worktree), None);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn creates_a_branch_for_a_worktree_and_deletes_it() {
        let root = repo("new-branch");
        let worktree = root.join("wt");
        add_worktree_new_branch(&root, &worktree, "feat/x", "HEAD").unwrap();
        assert_eq!(current_branch(&worktree).as_deref(), Some("feat/x"));
        remove_worktree(&root, &worktree).unwrap();
        delete_branch(&root, "feat/x").unwrap();
        assert!(!branch_exists(&root, "feat/x"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn checks_out_an_existing_branch_unless_another_worktree_holds_it() {
        let root = repo("existing-branch");
        create_branch(&root, "feat/x");
        let worktree = root.join("wt");
        add_worktree_on_branch(&root, &worktree, "feat/x").unwrap();
        assert_eq!(current_branch(&worktree).as_deref(), Some("feat/x"));
        assert!(add_worktree_on_branch(&root, &root.join("other"), "main").is_err());
        std::fs::remove_dir_all(root).ok();
    }
}
