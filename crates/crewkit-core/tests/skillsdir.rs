//! The no-CLI channel for Claude Code: kit plugins copied into
//! `<config>/skills/<name>/`, where Claude Code loads them as
//! `<name>@skills-dir`. Sandboxed under a temp root.

mod common;

use std::path::Path;

use common::synth_kit;
use crewkit_core::inventory::Status;
use crewkit_core::{Adapter, Engine, Paths, ScanReport, StepStatus};

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Claude Code with a config dir on disk but no CLI anywhere — the
/// machine of a user who only ever ran the desktop app.
fn engine(root: &Path, claude_cli: Option<&Path>) -> Engine {
    let paths = Paths::rooted(root);
    std::fs::create_dir_all(&paths.claude_config_dir).unwrap();
    std::fs::write(paths.claude_config_dir.join("settings.json"), "{}").unwrap();
    let globs = claude_cli
        .map(|p| format!("[{:?}]", p.to_string_lossy()))
        .unwrap_or_else(|| "[]".into());
    let adapter = Adapter::load(&format!(
        r#"{{
          "id": "claude-code",
          "name": "Claude Code",
          "cli": {{ "pathNames": [], "bundledGlobs": {globs} }},
          "files": {{ "settings": "${{claudeConfigDir}}/settings.json" }}
        }}"#
    ))
    .unwrap();
    let (kit, zips_dir) = synth_kit(root);
    let bridge_source = root.join("bridge-source");
    std::fs::write(&bridge_source, b"#!/bin/sh\nexit 0\n").unwrap();
    Engine {
        paths,
        adapters: vec![adapter],
        kit,
        zips_dir,
        bridge_source: Some(bridge_source),
        frontmatter_map: crewkit_core::assets::frontmatter_map().unwrap(),
    }
}

fn item<'a>(scan: &'a ScanReport, id: &str) -> &'a crewkit_core::inventory::ItemState {
    scan.items
        .iter()
        .find(|i| i.kind == "plugin" && i.client == "claude-code" && i.id == id)
        .unwrap()
}

#[test]
fn plugins_install_into_skills_dir_without_a_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(tmp.path(), None);
    let skills = engine.paths.claude_config_dir.join("skills");

    let report = engine.install(|_| {}).unwrap();
    assert!(
        !report.steps.iter().any(|s| s.status == StepStatus::Failed),
        "{:#?}",
        report.steps
    );
    let toolbox = skills.join("toolbox");
    assert!(toolbox.join(".claude-plugin/plugin.json").is_file());
    assert!(toolbox.join("skills/toolbox-skill/SKILL.md").is_file());
    assert!(toolbox.join(".crewkit.json").is_file());
    assert!(skills.join("notes/.claude-plugin/plugin.json").is_file());
    assert!(!skills.join(".toolbox.crewkit-tmp").exists());
    let row = item(&report.scan, "toolbox@testmkt");
    assert_eq!(row.status, Status::Installed);
    assert_eq!(row.version.as_deref(), Some("9.9.9"));
    assert!(row.path.ends_with("toolbox"), "{}", row.path);
    // MCP servers still need the CLI, and the report says so.
    let skip = report
        .steps
        .iter()
        .find(|s| s.step == "Claude install")
        .unwrap();
    assert!(skip.message.contains("MCP servers need it"), "{skip:?}");

    // Again: nothing to do.
    let again = engine.install(|_| {}).unwrap();
    let skips = again
        .steps
        .iter()
        .filter(|s| s.step.starts_with("Install plugin") && s.status == StepStatus::Skipped)
        .count();
    assert_eq!(skips, 2, "{:#?}", again.steps);

    // The user switches it off in Claude Code; the scan follows, and an
    // install switches it back on.
    let settings = engine.paths.claude_config_dir.join("settings.json");
    std::fs::write(
        &settings,
        r#"{ "enabledPlugins": { "toolbox@skills-dir": false } }"#,
    )
    .unwrap();
    assert_eq!(
        item(&engine.scan().unwrap(), "toolbox@testmkt").status,
        Status::NotInstalled
    );
    engine.install(|_| {}).unwrap();
    assert_eq!(
        json(&settings)["enabledPlugins"]["toolbox@skills-dir"],
        true
    );

    // Removal takes the copy away.
    engine
        .remove_item("plugin", "toolbox@testmkt", |_| {})
        .unwrap();
    assert!(!toolbox.exists());
    assert!(skills.join("notes").exists());

    // A switch-off outlives the removal; one install still brings the
    // plugin back on.
    std::fs::write(
        &settings,
        r#"{ "enabledPlugins": { "toolbox@skills-dir": false } }"#,
    )
    .unwrap();
    let back = engine.install(|_| {}).unwrap();
    assert_eq!(
        json(&settings)["enabledPlugins"]["toolbox@skills-dir"],
        true
    );
    assert_eq!(
        item(&back.scan, "toolbox@testmkt").status,
        Status::Installed
    );
}

/// A skill folder of the same name that CrewKit did not create is the
/// user's: it is never overwritten and never removed.
#[test]
fn foreign_skill_folder_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine(tmp.path(), None);
    let mine = engine.paths.claude_config_dir.join("skills/toolbox");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::write(mine.join("SKILL.md"), "mine").unwrap();

    let report = engine.install(|_| {}).unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.step == "Install plugin toolbox@testmkt")
        .unwrap();
    assert_eq!(step.status, StepStatus::Failed);
    assert!(step.message.contains("not created by CrewKit"), "{step:?}");
    engine
        .remove_item("plugin", "toolbox@testmkt", |_| {})
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(mine.join("SKILL.md")).unwrap(),
        "mine"
    );
}

/// Once a CLI shows up, the marketplace install takes over and the
/// skills-dir copy goes — otherwise the skills would load twice.
#[test]
#[cfg(unix)]
fn marketplace_install_replaces_the_skills_dir_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let skills = Paths::rooted(tmp.path()).claude_config_dir.join("skills");
    engine(tmp.path(), None).install(|_| {}).unwrap();
    assert!(skills.join("toolbox").is_dir());

    let cli = fake_cli(tmp.path());
    let with_cli = engine(tmp.path(), Some(&cli));
    let report = with_cli.install(|_| {}).unwrap();
    assert!(
        report
            .steps
            .iter()
            .any(|s| s.step == "Remove toolbox@skills-dir" && s.status == StepStatus::Ok),
        "{:#?}",
        report.steps
    );
    assert!(!skills.join("toolbox").exists());
    assert!(!skills.join("notes").exists());
    let row = item(&report.scan, "toolbox@testmkt");
    assert_eq!(row.status, Status::Installed);
    assert!(row.path.ends_with("settings.json"), "{}", row.path);
}

/// A marketplace install the user switched off is reinstalled, not
/// skipped because a skills-dir copy also reads as installed — skipping
/// would drop the copy and leave nothing loading.
#[test]
#[cfg(unix)]
fn disabled_marketplace_plugin_is_reinstalled_not_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    engine(tmp.path(), None).install(|_| {}).unwrap();
    let config = Paths::rooted(tmp.path()).claude_config_dir;
    std::fs::create_dir_all(config.join("plugins")).unwrap();
    std::fs::write(
        config.join("plugins/installed_plugins.json"),
        r#"{"version":2,"plugins":{"toolbox@testmkt":[{"version":"9.9.9"}]}}"#,
    )
    .unwrap();
    let with_cli = engine(tmp.path(), Some(&fake_cli(tmp.path())));
    // engine() resets settings.json to {}; switch the marketplace id off.
    std::fs::write(
        config.join("settings.json"),
        r#"{ "enabledPlugins": { "toolbox@testmkt": false } }"#,
    )
    .unwrap();
    let report = with_cli.install(|_| {}).unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.step == "Install plugin toolbox@testmkt")
        .unwrap();
    assert_eq!(step.status, StepStatus::Ok, "{:#?}", report.steps);
    assert_eq!(
        json(&config.join("settings.json"))["enabledPlugins"]["toolbox@testmkt"],
        true
    );
    assert!(!config.join("skills/toolbox").exists());
}

/// A fake claude: `plugin install` records both plugins the way the real
/// CLI does; everything else succeeds silently.
#[cfg(unix)]
fn fake_cli(root: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let cli = root.join("fake-claude");
    std::fs::write(
        &cli,
        r#"#!/bin/sh
case "$1 $2" in
  "--version ") echo "2.1.299 (Claude Code)" ;;
  "plugin install")
    mkdir -p "$CLAUDE_CONFIG_DIR/plugins"
    echo '{"version":2,"plugins":{"toolbox@testmkt":[{"version":"9.9.9"}],"notes@testmkt":[{"version":"1.0.0"}]}}' \
      > "$CLAUDE_CONFIG_DIR/plugins/installed_plugins.json"
    echo '{"enabledPlugins":{"toolbox@testmkt":true,"notes@testmkt":true}}' \
      > "$CLAUDE_CONFIG_DIR/settings.json" ;;
esac
exit 0
"#,
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli
}
