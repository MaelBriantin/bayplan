use std::collections::BTreeMap;

use serde::Deserialize;

/// What a bay runs, read from `.bayplan/bays/<bay>.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BayConfig {
    /// Groups of the descriptor whose services the bay duplicates.
    pub groups: Vec<String>,
    /// Git ref the worktree starts from.
    pub from: Option<String>,
    /// Resource name → options of the bay's share, read by that resource only.
    #[serde(default)]
    pub resources: BTreeMap<String, toml::Table>,
}
