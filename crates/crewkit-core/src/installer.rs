use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use crate::adapter::Adapter;
use crate::bridge::{self, AuthState};
use crate::cli;
use crate::detect::{detect_all, DetectedClient};
use crate::error::Result;
use crate::fsops::{self, Snapshotter};
use crate::inventory::{claude_plugin_install, codex_plugin_install, inventory, ItemState, Status};
use crate::kit::{Kit, KitPlugin, McpServer};
use crate::marketplace;
use crate::mcp::{self, Outcome, RemoveOutcome};
use crate::paths::Paths;
use crate::state::ManagedState;
use crate::translate::FrontmatterMap;

const CLI_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Ok,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepReport {
    pub step: String,
    pub client: String,
    pub status: StepStatus,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    pub clients: Vec<DetectedClient>,
    pub items: Vec<ItemState>,
    /// CrewKit-level OAuth sessions per MCP server (shared by all clients).
    pub auth: Vec<AuthState>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    pub steps: Vec<StepReport>,
    /// Display names of clients that must be restarted to pick up changes.
    pub restart_needed: Vec<String>,
    /// Fresh post-install scan, so the UI can show the honest end state.
    pub scan: ScanReport,
}

/// Narrows an install to specific clients and/or kit items. `None`
/// means "no restriction": the default scope covers the whole kit on
/// every detected client (what the Install-kit button does).
#[derive(Debug, Clone, Default)]
pub struct InstallScope {
    /// Adapter client ids to touch (e.g. "claude-code"); None = all.
    pub clients: Option<HashSet<String>>,
    /// (kind, id) pairs to install; None = every item in the kit.
    pub items: Option<HashSet<(String, String)>>,
}

impl InstallScope {
    pub fn everything() -> Self {
        Self::default()
    }

    pub fn is_everything(&self) -> bool {
        self.clients.is_none() && self.items.is_none()
    }

    fn wants_client(&self, id: &str) -> bool {
        self.clients.as_ref().is_none_or(|c| c.contains(id))
    }

    fn wants_item(&self, kind: &str, id: &str) -> bool {
        self.items
            .as_ref()
            .is_none_or(|items| items.iter().any(|(k, i)| k == kind && i == id))
    }
}

/// The engine ties a kit, its payload directory and the client adapters
/// to one machine (via `Paths`). The desktop app owns one of these.
pub struct Engine {
    pub paths: Paths,
    pub adapters: Vec<Adapter>,
    pub kit: Kit,
    /// Directory holding the kit's plugin zips.
    pub zips_dir: PathBuf,
    /// The bundled crewkit-bridge binary, deployed to a stable path on install.
    pub bridge_source: PathBuf,
    /// Frontmatter mapping table for the skill-translate pass (data, not code).
    pub frontmatter_map: FrontmatterMap,
}

impl Engine {
    pub fn scan(&self) -> Result<ScanReport> {
        let crewkit_dir = self.paths.crewkit_dir();
        let clients = detect_all(&self.adapters, &self.paths);
        let state = ManagedState::load(&crewkit_dir)?;
        let items = inventory(&self.kit, &self.paths, &state, &clients)?;
        let auth = bridge::auth_status(&self.kit, &crewkit_dir);
        Ok(ScanReport {
            clients,
            items,
            auth,
        })
    }

    /// Install the kit into every detected client. Entries that match a
    /// kit item — same id, or an MCP entry pointing at the same endpoint
    /// under any id — are adopted: replaced with CrewKit's own shape,
    /// with the original config snapshotted first. Unrelated user entries
    /// are never touched; safe to run repeatedly. Reports every step,
    /// including the ones it skipped and why.
    pub fn install(&self, progress: impl FnMut(&StepReport)) -> Result<InstallReport> {
        self.install_scoped(&InstallScope::everything(), progress)
    }

    /// Like `install`, but limited to the clients and items in `scope` —
    /// this is what per-cell installs and the per-app column actions use.
    pub fn install_scoped(
        &self,
        scope: &InstallScope,
        mut progress: impl FnMut(&StepReport),
    ) -> Result<InstallReport> {
        let mut steps: Vec<StepReport> = Vec::new();
        let mut push = |report: StepReport, steps: &mut Vec<StepReport>| {
            progress(&report);
            steps.push(report);
        };

        let crewkit_dir = self.paths.crewkit_dir();
        let mut snap = Snapshotter::new(&crewkit_dir);
        let mut state = ManagedState::load(&crewkit_dir)?;
        let clients = detect_all(&self.adapters, &self.paths);
        let pre = inventory(&self.kit, &self.paths, &state, &clients)?;
        let pre_status: HashMap<(String, String, String), Status> = pre
            .iter()
            .map(|i| ((i.kind.clone(), i.client.clone(), i.id.clone()), i.status))
            .collect();
        let status_of = |kind: &str, client: &str, id: &str| {
            pre_status
                .get(&(kind.to_string(), client.to_string(), id.to_string()))
                .copied()
                .unwrap_or(Status::NotInstalled)
        };
        let client = |id: &str| clients.iter().find(|c| c.id == id);
        let env = self.paths.cli_env();

        // What this run is asked to install. An unrestricted scope keeps
        // the historical whole-kit behavior byte for byte.
        let wanted_plugins: Vec<&KitPlugin> = self
            .kit
            .active_plugins()
            .filter(|p| scope.wants_item("plugin", &self.kit.plugin_id(p)))
            .collect();
        let (wanted_servers, unsupported_servers): (Vec<&McpServer>, Vec<&McpServer>) = self
            .kit
            .active_mcp_servers()
            .filter(|s| scope.wants_item("mcp", &s.id))
            .partition(|s| s.transport_supported());
        let plugins_pass = scope.items.is_none() || !wanted_plugins.is_empty();
        let mcp_pass = scope.items.is_none() || !wanted_servers.is_empty();

        // Spec: a server whose transport this installer does not speak is
        // skipped with a warning — it must never fail the whole kit.
        for server in &unsupported_servers {
            push(
                skipped(
                    &format!("Add MCP server {}", server.id),
                    "crewkit",
                    &format!(
                        "transport `{}` is not supported by this CrewKit version",
                        server.transport()
                    ),
                ),
                &mut steps,
            );
        }

        // 1. Stage the dual-format marketplace directory.
        // Keyed by the marketplace name (what clients register), not the
        // kit id: renaming a kit must not orphan client registrations.
        // MCP-only scopes skip staging entirely — it serves only plugins.
        let marketplace_dir = crewkit_dir
            .join("marketplace")
            .join(&self.kit.marketplace_name);
        let stage_needed =
            plugins_pass && (scope.wants_client("claude-code") || scope.wants_client("codex"));
        if stage_needed {
            match marketplace::stage(
                &self.kit,
                &self.zips_dir,
                &marketplace_dir,
                &self.frontmatter_map,
            ) {
                Ok(warnings) => {
                    push(
                        StepReport {
                            step: "Stage marketplace".into(),
                            client: "crewkit".into(),
                            status: StepStatus::Ok,
                            message: marketplace_dir.display().to_string(),
                        },
                        &mut steps,
                    );
                    // Partial-support notes from the skill-translate pass.
                    for warning in warnings {
                        push(
                            StepReport {
                                step: "Skill check".into(),
                                client: "crewkit".into(),
                                status: StepStatus::Skipped,
                                message: warning,
                            },
                            &mut steps,
                        );
                    }
                }
                Err(e) => {
                    push(
                        StepReport {
                            step: "Stage marketplace".into(),
                            client: "crewkit".into(),
                            status: StepStatus::Failed,
                            message: e.to_string(),
                        },
                        &mut steps,
                    );
                    // Nothing can be installed without the staged marketplace.
                    let scan = self.scan()?;
                    return Ok(InstallReport {
                        steps,
                        restart_needed: vec![],
                        scan,
                    });
                }
            }
        }
        let marketplace_path = marketplace_dir.display().to_string();

        // 2. Deploy the crewkit-bridge binary and its server map. Every
        //    client's MCP entry launches this bridge, so one CrewKit-level
        //    OAuth session serves them all.
        let bridge_bin = bridge::bridge_path(&crewkit_dir);
        let bridge_ok = if !mcp_pass {
            // Plugin-only scope: no MCP entry is written, so the bridge
            // deploy step would only be noise.
            bridge_bin.exists()
        } else {
            match bridge::install_bridge(&self.bridge_source, &crewkit_dir).and_then(|updated| {
                bridge::write_servers_config(&self.kit, &crewkit_dir).map(|()| updated)
            }) {
                Ok(updated) => {
                    push(
                        StepReport {
                            step: "Install crewkit-bridge".into(),
                            client: "crewkit".into(),
                            status: if updated {
                                StepStatus::Ok
                            } else {
                                StepStatus::Skipped
                            },
                            message: if updated {
                                bridge_bin.display().to_string()
                            } else {
                                "already up to date".into()
                            },
                        },
                        &mut steps,
                    );
                    true
                }
                Err(e) => {
                    push(
                        StepReport {
                            step: "Install crewkit-bridge".into(),
                            client: "crewkit".into(),
                            status: StepStatus::Failed,
                            message: e.to_string(),
                        },
                        &mut steps,
                    );
                    false
                }
            }
        };
        let bridge_str = bridge_bin.to_string_lossy().into_owned();

        // 3. Claude: plugins + MCP via the claude CLI (PATH or bundled).
        let mut claude_changed = false;
        match client("claude-code").and_then(|c| c.cli_path.clone()) {
            Some(claude_cli) if scope.wants_client("claude-code") => {
                // The CLI requires its config directory to exist.
                let _ = std::fs::create_dir_all(&self.paths.claude_config_dir);
                // Adopt a marketplace registration under our name that points
                // somewhere else (the user added the kit's source by hand, or
                // the staging path moved): re-point it at the staged copy so
                // the kit's plugins are managed from here on.
                if plugins_pass {
                    let registered_at = fsops::read_json(
                        &self
                            .paths
                            .claude_config_dir
                            .join("plugins/known_marketplaces.json"),
                    )
                    .unwrap_or_default()
                    .and_then(|known| {
                        let entry = known.get(&self.kit.marketplace_name)?;
                        entry
                            .get("installLocation")
                            .or_else(|| entry.get("source").and_then(|s| s.get("path")))
                            .and_then(|v| v.as_str())
                            .map(PathBuf::from)
                    });
                    let elsewhere =
                        registered_at.is_some_and(|p| canonical(&p) != canonical(&marketplace_dir));
                    if elsewhere {
                        run_cli_step(
                            &claude_cli,
                            &[
                                "plugin",
                                "marketplace",
                                "remove",
                                &self.kit.marketplace_name,
                            ],
                            &env,
                            &format!("Adopt marketplace {}", self.kit.marketplace_name),
                            "claude-code",
                            &mut push,
                            &mut steps,
                        );
                    }
                }
                let registered = plugins_pass
                    && run_cli_step(
                        &claude_cli,
                        &["plugin", "marketplace", "add", &marketplace_path],
                        &env,
                        "Register marketplace",
                        "claude-code",
                        &mut push,
                        &mut steps,
                    );
                if registered {
                    for plugin in wanted_plugins.iter().copied() {
                        let plugin_id = self.kit.plugin_id(plugin);
                        if status_of("plugin", "claude-code", &plugin_id) == Status::Installed {
                            // Reinstall means update-or-skip, never duplicate:
                            // when the kit ships a newer version, update it.
                            let staged = staged_plugin_version(&marketplace_dir, &plugin.name);
                            let installed =
                                claude_plugin_install(&self.paths, &plugin_id).map(|(v, _)| v);
                            match (staged, installed) {
                                (Some(next), Some(current)) if next != current => {
                                    let ok = run_cli_step(
                                        &claude_cli,
                                        &["plugin", "update", &plugin_id],
                                        &env,
                                        &format!("Update plugin {plugin_id} to v{next}"),
                                        "claude-code",
                                        &mut push,
                                        &mut steps,
                                    );
                                    claude_changed |= ok;
                                }
                                (_, current) => push(
                                    skipped(
                                        &format!("Install plugin {plugin_id}"),
                                        "claude-code",
                                        &match current {
                                            Some(v) => format!("already installed (v{v})"),
                                            None => "already installed".into(),
                                        },
                                    ),
                                    &mut steps,
                                ),
                            }
                            continue;
                        }
                        let ok = run_cli_step(
                            &claude_cli,
                            // --yes: headless accept of marketplace-declared prompts.
                            &["plugin", "install", "--scope", "user", "--yes", &plugin_id],
                            &env,
                            &format!("Install plugin {plugin_id}"),
                            "claude-code",
                            &mut push,
                            &mut steps,
                        );
                        if ok {
                            state
                                .plugins
                                .insert(ManagedState::key("claude-code", &plugin_id));
                            claude_changed = true;
                        }
                    }
                }
                // MCP entries point at crewkit-bridge (stdio); entries from
                // older CrewKit versions (remote HTTP shape) are migrated and
                // user-added entries for the same server are adopted.
                let claude_json = self.paths.claude_config_dir.join(".claude.json");
                let user_servers = fsops::read_json(&claude_json)
                    .unwrap_or_default()
                    .and_then(|config| config.get("mcpServers").cloned());
                for server in wanted_servers.iter().copied() {
                    if !bridge_ok {
                        break;
                    }
                    let existing = user_servers.as_ref().and_then(|s| s.get(&server.id));
                    let is_bridge_shaped = existing.is_some_and(|e| {
                        bridge::is_bridge_command(e.get("command").and_then(|c| c.as_str()))
                    });
                    let state_managed = state
                        .mcp_servers
                        .contains(&ManagedState::key("claude-code", &server.id));

                    // Adopt user entries under other ids that point at this
                    // server's endpoint — with the bridge entry in place they
                    // would connect the same server twice.
                    for dup in
                        mcp::json_servers_targeting(user_servers.as_ref(), &server.url, &server.id)
                    {
                        let _ = snap.backup(&claude_json);
                        let ok = run_cli_step(
                            &claude_cli,
                            &["mcp", "remove", "--scope", "user", &dup],
                            &env,
                            &format!("Adopt MCP server {} (drop duplicate `{dup}`)", server.id),
                            "claude-code",
                            &mut push,
                            &mut steps,
                        );
                        claude_changed |= ok;
                    }

                    if is_bridge_shaped {
                        push(
                            skip("Add MCP server", "claude-code", &server.id),
                            &mut steps,
                        );
                        continue;
                    }
                    let adopting = existing.is_some() && !state_managed;
                    let step_name = if adopting {
                        format!("Adopt MCP server {}", server.id)
                    } else {
                        format!("Add MCP server {}", server.id)
                    };
                    if existing.is_some() {
                        // Replace in place — ours in the old remote shape, or
                        // a user entry being adopted (the snapshot keeps the
                        // original): drop it, then add the bridge entry.
                        let _ = snap.backup(&claude_json);
                        let _ = cli::run(
                            &claude_cli,
                            &["mcp", "remove", "--scope", "user", &server.id],
                            &env,
                            CLI_TIMEOUT,
                        );
                    }
                    let ok = run_cli_step(
                        &claude_cli,
                        &[
                            "mcp",
                            "add",
                            "--scope",
                            "user",
                            &server.id,
                            "--",
                            &bridge_str,
                            &server.id,
                        ],
                        &env,
                        &step_name,
                        "claude-code",
                        &mut push,
                        &mut steps,
                    );
                    if ok {
                        state
                            .mcp_servers
                            .insert(ManagedState::key("claude-code", &server.id));
                        claude_changed = true;
                    }
                }
            }
            _ => {
                if scope.wants_client("claude-code") {
                    push(
                        StepReport {
                            step: "Claude install".into(),
                            client: "claude-code".into(),
                            status: StepStatus::Skipped,
                            message: "claude CLI not found (neither on PATH nor bundled)".into(),
                        },
                        &mut steps,
                    );
                }
            }
        }

        // 4. Codex: plugins via CLI; MCP written directly into config.toml
        //    (`codex mcp add` can hang on its post-write OAuth probe).
        let mut codex_changed = false;
        match client("codex").and_then(|c| c.cli_path.clone()) {
            Some(codex_cli) if scope.wants_client("codex") => {
                // The CLI refuses to run when CODEX_HOME does not exist.
                let _ = std::fs::create_dir_all(&self.paths.codex_home);
                let mut registered = plugins_pass
                    && run_cli_step(
                        &codex_cli,
                        &["plugin", "marketplace", "add", &marketplace_path],
                        &env,
                        "Register marketplace",
                        "codex",
                        &mut push,
                        &mut steps,
                    );
                if plugins_pass && !registered {
                    // Self-heal a stale registration of our own marketplace
                    // name (e.g. after the staging path moved): re-point it.
                    let stale = steps
                        .last()
                        .map(|s| s.message.contains("already added from a different source"))
                        .unwrap_or(false);
                    if stale {
                        let _ = cli::run(
                            &codex_cli,
                            &[
                                "plugin",
                                "marketplace",
                                "remove",
                                &self.kit.marketplace_name,
                            ],
                            &env,
                            CLI_TIMEOUT,
                        );
                        registered = run_cli_step(
                            &codex_cli,
                            &["plugin", "marketplace", "add", &marketplace_path],
                            &env,
                            "Re-register marketplace",
                            "codex",
                            &mut push,
                            &mut steps,
                        );
                    }
                }
                if registered {
                    for plugin in wanted_plugins.iter().copied() {
                        let plugin_id = self.kit.plugin_id(plugin);
                        if status_of("plugin", "codex", &plugin_id) == Status::Installed {
                            let staged = staged_plugin_version(&marketplace_dir, &plugin.name);
                            let installed = codex_plugin_install(
                                &self.paths,
                                &self.kit.marketplace_name,
                                &plugin.name,
                            )
                            .map(|(v, _)| v);
                            match (staged, installed) {
                                (Some(next), Some(current)) if next != current => {
                                    // `codex plugin add` installs the new
                                    // version from the refreshed snapshot.
                                    let ok = run_cli_step(
                                        &codex_cli,
                                        &["plugin", "add", &plugin_id],
                                        &env,
                                        &format!("Update plugin {plugin_id} to v{next}"),
                                        "codex",
                                        &mut push,
                                        &mut steps,
                                    );
                                    codex_changed |= ok;
                                }
                                (_, current) => push(
                                    skipped(
                                        &format!("Install plugin {plugin_id}"),
                                        "codex",
                                        &match current {
                                            Some(v) => format!("already installed (v{v})"),
                                            None => "already installed".into(),
                                        },
                                    ),
                                    &mut steps,
                                ),
                            }
                            continue;
                        }
                        let ok = run_cli_step(
                            &codex_cli,
                            &["plugin", "add", &plugin_id],
                            &env,
                            &format!("Install plugin {plugin_id}"),
                            "codex",
                            &mut push,
                            &mut steps,
                        );
                        if ok {
                            state.plugins.insert(ManagedState::key("codex", &plugin_id));
                            codex_changed = true;
                        }
                    }
                }
                let config_toml = self.paths.codex_home.join("config.toml");
                for server in wanted_servers.iter().copied() {
                    if !bridge_ok {
                        break;
                    }
                    let state_managed = state
                        .mcp_servers
                        .contains(&ManagedState::key("codex", &server.id));
                    let step_name = format!("Add MCP server {}", server.id);
                    match mcp::adopt_codex_url_duplicates(
                        &config_toml,
                        &server.url,
                        &server.id,
                        &mut snap,
                    ) {
                        Ok(dups) => {
                            for dup in dups {
                                codex_changed = true;
                                push(
                                    ok_step(
                                        &format!(
                                            "Adopt MCP server {} (drop duplicate `{dup}`)",
                                            server.id
                                        ),
                                        "codex",
                                        "same endpoint — now served by the CrewKit entry",
                                    ),
                                    &mut steps,
                                );
                            }
                        }
                        Err(e) => push(failed(&step_name, "codex", &e.to_string()), &mut steps),
                    }
                    match mcp::ensure_codex_server(
                        &config_toml,
                        &server.id,
                        &bridge_bin,
                        state_managed,
                        &mut snap,
                    ) {
                        Ok(outcome) => {
                            if matches!(
                                outcome,
                                Outcome::Installed | Outcome::Updated | Outcome::Adopted
                            ) {
                                codex_changed = true;
                            }
                            state
                                .mcp_servers
                                .insert(ManagedState::key("codex", &server.id));
                            push(outcome_step(&step_name, "codex", outcome), &mut steps);
                        }
                        Err(e) => push(
                            StepReport {
                                step: step_name,
                                client: "codex".into(),
                                status: StepStatus::Failed,
                                message: e.to_string(),
                            },
                            &mut steps,
                        ),
                    }
                }
            }
            _ => {
                if scope.wants_client("codex") {
                    push(
                        StepReport {
                            step: "Codex install".into(),
                            client: "codex".into(),
                            status: StepStatus::Skipped,
                            message:
                                "codex CLI not found (neither on PATH nor bundled in ChatGPT.app)"
                                    .into(),
                        },
                        &mut steps,
                    );
                }
            }
        }

        // 5. Claude Desktop: its local config is stdio-only, so it gets the
        //    same crewkit-bridge entries as everyone else.
        let mut desktop_changed = false;
        let desktop = client("claude-desktop");
        let desktop_wanted = scope.wants_client("claude-desktop") && mcp_pass;
        if desktop_wanted && desktop.map(|c| c.app_installed).unwrap_or(false) && bridge_ok {
            let desktop_config = self
                .paths
                .app_support
                .join("Claude/claude_desktop_config.json");
            for server in wanted_servers.iter().copied() {
                let state_managed = state
                    .mcp_servers
                    .contains(&ManagedState::key("claude-desktop", &server.id));
                let desired = bridge::stdio_entry(&bridge_bin, &server.id);
                let step_name = format!("Add MCP server {}", server.id);
                match mcp::adopt_json_url_duplicates(
                    &desktop_config,
                    &server.url,
                    &server.id,
                    &mut snap,
                ) {
                    Ok(dups) => {
                        for dup in dups {
                            desktop_changed = true;
                            push(
                                ok_step(
                                    &format!(
                                        "Adopt MCP server {} (drop duplicate `{dup}`)",
                                        server.id
                                    ),
                                    "claude-desktop",
                                    "same endpoint — now served by the CrewKit entry",
                                ),
                                &mut steps,
                            );
                        }
                    }
                    Err(e) => push(
                        failed(&step_name, "claude-desktop", &e.to_string()),
                        &mut steps,
                    ),
                }
                match mcp::ensure_json_server(
                    &desktop_config,
                    &server.id,
                    &desired,
                    state_managed,
                    &mut snap,
                ) {
                    Ok(outcome) => {
                        if matches!(
                            outcome,
                            Outcome::Installed | Outcome::Updated | Outcome::Adopted
                        ) {
                            desktop_changed = true;
                        }
                        state
                            .mcp_servers
                            .insert(ManagedState::key("claude-desktop", &server.id));
                        push(
                            outcome_step(&step_name, "claude-desktop", outcome),
                            &mut steps,
                        );
                    }
                    Err(e) => push(
                        StepReport {
                            step: step_name,
                            client: "claude-desktop".into(),
                            status: StepStatus::Failed,
                            message: e.to_string(),
                        },
                        &mut steps,
                    ),
                }
            }
        } else if desktop_wanted {
            push(
                StepReport {
                    step: "Claude Desktop MCP".into(),
                    client: "claude-desktop".into(),
                    status: StepStatus::Skipped,
                    message: if bridge_ok {
                        "Claude.app not found".into()
                    } else {
                        "crewkit-bridge install failed".into()
                    },
                },
                &mut steps,
            );
        }

        // 6. Retire items the kit marks for removal — clean them out of
        //    every client so a kit update can drop old skills and servers.
        //    Only the full-kit install retires; scoped runs leave the rest
        //    of the machine alone.
        if scope.is_everything() {
            let mut emit = |report: StepReport| {
                progress(&report);
                steps.push(report);
            };
            let retired_plugins: Vec<KitPlugin> = self
                .kit
                .plugins
                .iter()
                .filter(|p| p.remove)
                .cloned()
                .collect();
            for plugin in &retired_plugins {
                let (c, x) =
                    self.remove_plugin_inner(plugin, &pre, &clients, &mut state, None, &mut emit);
                claude_changed |= c;
                codex_changed |= x;
            }
            let retired_servers: Vec<McpServer> = self
                .kit
                .mcp_servers
                .iter()
                .filter(|s| s.remove)
                .cloned()
                .collect();
            for server in &retired_servers {
                let (c, x, d) = self.remove_mcp_inner(
                    server, &pre, &clients, &mut state, &mut snap, None, &mut emit,
                );
                claude_changed |= c;
                codex_changed |= x;
                desktop_changed |= d;
            }
        }

        state.save(&crewkit_dir)?;

        // 7. Honest restart guidance: tell the user, don't restart for them.
        let mut restart_needed = Vec::new();
        for c in &clients {
            let changed = match c.id.as_str() {
                "claude-desktop" => desktop_changed || claude_changed,
                "chatgpt-desktop" => codex_changed,
                _ => false,
            };
            if c.restart_required && c.present && changed {
                restart_needed.push(c.name.clone());
            }
        }

        let scan = self.scan()?;
        Ok(InstallReport {
            steps,
            restart_needed,
            scan,
        })
    }

    /// Remove one kit item ("plugin" or "mcp") from every client. Only
    /// CrewKit-owned entries are touched; user config is never modified.
    pub fn remove_item(
        &self,
        kind: &str,
        id: &str,
        progress: impl FnMut(&StepReport),
    ) -> Result<InstallReport> {
        self.remove_item_scoped(kind, id, None, progress)
    }

    /// Like `remove_item`, but limited to the client ids in `targets`
    /// (None = every client). The CrewKit-level OAuth session is only
    /// dropped on an unrestricted removal — other clients may still use it.
    pub fn remove_item_scoped(
        &self,
        kind: &str,
        id: &str,
        targets: Option<&HashSet<String>>,
        mut progress: impl FnMut(&StepReport),
    ) -> Result<InstallReport> {
        let crewkit_dir = self.paths.crewkit_dir();
        let mut snap = Snapshotter::new(&crewkit_dir);
        let mut state = ManagedState::load(&crewkit_dir)?;
        let clients = detect_all(&self.adapters, &self.paths);
        let items = inventory(&self.kit, &self.paths, &state, &clients)?;
        let mut steps: Vec<StepReport> = Vec::new();
        let (mut claude_changed, mut codex_changed, mut desktop_changed) = (false, false, false);

        {
            let mut emit = |report: StepReport| {
                progress(&report);
                steps.push(report);
            };
            match kind {
                "plugin" => {
                    let plugin = self
                        .kit
                        .plugins
                        .iter()
                        .find(|p| p.name == id || self.kit.plugin_id(p) == id)
                        .ok_or_else(|| {
                            crate::error::Error::Invalid(format!("unknown plugin: {id}"))
                        })?;
                    let (c, x) = self.remove_plugin_inner(
                        plugin, &items, &clients, &mut state, targets, &mut emit,
                    );
                    claude_changed |= c;
                    codex_changed |= x;
                }
                "mcp" => {
                    let server = self
                        .kit
                        .mcp_servers
                        .iter()
                        .find(|s| s.id == id)
                        .ok_or_else(|| {
                            crate::error::Error::Invalid(format!("unknown server: {id}"))
                        })?;
                    let (c, x, d) = self.remove_mcp_inner(
                        server, &items, &clients, &mut state, &mut snap, targets, &mut emit,
                    );
                    claude_changed |= c;
                    codex_changed |= x;
                    desktop_changed |= d;
                }
                other => {
                    return Err(crate::error::Error::Invalid(format!(
                        "unknown item kind: {other}"
                    )))
                }
            }
        }

        state.save(&crewkit_dir)?;
        let mut restart_needed = Vec::new();
        for c in &clients {
            let changed = match c.id.as_str() {
                "claude-desktop" => desktop_changed || claude_changed,
                "chatgpt-desktop" => codex_changed,
                _ => false,
            };
            if c.restart_required && c.present && changed {
                restart_needed.push(c.name.clone());
            }
        }
        let scan = self.scan()?;
        Ok(InstallReport {
            steps,
            restart_needed,
            scan,
        })
    }

    /// Uninstall a plugin from both ecosystems via their CLIs.
    /// Returns (claude_changed, codex_changed).
    fn remove_plugin_inner(
        &self,
        plugin: &KitPlugin,
        items: &[ItemState],
        clients: &[DetectedClient],
        state: &mut ManagedState,
        targets: Option<&HashSet<String>>,
        emit: &mut dyn FnMut(StepReport),
    ) -> (bool, bool) {
        let plugin_id = self.kit.plugin_id(plugin);
        let env = self.paths.cli_env();
        let client = |id: &str| clients.iter().find(|c| c.id == id);
        let wants = |id: &str| targets.is_none_or(|t| t.contains(id));
        let mut claude_changed = false;
        let mut codex_changed = false;

        if let Some(cli) =
            client("claude-code").and_then(|c| c.cli_path.clone().filter(|_| wants("claude-code")))
        {
            let step = format!("Remove plugin {plugin_id}");
            if find_status(items, "plugin", "claude-code", &plugin_id) == Status::Installed {
                if run_cli_emit(
                    &cli,
                    &["plugin", "uninstall", &plugin_id],
                    &env,
                    &step,
                    "claude-code",
                    emit,
                ) {
                    state
                        .plugins
                        .remove(&ManagedState::key("claude-code", &plugin_id));
                    claude_changed = true;
                }
            } else {
                emit(skipped(&step, "claude-code", "not installed"));
            }
        }

        if let Some(cli) =
            client("codex").and_then(|c| c.cli_path.clone().filter(|_| wants("codex")))
        {
            let step = format!("Remove plugin {plugin_id}");
            if find_status(items, "plugin", "codex", &plugin_id) == Status::Installed {
                if run_cli_emit(
                    &cli,
                    &["plugin", "remove", &plugin_id],
                    &env,
                    &step,
                    "codex",
                    emit,
                ) {
                    state
                        .plugins
                        .remove(&ManagedState::key("codex", &plugin_id));
                    codex_changed = true;
                }
            } else {
                emit(skipped(&step, "codex", "not installed"));
            }
        }

        (claude_changed, codex_changed)
    }

    /// Remove an MCP server's bridge entries from every client and drop
    /// its cached session. Returns (claude, codex, desktop) change flags.
    #[allow(clippy::too_many_arguments)]
    fn remove_mcp_inner(
        &self,
        server: &McpServer,
        items: &[ItemState],
        clients: &[DetectedClient],
        state: &mut ManagedState,
        snap: &mut Snapshotter,
        targets: Option<&HashSet<String>>,
        emit: &mut dyn FnMut(StepReport),
    ) -> (bool, bool, bool) {
        let env = self.paths.cli_env();
        let client = |id: &str| clients.iter().find(|c| c.id == id);
        let wants = |id: &str| targets.is_none_or(|t| t.contains(id));
        let crewkit_dir = self.paths.crewkit_dir();
        let step = format!("Remove MCP server {}", server.id);
        let (mut claude_changed, mut codex_changed, mut desktop_changed) = (false, false, false);

        if let Some(cli) =
            client("claude-code").and_then(|c| c.cli_path.clone().filter(|_| wants("claude-code")))
        {
            match find_status(items, "mcp", "claude-code", &server.id) {
                Status::Installed => {
                    if run_cli_emit(
                        &cli,
                        &["mcp", "remove", "--scope", "user", &server.id],
                        &env,
                        &step,
                        "claude-code",
                        emit,
                    ) {
                        state
                            .mcp_servers
                            .remove(&ManagedState::key("claude-code", &server.id));
                        claude_changed = true;
                    }
                }
                Status::InstalledForeign => {
                    emit(skip_foreign("Remove MCP server", "claude-code", &server.id))
                }
                _ => emit(skipped(&step, "claude-code", "not installed")),
            }
        }

        if wants("codex") && client("codex").map(|c| c.present).unwrap_or(false) {
            let config_toml = self.paths.codex_home.join("config.toml");
            let owned = find_status(items, "mcp", "codex", &server.id) == Status::Installed;
            match mcp::remove_codex_server(&config_toml, &server.id, owned, snap) {
                Ok(RemoveOutcome::Removed) => {
                    state
                        .mcp_servers
                        .remove(&ManagedState::key("codex", &server.id));
                    codex_changed = true;
                    emit(ok_step(&step, "codex", "removed"));
                }
                Ok(RemoveOutcome::NotPresent) => emit(skipped(&step, "codex", "not installed")),
                Ok(RemoveOutcome::SkippedForeign) => {
                    emit(skip_foreign("Remove MCP server", "codex", &server.id))
                }
                Err(e) => emit(failed(&step, "codex", &e.to_string())),
            }
        }

        if wants("claude-desktop")
            && client("claude-desktop")
                .map(|c| c.app_installed)
                .unwrap_or(false)
        {
            let desktop_config = self
                .paths
                .app_support
                .join("Claude/claude_desktop_config.json");
            let owned =
                find_status(items, "mcp", "claude-desktop", &server.id) == Status::Installed;
            match mcp::remove_json_server(&desktop_config, &server.id, owned, snap) {
                Ok(RemoveOutcome::Removed) => {
                    state
                        .mcp_servers
                        .remove(&ManagedState::key("claude-desktop", &server.id));
                    desktop_changed = true;
                    emit(ok_step(&step, "claude-desktop", "removed"));
                }
                Ok(RemoveOutcome::NotPresent) => {
                    emit(skipped(&step, "claude-desktop", "not installed"))
                }
                Ok(RemoveOutcome::SkippedForeign) => emit(skip_foreign(
                    "Remove MCP server",
                    "claude-desktop",
                    &server.id,
                )),
                Err(e) => emit(failed(&step, "claude-desktop", &e.to_string())),
            }
        }

        // Drop the CrewKit-level session (best-effort server-side
        // revocation) — only when removing from every client: a scoped
        // removal leaves the session for the clients that keep the server.
        if targets.is_none() && bridge::session::load(&crewkit_dir, &server.id).is_some() {
            let bridge_bin = bridge::bridge_path(&crewkit_dir);
            // The bridge revokes server-side before deleting; when it is
            // missing or fails, drop the local session directly.
            let logged_out = bridge_bin.exists()
                && cli::run(
                    &bridge_bin,
                    &["logout", &server.id],
                    &[],
                    Duration::from_secs(60),
                )
                .map(|o| o.success())
                .unwrap_or(false);
            if !logged_out {
                let _ = bridge::session::delete(&crewkit_dir, &server.id);
            }
            emit(ok_step(
                &format!("Log out {}", server.id),
                "crewkit",
                "session dropped",
            ));
        }

        (claude_changed, codex_changed, desktop_changed)
    }
}

/// Canonical form for path comparison; unresolvable paths compare as-is.
fn canonical(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The version the staged marketplace would install for a plugin.
fn staged_plugin_version(marketplace_dir: &std::path::Path, name: &str) -> Option<String> {
    fsops::read_json(
        &marketplace_dir
            .join("plugins")
            .join(name)
            .join(".claude-plugin/plugin.json"),
    )
    .ok()
    .flatten()?
    .get("version")?
    .as_str()
    .map(String::from)
}

fn find_status(items: &[ItemState], kind: &str, client: &str, id: &str) -> Status {
    items
        .iter()
        .find(|i| i.kind == kind && i.client == client && i.id == id)
        .map(|i| i.status)
        .unwrap_or(Status::NotInstalled)
}

fn ok_step(step: &str, client: &str, message: &str) -> StepReport {
    StepReport {
        step: step.to_string(),
        client: client.into(),
        status: StepStatus::Ok,
        message: message.into(),
    }
}

fn skipped(step: &str, client: &str, message: &str) -> StepReport {
    StepReport {
        step: step.to_string(),
        client: client.into(),
        status: StepStatus::Skipped,
        message: message.into(),
    }
}

fn failed(step: &str, client: &str, message: &str) -> StepReport {
    StepReport {
        step: step.to_string(),
        client: client.into(),
        status: StepStatus::Failed,
        message: message.into(),
    }
}

/// Like run_cli_step but for the single-emitter call sites.
fn run_cli_emit(
    program: &std::path::Path,
    args: &[&str],
    env: &[(String, String)],
    step: &str,
    client: &str,
    emit: &mut dyn FnMut(StepReport),
) -> bool {
    match cli::run(program, args, env, CLI_TIMEOUT) {
        Ok(output) if output.success() => {
            emit(ok_step(step, client, &tail(&output.combined())));
            true
        }
        Ok(output) => {
            emit(failed(step, client, &tail(&output.combined())));
            false
        }
        Err(e) => {
            emit(failed(step, client, &e.to_string()));
            false
        }
    }
}

fn skip(step: &str, client: &str, id: &str) -> StepReport {
    StepReport {
        step: format!("{step} {id}"),
        client: client.into(),
        status: StepStatus::Skipped,
        message: "already installed".into(),
    }
}

fn skip_foreign(step: &str, client: &str, id: &str) -> StepReport {
    StepReport {
        step: format!("{step} {id}"),
        client: client.into(),
        status: StepStatus::Skipped,
        message: "an entry with this id exists but is not managed by CrewKit — left untouched"
            .into(),
    }
}

fn outcome_step(step: &str, client: &str, outcome: Outcome) -> StepReport {
    let (status, message) = match outcome {
        Outcome::Installed => (StepStatus::Ok, "installed".to_string()),
        Outcome::Updated => (StepStatus::Ok, "updated".to_string()),
        Outcome::AlreadyInstalled => (StepStatus::Skipped, "already installed".to_string()),
        Outcome::Adopted => (
            StepStatus::Ok,
            "adopted — the entry added outside CrewKit was replaced and is now managed by CrewKit"
                .to_string(),
        ),
    };
    StepReport {
        step: step.to_string(),
        client: client.into(),
        status,
        message,
    }
}

/// Run one CLI call as an install step; returns true when it succeeded.
#[allow(clippy::too_many_arguments)]
fn run_cli_step(
    program: &std::path::Path,
    args: &[&str],
    env: &[(String, String)],
    step: &str,
    client: &str,
    push: &mut impl FnMut(StepReport, &mut Vec<StepReport>),
    steps: &mut Vec<StepReport>,
) -> bool {
    match cli::run(program, args, env, CLI_TIMEOUT) {
        Ok(output) if output.success() => {
            push(
                StepReport {
                    step: step.into(),
                    client: client.into(),
                    status: StepStatus::Ok,
                    message: tail(&output.combined()),
                },
                steps,
            );
            true
        }
        Ok(output) => {
            push(
                StepReport {
                    step: step.into(),
                    client: client.into(),
                    status: StepStatus::Failed,
                    message: tail(&output.combined()),
                },
                steps,
            );
            false
        }
        Err(e) => {
            push(
                StepReport {
                    step: step.into(),
                    client: client.into(),
                    status: StepStatus::Failed,
                    message: e.to_string(),
                },
                steps,
            );
            false
        }
    }
}

/// Keep step messages readable: last line, capped length.
fn tail(text: &str) -> String {
    let last = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = last.chars().take(300).collect();
    if last.chars().count() > 300 {
        out.push('…');
    }
    out
}
