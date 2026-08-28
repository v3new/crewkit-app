//! Full end-to-end install against the real client CLIs, with every
//! config directory sandboxed (`CLAUDE_CONFIG_DIR` / `CODEX_HOME` are
//! passed to the CLIs, so nothing outside the temp dir is touched) and
//! a synthetic kit payload (no proprietary skills required).
//!
//! Requires the clients on this machine; opt in with:
//! `CREWKIT_E2E=1 cargo test -p crewkit-core --test e2e -- --nocapture`

mod common;

use std::path::PathBuf;

use common::synth_kit;
use crewkit_core::inventory::Status;
use crewkit_core::translate::FrontmatterMap;
use crewkit_core::{Adapter, Engine, Paths, StepStatus};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[test]
fn full_install_roundtrip_in_sandbox() {
    if std::env::var("CREWKIT_E2E").is_err() {
        eprintln!("skipping: set CREWKIT_E2E=1 to run against real client CLIs");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());

    let adapters: Vec<Adapter> = ["claude-code", "claude-desktop", "codex", "chatgpt-desktop"]
        .iter()
        .map(|name| {
            let json =
                std::fs::read_to_string(repo_root().join(format!("adapters/{name}.json"))).unwrap();
            Adapter::load(&json).unwrap()
        })
        .collect();

    let (kit, zips_dir) = synth_kit(tmp.path());

    // Pre-seed hand-added MCP entries the install must adopt: one under
    // the kit id (Claude, remote shape) and one under a different id
    // pointing at the kit's endpoint (Codex).
    std::fs::create_dir_all(&paths.claude_config_dir).unwrap();
    std::fs::write(
        paths.claude_config_dir.join(".claude.json"),
        r#"{"mcpServers": {"test-mcp": {"type": "http", "url": "https://mcp.example.dev/mcp"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(&paths.codex_home).unwrap();
    std::fs::write(
        paths.codex_home.join("config.toml"),
        "[mcp_servers.hand-added]\nurl = \"https://mcp.example.dev/mcp\"\n",
    )
    .unwrap();

    // A stand-in bridge binary is enough: install only deploys and
    // references it — clients launch it later, outside this test.
    let bridge_source = tmp.path().join("bridge-source");
    std::fs::write(&bridge_source, b"#!/bin/sh\nexit 0\n").unwrap();

    let map_json =
        std::fs::read_to_string(repo_root().join("adapters/frontmatter-map.json")).unwrap();
    let engine = Engine {
        paths: paths.clone(),
        adapters,
        kit,
        zips_dir,
        bridge_source,
        frontmatter_map: FrontmatterMap::load(&map_json).unwrap(),
    };

    let report = engine
        .install(|step| {
            eprintln!(
                "[{:?}] {} :: {} — {}",
                step.status, step.client, step.step, step.message
            )
        })
        .unwrap();
    assert!(
        !report.steps.iter().any(|s| s.status == StepStatus::Failed),
        "no step may fail: {:#?}",
        report.steps
    );

    // The hand-added entries were adopted, not skipped: the Claude entry
    // is bridge-shaped now, and the Codex duplicate under its own id is gone.
    // Gate on what the installer itself requires: a usable CLI.
    let has_cli = |id: &str| {
        report
            .scan
            .clients
            .iter()
            .any(|c| c.id == id && c.cli_path.is_some())
    };
    if has_cli("claude-code") {
        let claude_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(paths.claude_config_dir.join(".claude.json")).unwrap(),
        )
        .unwrap();
        let cmd = claude_json["mcpServers"]["test-mcp"]["command"]
            .as_str()
            .unwrap_or_default();
        assert!(
            cmd.ends_with("crewkit-bridge"),
            "hand-added Claude entry must be adopted into the bridge shape, got: {claude_json}"
        );
    }
    if has_cli("codex") {
        let toml = std::fs::read_to_string(paths.codex_home.join("config.toml")).unwrap();
        assert!(
            !toml.contains("hand-added"),
            "same-endpoint duplicate must be adopted (removed): {toml}"
        );
        // The codex CLI may normalize mcp_servers to inline form — check
        // the parsed value, not the rendering.
        let doc: toml_edit::DocumentMut = toml.parse().unwrap();
        let cmd = doc["mcp_servers"]["test-mcp"]["command"]
            .as_str()
            .unwrap_or_default();
        assert!(cmd.ends_with("crewkit-bridge"), "{toml}");
    }

    // Second run must be a no-op: everything already installed or skipped.
    let second = engine.install(|_| {}).unwrap();
    assert!(
        !second.steps.iter().any(|s| s.status == StepStatus::Failed),
        "second run must not fail: {:#?}",
        second.steps
    );
    for item in &second.scan.items {
        // A client without a usable CLI can't have its seeded entry
        // adopted — only clients the installer actually reached count.
        if item.client == "claude-code" && !has_cli("claude-code") {
            continue;
        }
        assert_ne!(
            item.status,
            Status::InstalledForeign,
            "nothing should stay foreign after install (adoption): {item:?}"
        );
    }
    eprintln!("--- final inventory ---");
    for item in &second.scan.items {
        eprintln!(
            "{} {} @ {} -> {:?}",
            item.kind, item.id, item.client, item.status
        );
    }

    // Removal roundtrip: uninstall one plugin and the MCP server from
    // every client, then verify the inventory reports them gone.
    let removed = engine
        .remove_item("plugin", "toolbox", |step| {
            eprintln!(
                "[{:?}] {} :: {} — {}",
                step.status, step.client, step.step, step.message
            )
        })
        .unwrap();
    assert!(
        !removed.steps.iter().any(|s| s.status == StepStatus::Failed),
        "plugin removal must not fail: {:#?}",
        removed.steps
    );
    let removed = engine
        .remove_item("mcp", "test-mcp", |step| {
            eprintln!(
                "[{:?}] {} :: {} — {}",
                step.status, step.client, step.step, step.message
            )
        })
        .unwrap();
    assert!(
        !removed.steps.iter().any(|s| s.status == StepStatus::Failed),
        "mcp removal must not fail: {:#?}",
        removed.steps
    );
    for item in &removed.scan.items {
        let is_removed_target = (item.kind == "plugin" && item.id.starts_with("toolbox@"))
            || (item.kind == "mcp" && item.id == "test-mcp");
        if is_removed_target && item.status == Status::Installed {
            panic!("item still installed after removal: {item:?}");
        }
    }
}
