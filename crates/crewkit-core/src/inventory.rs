use serde::Serialize;
use toml_edit::DocumentMut;

use crate::detect::DetectedClient;
use crate::error::Result;
use crate::fsops;
use crate::kit::Kit;
use crate::mcp::{MANAGED_KEY, MANAGED_VALUE};
use crate::paths::Paths;
use crate::state::ManagedState;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Installed and owned by CrewKit.
    Installed,
    /// An entry for this item exists (same id, or an MCP entry pointing
    /// at the same endpoint) but was added outside CrewKit — installing
    /// adopts it: CrewKit replaces it with its own entry and manages it.
    InstalledForeign,
    NotInstalled,
    /// The target client is not present on this machine.
    ClientUnavailable,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemState {
    /// "plugin" or "mcp".
    pub kind: String,
    pub id: String,
    pub client: String,
    pub status: Status,
    pub detail: String,
    /// Installed version, when the client tracks one (plugins).
    pub version: Option<String>,
    /// Last install/update time, unix milliseconds.
    pub updated_at_ms: Option<u64>,
}

/// Installed version + last-update time of a Claude plugin, from
/// `~/.claude/plugins/installed_plugins.json`.
pub fn claude_plugin_install(paths: &Paths, plugin_id: &str) -> Option<(String, Option<u64>)> {
    let registry = fsops::read_json(
        &paths
            .claude_config_dir
            .join("plugins/installed_plugins.json"),
    )
    .ok()
    .flatten()?;
    let entry = registry
        .get("plugins")?
        .get(plugin_id)?
        .as_array()?
        .first()?;
    let version = entry.get("version")?.as_str()?.to_string();
    let updated = entry
        .get("lastUpdated")
        .or_else(|| entry.get("installedAt"))
        .and_then(|v| v.as_str())
        .and_then(iso_to_epoch_ms);
    Some((version, updated))
}

/// Installed version + last-update time of a Codex plugin, from its
/// versioned cache directory `~/.codex/plugins/cache/<mkt>/<name>/<ver>/`.
pub fn codex_plugin_install(
    paths: &Paths,
    marketplace: &str,
    name: &str,
) -> Option<(String, Option<u64>)> {
    let dir = paths
        .codex_home
        .join("plugins/cache")
        .join(marketplace)
        .join(name);
    let mut versions: Vec<(String, Option<u64>)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            (e.file_name().to_string_lossy().into_owned(), mtime)
        })
        .collect();
    versions.sort();
    versions.pop()
}

/// Minimal ISO-8601 (`YYYY-MM-DDTHH:MM:SS[.sss]Z`) to unix milliseconds.
fn iso_to_epoch_ms(iso: &str) -> Option<u64> {
    let date = &iso.get(0..10)?;
    let time = iso.get(11..19).unwrap_or("00:00:00");
    let mut parts = date.split('-');
    let (y, m, d): (i64, i64, i64) = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    // Howard Hinnant's days-from-civil algorithm.
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

/// Read what is actually installed straight from the clients' own state
/// files — the honest inventory the UI shows and the installer consults.
pub fn inventory(
    kit: &Kit,
    paths: &Paths,
    state: &ManagedState,
    clients: &[DetectedClient],
) -> Result<Vec<ItemState>> {
    let mut items = Vec::new();
    let present = |id: &str| clients.iter().any(|c| c.id == id && c.present);

    // --- Claude plugins: ~/.claude/settings.json → enabledPlugins ---
    let claude_settings = fsops::read_json(&paths.claude_config_dir.join("settings.json"))?;
    for plugin in kit.active_plugins() {
        let plugin_id = kit.plugin_id(plugin);
        let status = if !present("claude-code") {
            Status::ClientUnavailable
        } else {
            let enabled = claude_settings
                .as_ref()
                .and_then(|s| s.get("enabledPlugins"))
                .and_then(|p| p.get(&plugin_id))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if enabled {
                Status::Installed
            } else {
                Status::NotInstalled
            }
        };
        let install = claude_plugin_install(paths, &plugin_id);
        items.push(ItemState {
            kind: "plugin".into(),
            id: plugin_id,
            client: "claude-code".into(),
            status,
            detail: "via enabledPlugins in Claude settings".into(),
            version: install.as_ref().map(|(v, _)| v.clone()),
            updated_at_ms: install.and_then(|(_, t)| t),
        });
    }

    // --- Codex plugins and MCP servers: ~/.codex/config.toml ---
    let codex_config_path = paths.codex_home.join("config.toml");
    let codex_doc: Option<DocumentMut> = if codex_config_path.exists() {
        std::fs::read_to_string(&codex_config_path)
            .ok()
            .and_then(|t| t.parse().ok())
    } else {
        None
    };
    for plugin in kit.active_plugins() {
        let plugin_id = kit.plugin_id(plugin);
        let status = if !present("codex") {
            Status::ClientUnavailable
        } else {
            let enabled = codex_doc
                .as_ref()
                .and_then(|d| d.get("plugins"))
                .and_then(|p| p.as_table_like())
                .and_then(|t| t.get(&plugin_id))
                .and_then(|e| e.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if enabled {
                Status::Installed
            } else {
                Status::NotInstalled
            }
        };
        let install = codex_plugin_install(paths, &kit.marketplace_name, &plugin.name);
        items.push(ItemState {
            kind: "plugin".into(),
            id: plugin_id,
            client: "codex".into(),
            status,
            detail: "via [plugins] in codex config.toml".into(),
            version: install.as_ref().map(|(v, _)| v.clone()),
            updated_at_ms: install.and_then(|(_, t)| t),
        });
    }
    for server in kit.active_mcp_servers() {
        let default_detail = "crewkit-bridge (stdio) in codex config.toml".to_string();
        let (status, detail) = if !present("codex") {
            (Status::ClientUnavailable, default_detail)
        } else {
            match codex_doc
                .as_ref()
                .and_then(|d| d.get("mcp_servers"))
                .and_then(|t| t.as_table_like())
                .and_then(|t| t.get(&server.id))
            {
                Some(entry) => {
                    let marked =
                        entry.get(MANAGED_KEY).and_then(|v| v.as_str()) == Some(MANAGED_VALUE);
                    let ours = marked
                        || state
                            .mcp_servers
                            .contains(&ManagedState::key("codex", &server.id));
                    if ours {
                        (Status::Installed, default_detail)
                    } else {
                        (Status::InstalledForeign, foreign_detail(&server.id))
                    }
                }
                None => match codex_doc
                    .as_ref()
                    .map(|d| crate::mcp::codex_servers_targeting(d, &server.url, &server.id))
                    .and_then(|dups| dups.into_iter().next())
                {
                    Some(dup) => (Status::InstalledForeign, foreign_detail(&dup)),
                    None => (Status::NotInstalled, default_detail),
                },
            }
        };
        items.push(ItemState {
            kind: "mcp".into(),
            id: server.id.clone(),
            client: "codex".into(),
            status,
            detail,
            version: None,
            updated_at_ms: None,
        });
    }

    // --- Claude Code MCP servers: user scope in .claude.json ---
    let claude_user_config = fsops::read_json(&paths.claude_config_dir.join(".claude.json"))?;
    let claude_servers = claude_user_config
        .as_ref()
        .and_then(|c| c.get("mcpServers"));
    for server in kit.active_mcp_servers() {
        let default_detail = "crewkit-bridge (stdio) at user scope in .claude.json".to_string();
        let (status, detail) = if !present("claude-code") {
            (Status::ClientUnavailable, default_detail)
        } else {
            match claude_servers.and_then(|m| m.get(&server.id)) {
                Some(entry) => {
                    let bridge_shaped = crate::bridge::is_bridge_command(
                        entry.get("command").and_then(|c| c.as_str()),
                    );
                    let ours = bridge_shaped
                        || state
                            .mcp_servers
                            .contains(&ManagedState::key("claude-code", &server.id));
                    if ours {
                        (Status::Installed, default_detail)
                    } else {
                        (Status::InstalledForeign, foreign_detail(&server.id))
                    }
                }
                None => match crate::mcp::json_servers_targeting(
                    claude_servers,
                    &server.url,
                    &server.id,
                )
                .into_iter()
                .next()
                {
                    Some(dup) => (Status::InstalledForeign, foreign_detail(&dup)),
                    None => (Status::NotInstalled, default_detail),
                },
            }
        };
        items.push(ItemState {
            kind: "mcp".into(),
            id: server.id.clone(),
            client: "claude-code".into(),
            status,
            detail,
            version: None,
            updated_at_ms: None,
        });
    }

    // --- Claude Desktop MCP servers: crewkit-bridge entries in
    // claude_desktop_config.json (its local config is stdio-only). The app
    // may rewrite this file and drop unknown keys, so ownership is judged
    // by the bridge shape plus CrewKit's state.
    let desktop_config =
        fsops::read_json(&paths.app_support.join("Claude/claude_desktop_config.json"))?;
    let desktop_servers = desktop_config.as_ref().and_then(|c| c.get("mcpServers"));
    for server in kit.active_mcp_servers() {
        let default_detail = "crewkit-bridge (stdio) in claude_desktop_config.json".to_string();
        let (status, detail) = if !present("claude-desktop") {
            (Status::ClientUnavailable, default_detail)
        } else {
            match desktop_servers.and_then(|m| m.get(&server.id)) {
                Some(entry) => {
                    let bridge_shaped = crate::bridge::is_bridge_command(
                        entry.get("command").and_then(|c| c.as_str()),
                    );
                    let ours = bridge_shaped
                        || state
                            .mcp_servers
                            .contains(&ManagedState::key("claude-desktop", &server.id));
                    if ours {
                        (Status::Installed, default_detail)
                    } else {
                        (Status::InstalledForeign, foreign_detail(&server.id))
                    }
                }
                None => match crate::mcp::json_servers_targeting(
                    desktop_servers,
                    &server.url,
                    &server.id,
                )
                .into_iter()
                .next()
                {
                    Some(dup) => (Status::InstalledForeign, foreign_detail(&dup)),
                    None => (Status::NotInstalled, default_detail),
                },
            }
        };
        items.push(ItemState {
            kind: "mcp".into(),
            id: server.id.clone(),
            client: "claude-desktop".into(),
            status,
            detail,
            version: None,
            updated_at_ms: None,
        });
    }

    Ok(items)
}

/// Detail line for an entry added outside CrewKit that installing adopts.
fn foreign_detail(entry_id: &str) -> String {
    format!("`{entry_id}` was added outside CrewKit — installing takes over management")
}
