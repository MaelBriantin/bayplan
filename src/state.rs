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
