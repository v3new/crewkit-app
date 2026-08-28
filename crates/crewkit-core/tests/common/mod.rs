//! Shared test fixtures: a synthetic kit with real zip payloads, so the
//! test suite is self-contained and never depends on proprietary skills.

use std::path::{Path, PathBuf};
use std::process::Command;

use crewkit_core::kit::Kit;

/// Build a two-plugin kit under `root`: one Codex-plugin-shaped payload
/// ("toolbox") and one bare Agent Skill ("notes"), zipped the way
/// publishers ship them. Returns the kit and the zips directory.
pub fn synth_kit(root: &Path) -> (Kit, PathBuf) {
    let sources = root.join("src");
    let zips = root.join("zips");
    std::fs::create_dir_all(&zips).unwrap();

    // Plugin-shaped payload with its own manifest and one skill.
    let toolbox = sources.join("toolbox");
    std::fs::create_dir_all(toolbox.join(".codex-plugin")).unwrap();
    std::fs::write(
        toolbox.join(".codex-plugin/plugin.json"),
        r#"{ "name": "toolbox", "version": "9.9.9", "description": "test plugin" }"#,
    )
    .unwrap();
    let skill = toolbox.join("skills/toolbox-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: toolbox-skill\ndescription: A test skill.\n---\nBody.\n",
    )
    .unwrap();

    // Bare skill payload (SKILL.md at the root).
    let notes = sources.join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(
        notes.join("SKILL.md"),
        "---\nname: notes\ndescription: Another test skill.\n---\nBody.\n",
    )
    .unwrap();

    // bsdtar (`tar` on both macOS and Windows 10+) writes zip archives
    // when the output name ends in .zip (-a picks the format).
    for name in ["toolbox", "notes"] {
        let status = Command::new("tar")
            .arg("-a")
            .arg("-cf")
            .arg(zips.join(format!("{name}.zip")))
            .arg("-C")
            .arg(&sources)
            .arg(name)
            .status()
            .unwrap();
        assert!(status.success());
    }

    let kit = Kit::load(
        r#"{
          "id": "test-kit",
          "name": "Test Kit",
          "version": "1.0.0",
          "publisher": "Test Publisher",
          "marketplaceName": "testmkt",
          "mcpServers": [
            { "id": "test-mcp", "url": "https://mcp.example.dev/mcp" }
          ],
          "plugins": [
            { "name": "toolbox", "zip": "toolbox.zip", "description": "Test plugin." },
            { "name": "notes", "zip": "notes.zip", "description": "Bare skill." }
          ]
        }"#,
    )
    .unwrap();
    (kit, zips)
}
