mod bay;
mod descriptor;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

pub use bay::BayConfig;
pub use descriptor::{
    Dep, Descriptor, MysqlConfig, RabbitmqConfig, RedisConfig, Registration, Resources, RouterKind,
};

use crate::git;

/// Where bayplan keeps its files in the repository it manages.
pub struct Layout {
    pub root: PathBuf,
    /// Local files of bayplan: bay configs, generated files, worktrees.
    dir: PathBuf,
}

impl Layout {
    pub fn discover() -> Result<Self> {
        Ok(Self::new(git::main_root(&std::env::current_dir()?)?))
    }

    pub fn new(root: PathBuf) -> Self {
        let dir = root.join(".bayplan");
        Self { root, dir }
    }

    pub fn descriptor(&self) -> PathBuf {
        self.root.join("bayplan.toml")
    }

    pub fn bay_config(&self, bay: &str) -> PathBuf {
        self.dir.join("bays").join(format!("{bay}.toml"))
    }

    /// Generated files of a bay: compose file, rewritten files, state.
    pub fn bay_dir(&self, bay: &str) -> PathBuf {
        self.dir.join("bays").join(bay)
    }

    pub fn worktree(&self, bay: &str) -> PathBuf {
        self.dir.join("worktrees").join(bay)
    }

    /// Bays that have a config file or a state, so that a bay whose config was deleted still counts.
    pub fn bays(&self) -> Result<Vec<String>> {
        let dir = self.dir.join("bays");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut bays = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let Some(name) = path
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let is_config = path.extension().is_some_and(|ext| ext == "toml");
            if is_config || path.join("state.json").exists() {
                bays.insert(name);
            }
        }
        Ok(bays.into_iter().collect())
    }

    /// Lock held while a command changes bays, so that two of them never take the same share.
    pub fn lock(&self) -> PathBuf {
        self.dir.join("lock")
    }

    /// Compose project of the main stack, named after its directory as Compose does.
    pub fn main_project(&self) -> String {
        let name = self
            .root
            .file_name()
            .map_or_default(|name| name.to_string_lossy().to_lowercase());
        name.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect()
    }

    pub fn bay_project(&self, bay: &str) -> String {
        format!("{}-bay-{bay}", self.main_project())
    }
}

/// Longest bay name: it ends up in host labels, limited to 63 characters, after a service name.
const MAX_BAY_NAME: usize = 32;

pub fn validate_bay_name(bay: &str) -> Result<()> {
    let alphanumeric = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let valid = bay.chars().next().is_some_and(alphanumeric)
        && bay.chars().next_back().is_some_and(alphanumeric)
        && bay.chars().all(|c| alphanumeric(c) || c == '-')
        && bay.len() <= MAX_BAY_NAME;
    if !valid {
        bail!(
            "invalid bay name `{bay}`: use lowercase letters, digits and inner dashes, {MAX_BAY_NAME} characters at most"
        );
    }
    Ok(())
}

pub fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bay_names_must_fit_in_a_host_label() {
        for name in ["demo", "feat-42", "1"] {
            assert!(validate_bay_name(name).is_ok(), "{name}");
        }
        for name in ["", "-demo", "demo-", "Demo", "demo_1", &"a".repeat(33)] {
            assert!(validate_bay_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn reads_the_branch_of_a_bay() {
        let config: BayConfig = toml::from_str(
            r#"
            groups = ["app"]
            branch = "feat/demo"
            "#,
        )
        .unwrap();
        assert_eq!(config.branch.as_deref(), Some("feat/demo"));
    }
}
