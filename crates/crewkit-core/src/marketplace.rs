use std::path::{Path, PathBuf};

use serde_json::json;

use crate::error::{io_ctx, Error, Result};
use crate::fsops;
use crate::kit::{Kit, KitPlugin};
use crate::translate::{self, FrontmatterMap};

const CODEX_MANIFEST_DIR: &str = ".codex-plugin";
const CLAUDE_MANIFEST_DIR: &str = ".claude-plugin";

/// Build the staged marketplace directory that both ecosystems install from.
///
/// One directory serves both clients: Claude reads `.claude-plugin/marketplace.json`,
/// Codex reads `.agents/plugins/marketplace.json`, and each plugin carries both
/// a `.claude-plugin/` and a `.codex-plugin/` manifest. Kit payload zips are
/// normalized on the way in: a zip may contain a ready-made plugin (either
/// ecosystem's manifest) or a bare skill folder, which gets wrapped into a
/// single-skill plugin.
///
/// The build happens in a `.staging` sibling and is swapped in with a rename,
/// so clients never observe a half-built marketplace. Every skill passes
/// the translate check (frontmatter mapping table + OpenAI UI metadata);
/// the returned warnings surface in the install log as partial-support notes.
pub fn stage(kit: &Kit, zips_dir: &Path, dest: &Path, map: &FrontmatterMap) -> Result<Vec<String>> {
    let staging = dest.with_extension("staging");
    let _ = std::fs::remove_dir_all(&staging);
    let plugins_dir = staging.join("plugins");
    std::fs::create_dir_all(&plugins_dir)
        .map_err(io_ctx(format!("creating {}", plugins_dir.display())))?;

    let mut warnings = Vec::new();
    for plugin in kit.active_plugins() {
        let Some(zip_name) = plugin.zip.as_deref() else {
            return Err(Error::Invalid(format!(
                "plugin `{}` has no local payload — its artifact was not fetched",
                plugin.name
            )));
        };
        let zip = zips_dir.join(zip_name);
        if !zip.exists() {
            return Err(Error::Invalid(format!(
                "kit payload missing: {}",
                zip.display()
            )));
        }
        let extract = staging.join(".extract").join(&plugin.name);
        fsops::extract_zip(&zip, &extract)?;
        let payload = find_payload_root(&extract).ok_or_else(|| {
            Error::Invalid(format!(
                "{zip_name}: no plugin manifest or SKILL.md found inside"
            ))
        })?;

        let target = plugins_dir.join(&plugin.name);
        match payload {
            Payload::Plugin(root) => fsops::copy_tree(&root, &target)?,
            Payload::BareSkill(root) => {
                // Wrap a bare skill folder into a single-skill plugin.
                let skill_name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| plugin.name.clone());
                fsops::copy_tree(&root, &target.join("skills").join(skill_name))?;
            }
        }
        ensure_plugin_manifests(&target, kit, plugin)?;

        let skills_dir = target.join("skills");
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                if entry.path().join("SKILL.md").is_file() {
                    warnings.extend(translate::process_skill(
                        &entry.path(),
                        map,
                        &kit.publisher,
                    )?);
                }
            }
        }
    }

    std::fs::remove_dir_all(staging.join(".extract")).map_err(io_ctx("cleaning extract dir"))?;
    write_marketplace_manifests(&staging, kit)?;

    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .map_err(io_ctx(format!("removing old {}", dest.display())))?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(io_ctx(format!("creating {}", parent.display())))?;
    }
    std::fs::rename(&staging, dest).map_err(io_ctx(format!("activating {}", dest.display())))?;
    Ok(warnings)
}

enum Payload {
    Plugin(PathBuf),
    BareSkill(PathBuf),
}

/// Breadth-first search (a few levels deep) for the real content root:
/// zips typically wrap their payload in one or two directory levels.
fn find_payload_root(extract_dir: &Path) -> Option<Payload> {
    let mut queue = vec![extract_dir.to_path_buf()];
    for _depth in 0..4 {
        let mut next = Vec::new();
        for dir in &queue {
            if dir.join(CODEX_MANIFEST_DIR).is_dir() || dir.join(CLAUDE_MANIFEST_DIR).is_dir() {
                return Some(Payload::Plugin(dir.clone()));
            }
            if dir.join("SKILL.md").is_file() {
                return Some(Payload::BareSkill(dir.clone()));
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if entry.path().is_dir() && name != "__MACOSX" {
                        next.push(entry.path());
                    }
                }
            }
        }
        queue = next;
    }
    None
}

/// Make sure the plugin directory carries a manifest for both ecosystems.
/// An existing manifest is reused verbatim for the missing side; when the
/// payload was a bare skill, a minimal manifest is generated from kit data.
fn ensure_plugin_manifests(plugin_dir: &Path, kit: &Kit, plugin: &KitPlugin) -> Result<()> {
    let codex_manifest = plugin_dir.join(CODEX_MANIFEST_DIR).join("plugin.json");
    let claude_manifest = plugin_dir.join(CLAUDE_MANIFEST_DIR).join("plugin.json");

    let base = fsops::read_json(&codex_manifest)?
        .or(fsops::read_json(&claude_manifest)?)
        .unwrap_or_else(|| {
            json!({
                "name": plugin.name,
                "description": plugin.description,
                "version": "1.0.0",
                "author": { "name": kit.publisher },
            })
        });

    for manifest in [&codex_manifest, &claude_manifest] {
        if !manifest.exists() {
            let text = serde_json::to_string_pretty(&base).expect("manifest serializes");
            fsops::atomic_write(manifest, text.as_bytes())?;
        }
    }
    Ok(())
}

fn write_marketplace_manifests(root: &Path, kit: &Kit) -> Result<()> {
    let claude = json!({
        "name": kit.marketplace_name,
        "owner": { "name": kit.publisher },
        "plugins": kit.active_plugins().map(|p| json!({
            "name": p.name,
            "source": format!("./plugins/{}", p.name),
            "description": p.description,
        })).collect::<Vec<_>>(),
    });
    let codex = json!({
        "name": kit.marketplace_name,
        "interface": { "displayName": kit.name },
        "plugins": kit.active_plugins().map(|p| json!({
            "name": p.name,
            "source": { "source": "local", "path": format!("./plugins/{}", p.name) },
            "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" },
        })).collect::<Vec<_>>(),
    });

    for (rel, manifest) in [
        (".claude-plugin/marketplace.json", &claude),
        (".agents/plugins/marketplace.json", &codex),
    ] {
        let text = serde_json::to_string_pretty(manifest).expect("manifest serializes");
        fsops::atomic_write(&root.join(rel), text.as_bytes())?;
    }
    Ok(())
}
