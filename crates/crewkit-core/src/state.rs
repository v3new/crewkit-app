use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fsops;

/// CrewKit's own record of what it manages.
///
/// This is the source of truth for ownership: config entries written by
/// client CLIs carry no marker, so without this file CrewKit could not
/// tell its own entries from ones the user configured by hand — and the
/// installer must never touch the latter.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ManagedState {
    /// Plugin installs, keyed `<client>:<plugin>@<marketplace>`.
    pub plugins: BTreeSet<String>,
    /// MCP server entries, keyed `<client>:<server-id>`.
    pub mcp_servers: BTreeSet<String>,
}

impl ManagedState {
    pub fn path(crewkit_dir: &Path) -> PathBuf {
        crewkit_dir.join("state.json")
    }

    pub fn load(crewkit_dir: &Path) -> Result<Self> {
        match fsops::read_json(&Self::path(crewkit_dir))? {
            Some(value) => Ok(serde_json::from_value(value).unwrap_or_default()),
            None => Ok(Self::default()),
        }
    }

    pub fn save(&self, crewkit_dir: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).expect("state serializes");
        fsops::atomic_write(&Self::path(crewkit_dir), json.as_bytes())
    }

    pub fn key(client: &str, id: &str) -> String {
        format!("{client}:{id}")
    }
}
