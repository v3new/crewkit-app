use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Declarative description of one AI client: where it lives on disk,
/// how to find its CLI, and which files hold its configuration.
/// Adapters are data, not code — clients change monthly and updating
/// an adapter must not require touching install logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Adapter {
    pub id: String,
    pub name: String,
    /// Presence of any of these paths means the desktop app is installed.
    #[serde(default)]
    pub app_paths: Vec<String>,
    #[serde(default)]
    pub cli: Option<CliSpec>,
    /// Named configuration/state files, values are `${var}` templates.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Whether the client must be restarted after its config changes.
    #[serde(default)]
    pub restart_required: bool,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CliSpec {
    /// Binary names to look up on PATH.
    #[serde(default)]
    pub path_names: Vec<String>,
    /// Glob templates for binaries bundled inside app packages —
    /// lets CrewKit drive clients even when no CLI is on PATH.
    #[serde(default)]
    pub bundled_globs: Vec<String>,
    /// A helper tool the install path needs (e.g. `npx` for the
    /// mcp-remote bridge) rather than the client's own CLI: finding it
    /// is not evidence that the client is installed.
    #[serde(default)]
    pub helper: bool,
}

impl Adapter {
    pub fn load(json: &str) -> Result<Adapter> {
        serde_json::from_str(json).map_err(|e| Error::Parse {
            path: PathBuf::from("adapter.json"),
            message: e.to_string(),
        })
    }
}

/// Expand a path pattern where single `*` components match any directory
/// entry (e.g. a version directory). Returns matches sorted ascending,
/// so `.last()` picks the newest version for version-shaped names.
pub fn resolve_glob(pattern: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::new()];
    for component in pattern.components() {
        let part = component.as_os_str();
        if part == "*" {
            let mut expanded = Vec::new();
            for base in &candidates {
                if let Ok(entries) = std::fs::read_dir(base) {
                    for entry in entries.flatten() {
                        expanded.push(entry.path());
                    }
                }
            }
            candidates = expanded;
        } else {
            for c in &mut candidates {
                c.push(part);
            }
        }
    }
    let mut found: Vec<PathBuf> = candidates.into_iter().filter(|p| p.exists()).collect();
    found.sort();
    found
}
