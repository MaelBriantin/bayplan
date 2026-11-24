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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct Trees {
        root: PathBuf,
        worktree: PathBuf,
    }

    impl Trees {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("bayplan-deps-{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            let trees = Self {
                root: dir.join("main"),
                worktree: dir.join("bay"),
            };
            std::fs::create_dir_all(&trees.root).unwrap();
            std::fs::create_dir_all(&trees.worktree).unwrap();
            trees
        }

        fn write(dir: &Path, path: &str, content: &str) {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    impl Drop for Trees {
        fn drop(&mut self) {
            std::fs::remove_dir_all(self.root.parent().unwrap()).ok();
        }
    }

    fn dep(path: &str, lockfile: &str) -> Dep {
        Dep {
            path: PathBuf::from(path),
            lockfile: PathBuf::from(lockfile),
            install: None,
        }
    }

    #[test]
    fn clones_a_dependency_and_records_its_lockfile() {
        let trees = Trees::new("clone");
        Trees::write(&trees.root, "node_modules/left-pad/index.js", "pad");
        Trees::write(&trees.root, "package-lock.json", "v1");
        let mut state = BayState::default();
        clone(
            &trees.root,
            &trees.worktree,
            &[dep("node_modules", "package-lock.json")],
            &mut state,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(trees.worktree.join("node_modules/left-pad/index.js")).unwrap(),
            "pad"
        );
        assert_eq!(
            state.lockfiles.get("node_modules"),
            Some(&git::hash_file(&trees.root.join("package-lock.json")).unwrap())
        );
    }

    #[test]
    fn leaves_a_copy_already_there_and_a_dependency_the_main_tree_lacks() {
        let trees = Trees::new("clone-skip");
        Trees::write(&trees.root, "node_modules/new.js", "");
        Trees::write(&trees.worktree, "node_modules/old.js", "");
        let deps = [
            dep("node_modules", "package-lock.json"),
            dep("vendor", "composer.lock"),
        ];
        let mut state = BayState::default();
        clone(&trees.root, &trees.worktree, &deps, &mut state).unwrap();
        assert!(!trees.worktree.join("node_modules/new.js").exists());
        assert!(!trees.worktree.join("vendor").exists());
        assert!(state.lockfiles.is_empty());
    }

    #[test]
    fn tells_the_dependencies_whose_lockfile_changed() {
        let trees = Trees::new("outdated");
        Trees::write(&trees.root, "node_modules/a.js", "");
        Trees::write(&trees.root, "package-lock.json", "v1");
        Trees::write(&trees.worktree, "package-lock.json", "v1");
        let deps = [
            dep("node_modules", "package-lock.json"),
            dep("vendor", "composer.lock"),
        ];
        let mut state = BayState::default();
        clone(&trees.root, &trees.worktree, &deps, &mut state).unwrap();
        assert!(outdated(&trees.worktree, &deps, &state).unwrap().is_empty());

        Trees::write(&trees.worktree, "package-lock.json", "v2");
        let changed = outdated(&trees.worktree, &deps, &state).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, PathBuf::from("node_modules"));

        mark_installed(&trees.worktree, changed[0], &mut state).unwrap();
        assert!(outdated(&trees.worktree, &deps, &state).unwrap().is_empty());
    }
}
