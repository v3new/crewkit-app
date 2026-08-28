//! Sandboxed tests: everything runs against a temp directory with a
//! synthetic kit payload — never the real user configuration and never
//! proprietary skills.

mod common;

use std::path::{Path, PathBuf};

use common::synth_kit;
use crewkit_core::bridge::{install_bridge, stdio_entry, write_servers_config};
use crewkit_core::fsops::Snapshotter;
use crewkit_core::mcp::{ensure_codex_server, ensure_json_server, Outcome};
use crewkit_core::translate::FrontmatterMap;
use crewkit_core::{marketplace, Paths};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn frontmatter_map() -> FrontmatterMap {
    let json = std::fs::read_to_string(repo_root().join("adapters/frontmatter-map.json")).unwrap();
    FrontmatterMap::load(&json).unwrap()
}

fn bridge_bin() -> PathBuf {
    PathBuf::from("/fake/CrewKit/bin/crewkit-bridge")
}

#[test]
fn stage_marketplace_normalizes_all_payload_shapes() {
    let tmp = tempfile::tempdir().unwrap();
    let (kit, zips) = synth_kit(tmp.path());
    let dest = tmp.path().join("marketplace");
    let map = frontmatter_map();
    marketplace::stage(&kit, &zips, &dest, &map).unwrap();

    // Both ecosystem-level manifests exist.
    assert!(dest.join(".claude-plugin/marketplace.json").is_file());
    assert!(dest.join(".agents/plugins/marketplace.json").is_file());

    for plugin in &kit.plugins {
        let dir = dest.join("plugins").join(&plugin.name);
        // Every plugin ends up with both manifests, whatever shape the zip had.
        assert!(
            dir.join(".claude-plugin/plugin.json").is_file(),
            "{}",
            plugin.name
        );
        assert!(
            dir.join(".codex-plugin/plugin.json").is_file(),
            "{}",
            plugin.name
        );
        assert!(dir.join("skills").is_dir(), "{}", plugin.name);
    }

    // The bare-skill zip got wrapped into a single-skill plugin, and
    // OpenAI UI metadata was generated for it.
    assert!(dest.join("plugins/notes/skills/notes/SKILL.md").is_file());
    let yaml = std::fs::read_to_string(dest.join("plugins/notes/skills/notes/agents/openai.yaml"))
        .unwrap();
    assert!(yaml.contains("display_name: \"Notes\""));
    assert!(yaml.contains("developer_name: \"Test Publisher\""));

    // The plugin-shaped zip kept its own manifest (version preserved).
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dest.join("plugins/toolbox/.claude-plugin/plugin.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["version"], "9.9.9");

    // Re-staging over an existing marketplace succeeds (idempotent swap).
    marketplace::stage(&kit, &zips, &dest, &map).unwrap();
    assert!(dest.join(".claude-plugin/marketplace.json").is_file());
}

#[test]
fn bridge_deploys_with_server_map() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let crewkit_dir = paths.crewkit_dir();

    let source = tmp.path().join("bridge-source");
    std::fs::write(&source, b"#!/bin/sh\n").unwrap();

    assert!(install_bridge(&source, &crewkit_dir).unwrap());
    // Second deploy of identical bytes is a no-op.
    assert!(!install_bridge(&source, &crewkit_dir).unwrap());
    let installed = crewkit_core::bridge::bridge_path(&crewkit_dir);
    assert!(installed.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "bridge must be executable");
    }

    let (kit, _) = synth_kit(tmp.path());
    write_servers_config(&kit, &crewkit_dir).unwrap();
    let servers: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(crewkit_dir.join("servers.json")).unwrap())
            .unwrap();
    assert_eq!(
        servers["servers"]["test-mcp"]["url"],
        "https://mcp.example.dev/mcp"
    );
}

#[test]
fn codex_mcp_merge_is_idempotent_and_adopts_kit_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let config = paths.codex_home.join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    // Simulate a user config with their own entry, a comment, and a
    // leftover entry from an older CrewKit (remote URL shape, marked).
    std::fs::write(
        &config,
        "# user's own config\nmodel = \"o3\"\n\n[mcp_servers.my-server]\nurl = \"https://example.com/mcp\"\n\n[mcp_servers.old-managed]\nurl = \"https://old.example.dev/mcp\"\n_managedBy = \"crewkit\"\n",
    )
    .unwrap();
    let mut snap = Snapshotter::new(&paths.crewkit_dir());
    let bridge = bridge_bin();

    assert_eq!(
        ensure_codex_server(&config, "test-mcp", &bridge, false, &mut snap).unwrap(),
        Outcome::Installed
    );
    assert_eq!(
        ensure_codex_server(&config, "test-mcp", &bridge, false, &mut snap).unwrap(),
        Outcome::AlreadyInstalled
    );

    // Installing a kit server never touches unrelated user entries.
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("# user's own config"),
        "user comments preserved"
    );
    assert!(text.contains("model = \"o3\""), "user settings preserved");
    assert!(
        text.contains("https://example.com/mcp"),
        "unrelated user MCP entry preserved"
    );

    // A user entry under a kit item's id is adopted, not skipped.
    assert_eq!(
        ensure_codex_server(&config, "my-server", &bridge, false, &mut snap).unwrap(),
        Outcome::Adopted
    );

    // A marked entry in the old remote shape migrates to the bridge shape.
    assert_eq!(
        ensure_codex_server(&config, "old-managed", &bridge, false, &mut snap).unwrap(),
        Outcome::Updated
    );

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("# user's own config"));
    assert!(text.contains("model = \"o3\""));
    assert!(
        !text.contains("https://example.com/mcp"),
        "adopted entry now launches the bridge instead of the raw URL"
    );
    assert!(text.contains("_managedBy = \"crewkit\""));
    assert!(
        !text.contains("https://old.example.dev/mcp"),
        "stale url key dropped on migration"
    );
}

#[test]
fn codex_url_duplicates_are_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let config = paths.codex_home.join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    // The user added the kit's endpoint by hand under their own names —
    // once as a remote URL entry (trailing slash and all), once through
    // an mcp-remote wrapper — plus one unrelated server.
    std::fs::write(
        &config,
        concat!(
            "[mcp_servers.my-kit-server]\nurl = \"https://mcp.example.dev/mcp/\"\n\n",
            "[mcp_servers.wrapped]\ncommand = \"npx\"\nargs = [\"mcp-remote\", \"https://mcp.example.dev/mcp\"]\n\n",
            "[mcp_servers.other]\nurl = \"https://unrelated.example.com/mcp\"\n",
        ),
    )
    .unwrap();
    let mut snap = Snapshotter::new(&paths.crewkit_dir());

    let removed = crewkit_core::mcp::adopt_codex_url_duplicates(
        &config,
        "https://mcp.example.dev/mcp",
        "test-mcp",
        &mut snap,
    )
    .unwrap();
    assert_eq!(removed, vec!["my-kit-server", "wrapped"]);

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(!text.contains("my-kit-server"));
    assert!(!text.contains("wrapped"));
    assert!(
        text.contains("https://unrelated.example.com/mcp"),
        "unrelated entries stay"
    );

    // Idempotent: a second sweep finds nothing.
    let removed = crewkit_core::mcp::adopt_codex_url_duplicates(
        &config,
        "https://mcp.example.dev/mcp",
        "test-mcp",
        &mut snap,
    )
    .unwrap();
    assert!(removed.is_empty());
}

#[test]
fn json_mcp_merge_is_idempotent_and_adopts_kit_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let config = paths.app_support.join("Claude/claude_desktop_config.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        r#"{"preferences": {"theme": "dark"}, "mcpServers": {"mine": {"command": "npx"}}}"#,
    )
    .unwrap();
    let mut snap = Snapshotter::new(&paths.crewkit_dir());
    let bridge = bridge_bin();

    // Claude Desktop's config is stdio-only; entries launch crewkit-bridge.
    let desired = stdio_entry(&bridge, "test-mcp");
    assert_eq!(desired["command"], "/fake/CrewKit/bin/crewkit-bridge");
    assert_eq!(desired["args"][0], "test-mcp");

    assert_eq!(
        ensure_json_server(&config, "test-mcp", &desired, false, &mut snap).unwrap(),
        Outcome::Installed
    );
    assert_eq!(
        ensure_json_server(&config, "test-mcp", &desired, false, &mut snap).unwrap(),
        Outcome::AlreadyInstalled
    );

    // Installing a kit server never touches unrelated user entries...
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(value["preferences"]["theme"], "dark");
    assert_eq!(value["mcpServers"]["mine"]["command"], "npx");

    // ...a user entry under a kit item's id is adopted...
    let other = stdio_entry(&bridge, "mine");
    assert_eq!(
        ensure_json_server(&config, "mine", &other, false, &mut snap).unwrap(),
        Outcome::Adopted
    );
    // ...and a state-owned entry is updated in place.
    let moved = stdio_entry(Path::new("/fake/other/crewkit-bridge"), "test-mcp");
    assert_eq!(
        ensure_json_server(&config, "test-mcp", &moved, true, &mut snap).unwrap(),
        Outcome::Updated
    );

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(value["preferences"]["theme"], "dark");
    assert_eq!(
        value["mcpServers"]["mine"]["command"], "/fake/CrewKit/bin/crewkit-bridge",
        "adopted entry now launches the bridge"
    );
    assert_eq!(
        value["mcpServers"]["test-mcp"]["command"],
        "/fake/other/crewkit-bridge"
    );
}

#[test]
fn json_url_duplicates_are_adopted() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let config = paths.app_support.join("Claude/claude_desktop_config.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    // Claude Desktop is stdio-only: a hand-added remote server shows up
    // as an mcp-remote wrapper carrying the URL in its args.
    std::fs::write(
        &config,
        r#"{"mcpServers": {
            "my-kit-server": {"command": "npx", "args": ["mcp-remote", "https://mcp.example.dev/mcp"]},
            "other": {"command": "npx", "args": ["mcp-remote", "https://unrelated.example.com/mcp"]}
        }}"#,
    )
    .unwrap();
    let mut snap = Snapshotter::new(&paths.crewkit_dir());

    let removed = crewkit_core::mcp::adopt_json_url_duplicates(
        &config,
        "https://mcp.example.dev/mcp",
        "test-mcp",
        &mut snap,
    )
    .unwrap();
    assert_eq!(removed, vec!["my-kit-server"]);

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert!(value["mcpServers"].get("my-kit-server").is_none());
    assert!(
        value["mcpServers"].get("other").is_some(),
        "unrelated stays"
    );
}

#[test]
fn snapshot_rollback_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let config = paths.codex_home.join("config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "original = true\n").unwrap();

    let mut snap = Snapshotter::new(&paths.crewkit_dir());
    ensure_codex_server(&config, "test-mcp", &bridge_bin(), false, &mut snap).unwrap();

    // Rollback restores the pre-write file.
    std::fs::write(&config, "modified = true\n").unwrap();
    let restored = crewkit_core::fsops::rollback_latest(&paths.crewkit_dir()).unwrap();
    assert_eq!(restored, vec![config.clone()]);
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "original = true\n"
    );
}

// --- Remote kits: signatures, integrity, key pinning ---

#[test]
fn kit_signature_roundtrip() {
    let (secret, public) = crewkit_core::kits::generate_keypair();
    let manifest = br#"{"id":"x"}"#;
    let signature = crewkit_core::kits::sign_manifest(manifest, &secret).unwrap();
    crewkit_core::kits::verify_manifest(&public, manifest, &signature).unwrap();
    assert!(crewkit_core::kits::verify_manifest(&public, b"tampered", &signature).is_err());
    let (_, other_public) = crewkit_core::kits::generate_keypair();
    assert!(crewkit_core::kits::verify_manifest(&other_public, manifest, &signature).is_err());
}

#[test]
fn resolve_url_variants() {
    use crewkit_core::kits::resolve_url;
    let base = "https://example.com/kit/acme.json";
    assert_eq!(
        resolve_url(base, "https://cdn.example.com/a.zip"),
        "https://cdn.example.com/a.zip"
    );
    assert_eq!(
        resolve_url(base, "/skills/a.zip"),
        "https://example.com/skills/a.zip"
    );
    assert_eq!(
        resolve_url(base, "./a.zip"),
        "https://example.com/kit/a.zip"
    );
    assert_eq!(resolve_url(base, "a.zip"), "https://example.com/kit/a.zip");
}

/// A tiny single-threaded HTTP file server for the fetch test.
fn serve(routes: Vec<(String, Vec<u8>)>) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 2048];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
            let body = routes
                .iter()
                .find(|(route, _)| *route == path)
                .map(|(_, body)| body.clone());
            let response = match body {
                Some(body) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&response);
        }
    });
    port
}

#[test]
fn remote_kit_fetch_verifies_signature_and_integrity() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let crewkit_dir = paths.crewkit_dir();

    let (_, zips) = synth_kit(tmp.path());
    let zip = std::fs::read(zips.join("notes.zip")).unwrap();
    let sha = crewkit_core::kits::sha256_hex(&zip);
    let (secret, public) = crewkit_core::kits::generate_keypair();

    let manifest = serde_json::json!({
        "id": "remote-test",
        "name": "Remote Test Kit",
        "version": "1.0.0",
        "publisher": "Test",
        "publisherKey": public,
        "marketplaceName": "remote-test",
        "plugins": [{
            "name": "notes",
            "artifact": { "url": "/payload.zip", "sha256": sha },
        }],
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let signature = crewkit_core::kits::sign_manifest(&manifest_bytes, &secret).unwrap();

    let port = serve(vec![
        ("/kit.json".into(), manifest_bytes.clone()),
        ("/kit.json.sig".into(), signature.clone().into_bytes()),
        ("/payload.zip".into(), zip.clone()),
    ]);
    let url = format!("http://127.0.0.1:{port}/kit.json");

    // Happy path: verified, artifact downloaded and integrity-checked.
    let fetched = crewkit_core::kits::fetch_kit(&url, None, &crewkit_dir).unwrap();
    let local_zip = fetched.kit.plugins[0].zip.clone().unwrap();
    assert!(fetched.zips_dir.join(&local_zip).is_file());
    assert_eq!(
        crewkit_core::kits::sha256_hex(&std::fs::read(fetched.zips_dir.join(&local_zip)).unwrap()),
        sha
    );

    // Pinned-key mismatch is refused (CDN-compromise protection).
    let (_, other_public) = crewkit_core::kits::generate_keypair();
    let error = crewkit_core::kits::fetch_kit(&url, Some(&other_public), &crewkit_dir)
        .unwrap_err()
        .to_string();
    assert!(error.contains("publisher key changed"), "{error}");

    // A tampered manifest fails signature verification.
    let mut tampered = manifest.clone();
    tampered["name"] = serde_json::json!("Evil Kit");
    let tampered_bytes = serde_json::to_vec_pretty(&tampered).unwrap();
    let port2 = serve(vec![
        ("/kit.json".into(), tampered_bytes),
        ("/kit.json.sig".into(), signature.into_bytes()),
    ]);
    let error = crewkit_core::kits::fetch_kit(
        &format!("http://127.0.0.1:{port2}/kit.json"),
        None,
        &crewkit_dir,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("signature"), "{error}");
}

// --- Spec 1.0 manifest validation ---

fn minimal_kit(extra: &str) -> String {
    format!(
        r#"{{
          "id": "spec-kit",
          "name": "Spec Kit",
          "publisher": "Test",
          "marketplaceName": "specmkt"
          {extra}
        }}"#
    )
}

#[test]
fn kit_spec_version_gate() {
    use crewkit_core::kit::Kit;

    // Same major, newer minor: compatible.
    Kit::load(&minimal_kit(r#", "spec": "1.7""#)).unwrap();
    // Absent spec means 1.0.
    Kit::load(&minimal_kit("")).unwrap();
    // Different major: rejected with an update prompt.
    let error = Kit::load(&minimal_kit(r#", "spec": "2.0""#))
        .unwrap_err()
        .to_string();
    assert!(error.contains("update the app"), "{error}");
    // Garbage version string: rejected.
    let error = Kit::load(&minimal_kit(r#", "spec": "next""#))
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid spec version"), "{error}");
}

#[test]
fn kit_ignores_unknown_fields() {
    use crewkit_core::kit::Kit;

    // A 1.x manifest with fields this installer has never heard of must
    // still load — that is how minor spec revisions stay compatible.
    let kit = Kit::load(&minimal_kit(
        r#", "futureField": {"nested": true},
            "mcpServers": [{ "id": "srv", "url": "https://mcp.example.dev/mcp", "hints": ["x"] }]"#,
    ))
    .unwrap();
    assert_eq!(kit.mcp_servers.len(), 1);
}

#[test]
fn kit_requires_https() {
    use crewkit_core::kit::Kit;

    let error = Kit::load(&minimal_kit(
        r#", "mcpServers": [{ "id": "srv", "url": "http://mcp.example.dev/mcp" }]"#,
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("must use https"), "{error}");

    // Loopback stays allowed for local development.
    Kit::load(&minimal_kit(
        r#", "mcpServers": [{ "id": "srv", "url": "http://localhost:8080/mcp" }]"#,
    ))
    .unwrap();

    let error = Kit::load(&minimal_kit(
        r#", "telemetry": { "endpoint": "http://collect.example.dev" }"#,
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("telemetry endpoint"), "{error}");
}

#[test]
fn mcp_entry_extensions() {
    use crewkit_core::bridge::auth_status;
    use crewkit_core::kit::Kit;

    let kit = Kit::load(&minimal_kit(
        r#", "mcpServers": [
            { "id": "open-srv", "url": "https://a.example.dev/mcp", "auth": "none" },
            { "id": "oauth-srv", "url": "https://b.example.dev/mcp" },
            { "id": "future-srv", "url": "https://c.example.dev/mcp", "transport": "grpc" }
        ]"#,
    ))
    .unwrap();

    let by_id = |id: &str| kit.mcp_servers.iter().find(|s| s.id == id).unwrap();
    assert_eq!(by_id("oauth-srv").transport(), "http");
    assert!(by_id("oauth-srv").transport_supported());
    assert!(!by_id("future-srv").transport_supported());
    assert!(by_id("oauth-srv").uses_oauth());
    assert!(!by_id("open-srv").uses_oauth());

    // Open endpoints have no session to manage, so the auth report
    // omits them and the UI offers no authorize step.
    let tmp = tempfile::tempdir().unwrap();
    let auth = auth_status(&kit, tmp.path());
    let ids: Vec<_> = auth.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["oauth-srv", "future-srv"]);
}

/// A machine that uses Codex only through the desktop app: `~/.codex`
/// exists (with a config.toml) but no codex CLI is anywhere. MCP
/// servers are written straight into config.toml, so they must install
/// anyway; only plugins wait for a CLI, with an explanatory skip.
#[test]
fn codex_mcp_installs_without_a_cli() {
    use crewkit_core::inventory::Status;
    use crewkit_core::{Adapter, Engine, StepStatus};

    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(tmp.path());
    let (kit, zips_dir) = synth_kit(tmp.path());

    // A codex adapter with no discoverable CLI — only the config file
    // probe. Adapters are data, so the no-CLI machine is just this.
    let codex = Adapter::load(
        r#"{
          "id": "codex",
          "name": "Codex",
          "cli": { "pathNames": [], "bundledGlobs": [] },
          "files": { "config": "${codexHome}/config.toml" }
        }"#,
    )
    .unwrap();
    std::fs::create_dir_all(&paths.codex_home).unwrap();
    std::fs::write(paths.codex_home.join("config.toml"), "").unwrap();

    let bridge_source = tmp.path().join("bridge-source");
    std::fs::write(&bridge_source, b"#!/bin/sh\nexit 0\n").unwrap();
    let engine = Engine {
        paths: paths.clone(),
        adapters: vec![codex],
        kit,
        zips_dir,
        bridge_source,
        frontmatter_map: frontmatter_map(),
    };

    let report = engine.install(|_| {}).unwrap();
    assert!(
        !report.steps.iter().any(|s| s.status == StepStatus::Failed),
        "no step may fail: {:#?}",
        report.steps
    );
    // Plugins wait for a CLI — and say so.
    let plugin_skip = report
        .steps
        .iter()
        .find(|s| s.client == "codex" && s.step == "Codex plugins")
        .expect("plugins skip step");
    assert_eq!(plugin_skip.status, StepStatus::Skipped);
    assert!(plugin_skip.message.contains("plugins need the CLI"));
    // The MCP server landed in config.toml regardless (the exact TOML
    // rendering varies — check the parsed value).
    let toml = std::fs::read_to_string(paths.codex_home.join("config.toml")).unwrap();
    let doc: toml_edit::DocumentMut = toml.parse().unwrap();
    let cmd = doc["mcp_servers"]["test-mcp"]["command"]
        .as_str()
        .unwrap_or_default();
    assert!(cmd.ends_with("crewkit-bridge"), "{toml}");
    let installed = report
        .scan
        .items
        .iter()
        .find(|i| i.kind == "mcp" && i.client == "codex" && i.id == "test-mcp")
        .unwrap();
    assert_eq!(installed.status, Status::Installed);
}
