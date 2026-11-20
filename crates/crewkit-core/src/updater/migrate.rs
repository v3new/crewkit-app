//! Repairs of what older CrewKit versions left in client configs. Cheap
//! and idempotent, so they run before every update pass.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::cowork;
use crate::detect::detect_all;
use crate::error::Result;
use crate::fsops::{self, Snapshotter};
use crate::paths::Paths;
use crate::state::ManagedState;

const CLI_TIMEOUT: Duration = Duration::from_secs(60);

pub fn run(paths: &Paths) {
    for profile in cowork::profiles(paths) {
        let _ = cowork::repair(&profile);
    }
    let _ = move_claude_mcp_servers(paths, || claude_cli(paths));
}

fn claude_cli(paths: &Paths) -> Option<PathBuf> {
    detect_all(&crate::assets::adapters().ok()?, paths)
        .into_iter()
        .find(|c| c.id == "claude-code")?
        .cli_path
}

/// Older CrewKit always ran `claude` with `CLAUDE_CONFIG_DIR=~/.claude`,
/// which put its MCP servers in `~/.claude/.claude.json`, a file a plain
/// `claude` never reads. Re-add them where Claude Code looks, then drop
/// them from the old file.
fn move_claude_mcp_servers(
    paths: &Paths,
    find_cli: impl FnOnce() -> Option<PathBuf>,
) -> Result<()> {
    let Some(legacy) = paths.legacy_claude_json() else {
        return Ok(());
    };
    let Some(mut config) = fsops::read_json(&legacy)? else {
        return Ok(());
    };
    // `CLAUDE_CONFIG_DIR` set in a shell profile is invisible to the app;
    // a file `claude` itself has been run against is that user's live one.
    if config.get("projects").is_some() || config.get("numStartups").is_some() {
        return Ok(());
    }
    let managed = ManagedState::load(&paths.crewkit_dir())?.mcp_servers;
    let ids: Vec<String> = config
        .get("mcpServers")
        .and_then(Value::as_object)
        .map(|servers| {
            servers
                .iter()
                .filter(|(id, entry)| {
                    managed.contains(&ManagedState::key("claude-code", id))
                        && entry.get("command").is_some_and(Value::is_string)
                        && entry
                            .get("env")
                            .and_then(Value::as_object)
                            .is_none_or(|env| env.is_empty())
                })
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Ok(());
    }
    let Some(claude) = find_cli() else {
        return Ok(());
    };
    let live = fsops::read_json(&paths.claude_json)?
        .and_then(|c| c.get("mcpServers").cloned())
        .unwrap_or_default();
    let env = paths.cli_env();
    let mut moved = Vec::new();
    for id in ids {
        if live.get(&id).is_none() {
            let entry = &config["mcpServers"][&id];
            let command = entry["command"].as_str().unwrap_or_default();
            let mut args = vec!["mcp", "add", "--scope", "user", &id, "--", command];
            if let Some(extra) = entry.get("args").and_then(Value::as_array) {
                args.extend(extra.iter().filter_map(Value::as_str));
            }
            let added = crate::cli::run(&claude, &args, &env, CLI_TIMEOUT);
            if !added.is_ok_and(|output| output.success()) {
                continue;
            }
        }
        moved.push(id);
    }
    if moved.is_empty() {
        return Ok(());
    }
    if let Some(servers) = config.get_mut("mcpServers").and_then(Value::as_object_mut) {
        for id in &moved {
            servers.remove(id);
        }
    }
    Snapshotter::new(&paths.crewkit_dir()).backup(&legacy)?;
    let text = serde_json::to_string_pretty(&config).expect("json serializes");
    fsops::atomic_write(&legacy, text.as_bytes())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn default_paths(root: &std::path::Path) -> Paths {
        let rooted = Paths::rooted(root);
        Paths {
            claude_json: rooted.home.join(".claude.json"),
            ..rooted
        }
    }

    #[test]
    fn moves_only_managed_servers_out_of_the_legacy_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = default_paths(tmp.path());
        let legacy = paths.legacy_claude_json().unwrap();
        std::fs::create_dir_all(&paths.claude_config_dir).unwrap();
        std::fs::write(
            &legacy,
            r#"{"mcpServers":{
                "ours":{"type":"stdio","command":"/bin/bridge","args":["ours"],"env":{}},
                "kept":{"type":"stdio","command":"/bin/bridge","args":["kept"],"env":{}},
                "theirs":{"command":"npx","args":["x"]}
            },"other":1}"#,
        )
        .unwrap();
        std::fs::write(
            &paths.claude_json,
            r#"{"mcpServers":{"kept":{"command":"/bin/bridge"}}}"#,
        )
        .unwrap();
        let mut state = ManagedState::default();
        state
            .mcp_servers
            .insert(ManagedState::key("claude-code", "ours"));
        state
            .mcp_servers
            .insert(ManagedState::key("claude-code", "kept"));
        std::fs::create_dir_all(paths.crewkit_dir()).unwrap();
        state.save(&paths.crewkit_dir()).unwrap();

        let log = tmp.path().join("claude.log");
        let cli = tmp.path().join("claude");
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\necho \"[$CLAUDE_CONFIG_DIR] $@\" >> '{}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();

        move_claude_mcp_servers(&paths, || Some(cli.clone())).unwrap();

        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls, "[] mcp add --scope user ours -- /bin/bridge ours\n");
        let left: Value = serde_json::from_str(&std::fs::read_to_string(&legacy).unwrap()).unwrap();
        let servers = left["mcpServers"].as_object().unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["theirs"]);
        assert_eq!(left["other"], 1);

        move_claude_mcp_servers(&paths, || panic!("nothing left to move")).unwrap();
    }

    #[test]
    fn keeps_the_entry_when_the_cli_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = default_paths(tmp.path());
        let legacy = paths.legacy_claude_json().unwrap();
        std::fs::create_dir_all(&paths.claude_config_dir).unwrap();
        let original = r#"{"mcpServers":{"ours":{"command":"/bin/bridge","args":["ours"]}}}"#;
        std::fs::write(&legacy, original).unwrap();
        let mut state = ManagedState::default();
        state
            .mcp_servers
            .insert(ManagedState::key("claude-code", "ours"));
        std::fs::create_dir_all(paths.crewkit_dir()).unwrap();
        state.save(&paths.crewkit_dir()).unwrap();

        move_claude_mcp_servers(&paths, || Some(PathBuf::from("/usr/bin/false"))).unwrap();

        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), original);
    }

    #[test]
    fn leaves_a_config_claude_runs_against_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = default_paths(tmp.path());
        let legacy = paths.legacy_claude_json().unwrap();
        std::fs::create_dir_all(&paths.claude_config_dir).unwrap();
        let original = r#"{"numStartups":3,"mcpServers":{"ours":{"command":"/bin/bridge"}}}"#;
        std::fs::write(&legacy, original).unwrap();
        let mut state = ManagedState::default();
        state
            .mcp_servers
            .insert(ManagedState::key("claude-code", "ours"));
        std::fs::create_dir_all(paths.crewkit_dir()).unwrap();
        state.save(&paths.crewkit_dir()).unwrap();

        move_claude_mcp_servers(&paths, || panic!("must not look for the CLI")).unwrap();

        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), original);
    }
}
