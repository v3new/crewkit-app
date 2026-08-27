use std::path::Path;

use serde::Serialize;
use serde_json::json;
use toml_edit::DocumentMut;

use crate::error::{io_ctx, Error, Result};
use crate::fsops::{self, Snapshotter};

pub const MANAGED_KEY: &str = "_managedBy";
pub const MANAGED_VALUE: &str = "crewkit";

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Installed,
    Updated,
    AlreadyInstalled,
    /// The entry exists but CrewKit does not own it — user config is never touched.
    SkippedForeign,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoveOutcome {
    Removed,
    NotPresent,
    /// The entry is not CrewKit's — left untouched.
    SkippedForeign,
}

/// Remove a CrewKit-owned entry from Codex's `config.toml`.
pub fn remove_codex_server(
    config_path: &Path,
    server_id: &str,
    state_managed: bool,
    snap: &mut Snapshotter,
) -> Result<RemoveOutcome> {
    if !config_path.exists() {
        return Ok(RemoveOutcome::NotPresent);
    }
    let text = std::fs::read_to_string(config_path)
        .map_err(io_ctx(format!("reading {}", config_path.display())))?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| Error::Parse {
            path: config_path.to_path_buf(),
            message: e.to_string(),
        })?;

    let Some(entry) = doc
        .get("mcp_servers")
        .and_then(|i| i.as_table_like())
        .and_then(|t| t.get(server_id))
    else {
        return Ok(RemoveOutcome::NotPresent);
    };
    let marked = entry.get(MANAGED_KEY).and_then(|v| v.as_str()) == Some(MANAGED_VALUE);
    if !marked && !state_managed {
        return Ok(RemoveOutcome::SkippedForeign);
    }

    snap.backup(config_path)?;
    if let Some(table) = doc
        .get_mut("mcp_servers")
        .and_then(|i| i.as_table_like_mut())
    {
        table.remove(server_id);
    }
    fsops::atomic_write(config_path, doc.to_string().as_bytes())?;
    Ok(RemoveOutcome::Removed)
}

/// Remove a CrewKit-owned entry from a JSON `mcpServers` config.
pub fn remove_json_server(
    config_path: &Path,
    server_id: &str,
    owned: bool,
    snap: &mut Snapshotter,
) -> Result<RemoveOutcome> {
    let Some(mut root) = fsops::read_json(config_path)? else {
        return Ok(RemoveOutcome::NotPresent);
    };
    let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) else {
        return Ok(RemoveOutcome::NotPresent);
    };
    if !servers.contains_key(server_id) {
        return Ok(RemoveOutcome::NotPresent);
    }
    if !owned {
        return Ok(RemoveOutcome::SkippedForeign);
    }
    servers.remove(server_id);
    snap.backup(config_path)?;
    let text = serde_json::to_string_pretty(&root).expect("config serializes");
    fsops::atomic_write(config_path, text.as_bytes())?;
    Ok(RemoveOutcome::Removed)
}

/// Ensure a crewkit-bridge stdio entry in Codex's `config.toml`
/// (`[mcp_servers.<id>]` with `command`/`args`).
///
/// Written directly rather than via `codex mcp add`: the CLI attempts an
/// OAuth probe after writing and can hang indefinitely (verified on
/// codex-cli 0.148). `toml_edit` preserves the user's formatting and
/// comments — only our own entry is ever created or modified. Entries
/// from older CrewKit versions (remote `url` shape) are migrated to the
/// bridge shape in place.
pub fn ensure_codex_server(
    config_path: &Path,
    server_id: &str,
    bridge: &Path,
    state_managed: bool,
    snap: &mut Snapshotter,
) -> Result<Outcome> {
    let text = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .map_err(io_ctx(format!("reading {}", config_path.display())))?
    } else {
        String::new()
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| Error::Parse {
            path: config_path.to_path_buf(),
            message: e.to_string(),
        })?;

    let bridge_str = bridge.to_string_lossy();
    let existing = doc
        .get("mcp_servers")
        .and_then(|i| i.as_table_like())
        .and_then(|t| t.get(server_id));
    let mut existed = false;
    if let Some(entry) = existing {
        let marked = entry.get(MANAGED_KEY).and_then(|v| v.as_str()) == Some(MANAGED_VALUE);
        if !marked && !state_managed {
            return Ok(Outcome::SkippedForeign);
        }
        let same_command = entry.get("command").and_then(|v| v.as_str()) == Some(&bridge_str);
        let same_args = entry
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a.len() == 1 && a.get(0).and_then(|v| v.as_str()) == Some(server_id))
            .unwrap_or(false);
        if marked && same_command && same_args {
            return Ok(Outcome::AlreadyInstalled);
        }
        existed = true;
    }

    snap.backup(config_path)?;
    // Replace the whole entry: migrating from the old remote shape must
    // drop stale keys like `url`.
    let mut entry = toml_edit::Table::new();
    entry["command"] = toml_edit::value(bridge_str.as_ref());
    let mut args = toml_edit::Array::new();
    args.push(server_id);
    entry["args"] = toml_edit::value(args);
    entry[MANAGED_KEY] = toml_edit::value(MANAGED_VALUE);
    doc["mcp_servers"][server_id] = toml_edit::Item::Table(entry);
    if let Some(table) = doc["mcp_servers"].as_table_mut() {
        // Render as [mcp_servers.<id>] sections, matching what the CLI writes.
        table.set_implicit(true);
    }
    fsops::atomic_write(config_path, doc.to_string().as_bytes())?;
    Ok(if existed {
        Outcome::Updated
    } else {
        Outcome::Installed
    })
}

/// Ensure an entry in a JSON config with an `mcpServers` map (Claude
/// Desktop's `claude_desktop_config.json`).
///
/// Ownership rules: an identical entry is already installed; an entry
/// CrewKit's state owns is updated in place; anything else belongs to
/// the user and is left untouched.
pub fn ensure_json_server(
    config_path: &Path,
    id: &str,
    desired: &serde_json::Value,
    state_managed: bool,
    snap: &mut Snapshotter,
) -> Result<Outcome> {
    let mut root = fsops::read_json(config_path)?.unwrap_or_else(|| json!({}));
    let root_obj = root.as_object_mut().ok_or_else(|| Error::Parse {
        path: config_path.to_path_buf(),
        message: "root is not an object".into(),
    })?;

    let servers = root_obj
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| Error::Parse {
            path: config_path.to_path_buf(),
            message: "mcpServers is not an object".into(),
        })?;

    let mut existed = false;
    if let Some(existing) = servers.get(id) {
        if existing == desired {
            return Ok(Outcome::AlreadyInstalled);
        }
        if !state_managed {
            return Ok(Outcome::SkippedForeign);
        }
        existed = true;
    }

    servers.insert(id.to_string(), desired.clone());
    snap.backup(config_path)?;
    let text = serde_json::to_string_pretty(&root).expect("config serializes");
    fsops::atomic_write(config_path, text.as_bytes())?;
    Ok(if existed {
        Outcome::Updated
    } else {
        Outcome::Installed
    })
}
