use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What bayplan created for a bay, so that it can be reused or removed.
#[derive(Serialize, Deserialize, Default)]
pub struct BayState {
    /// Resource name → state of the bay's share, opaque to everything but its resource.
    #[serde(default)]
    pub resources: BTreeMap<String, Value>,
    /// Dependency path → hash of the lockfile its content matches.
    #[serde(default)]
    pub lockfiles: BTreeMap<String, String>,
    /// Host of the main stack → host of the bay.
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
    /// Registration file → lines this bay contributes.
    #[serde(default)]
    pub registrations: BTreeMap<String, Vec<String>>,
}

impl BayState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))
    }

    pub fn save(&mut self, path: &Path) -> Result<()> {
        self.resources.retain(|_, state| !state.is_null());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    /// State of a resource, `null` when the bay has no share of it yet.
    pub fn resource(&self, name: &str) -> &Value {
        self.resources.get(name).unwrap_or(&Value::Null)
    }

    pub fn resource_mut(&mut self, name: &str) -> &mut Value {
        self.resources.entry(name.to_owned()).or_insert(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bayplan-state-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn starts_empty_without_a_file() {
        let state = BayState::load(&dir("missing").join("state.json")).unwrap();
        assert!(state.resources.is_empty() && state.hosts.is_empty());
    }

    #[test]
    fn reports_an_unreadable_file_by_its_path() {
        let dir = dir("unreadable");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, "{").unwrap();
        let error = BayState::load(&path).err().unwrap();
        assert!(error.to_string().contains(&path.display().to_string()));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn saves_without_the_shares_left_null_and_reads_them_back() {
        let dir = dir("round-trip");
        let path = dir.join("bays").join("demo").join("state.json");
        let mut state = BayState::default();
        *state.resource_mut("mysql") = Value::Null;
        *state.resource_mut("redis") = json!({"indexes": {"0": 5}});
        state.hosts.insert(
            "app.localhost".to_owned(),
            "bay-demo.app.localhost".to_owned(),
        );
        state.save(&path).unwrap();

        let read = BayState::load(&path).unwrap();
        assert_eq!(read.resources.keys().collect::<Vec<_>>(), ["redis"]);
        assert_eq!(read.resource("redis"), &json!({"indexes": {"0": 5}}));
        assert_eq!(read.resource("mysql"), &Value::Null);
        assert_eq!(read.hosts, state.hosts);
        std::fs::remove_dir_all(dir).ok();
    }
}
