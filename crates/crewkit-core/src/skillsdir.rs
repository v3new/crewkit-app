//! Claude Code's skills-directory plugins — the install channel that
//! needs no `claude` CLI.
//!
//! Since Claude Code 2.1.157 a directory `<config>/skills/<name>/` that
//! holds `.claude-plugin/plugin.json` loads as plugin `<name>@skills-dir`
//! on every start, with no marketplace and no install step. CrewKit uses
//! it only when it cannot find the CLI: a marketplace install of the same
//! plugin would load the same skills twice under a second id. The copy
//! carries a marker naming the kit's marketplace, so a user's own skill
//! folder of the same name is never overwritten or removed. A user who
//! switches the plugin off gets `enabledPlugins["<name>@skills-dir"] =
//! false` in settings.json.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{io_ctx, Error, Result};
use crate::fsops::{self, Snapshotter};
use crate::installer::{StepReport, StepStatus};
use crate::inventory::claude_plugin_install;
use crate::kit::{Kit, KitPlugin};
use crate::paths::Paths;

/// Marketplace part of the plugin id Claude Code gives these plugins.
pub const SOURCE: &str = "skills-dir";
/// Ownership marker written into each copy. Claude Code ignores it.
const MARKER: &str = ".crewkit.json";

pub fn plugin_dir(paths: &Paths, name: &str) -> PathBuf {
    paths.claude_config_dir.join("skills").join(name)
}

/// Plugin names become directory names under `skills/`: anything but a
/// plain `[A-Za-z0-9_-]` name could point outside it, and Claude Code
/// skips dot-prefixed directories.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A dot-prefixed sibling Claude Code never loads: where a copy is built
/// before the swap, and where an old one waits to be deleted.
fn scratch_dir(paths: &Paths, name: &str, purpose: &str) -> PathBuf {
    paths
        .claude_config_dir
        .join("skills")
        .join(format!(".{name}.crewkit-{purpose}"))
}

fn remove_if_exists(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(io_ctx(format!("removing {}", dir.display())))?;
    }
    Ok(())
}

pub fn plugin_id(name: &str) -> String {
    format!("{name}@{SOURCE}")
}

/// Whether CrewKit put this copy there for this marketplace.
fn owned(paths: &Paths, marketplace: &str, name: &str) -> bool {
    valid_name(name)
        && fsops::read_json(&plugin_dir(paths, name).join(MARKER))
            .ok()
            .flatten()
            .and_then(|m| m.get("marketplace")?.as_str().map(|s| s == marketplace))
            .unwrap_or(false)
}

/// Version + install time of CrewKit's copy, when there is one.
pub fn installed(paths: &Paths, marketplace: &str, name: &str) -> Option<(String, Option<u64>)> {
    if !owned(paths, marketplace, name) {
        return None;
    }
    let dir = plugin_dir(paths, name);
    let version = fsops::read_json(&dir.join(".claude-plugin/plugin.json"))
        .ok()
        .flatten()
        .and_then(|m| m.get("version")?.as_str().map(String::from))
        .unwrap_or_else(|| "0.0.0".into());
    let updated = std::fs::metadata(dir.join(MARKER))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    Some((version, updated))
}

/// Claude Code loads these plugins unless settings.json switches the id
/// off explicitly — absence means enabled.
pub fn enabled(paths: &Paths, name: &str) -> bool {
    enabled_flag(paths, name) != Some(false)
}

fn enabled_flag(paths: &Paths, name: &str) -> Option<bool> {
    fsops::read_json(&paths.claude_config_dir.join("settings.json"))
        .ok()
        .flatten()?
        .get("enabledPlugins")?
        .get(plugin_id(name))?
        .as_bool()
}

/// What `install` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Placed {
    Installed,
    Updated,
    Unchanged,
}

/// Copy the staged plugin into the skills directory, replacing an older
/// copy of ours. A folder of that name that CrewKit does not own is left
/// alone and reported as an error.
pub fn install(paths: &Paths, marketplace: &str, name: &str, staged: &Path) -> Result<Placed> {
    if !valid_name(name) {
        return Err(Error::Invalid(format!(
            "plugin name `{name}` cannot be a skills directory"
        )));
    }
    let dir = plugin_dir(paths, name);
    let staged_version = fsops::read_json(&staged.join(".claude-plugin/plugin.json"))?
        .and_then(|m| m.get("version")?.as_str().map(String::from))
        .unwrap_or_else(|| "0.0.0".into());
    let current = installed(paths, marketplace, name).map(|(v, _)| v);
    if dir.exists() && current.is_none() {
        return Err(Error::Invalid(format!(
            "{} already exists and was not created by CrewKit",
            dir.display()
        )));
    }
    if current.as_deref() == Some(staged_version.as_str()) {
        return Ok(Placed::Unchanged);
    }
    // Build the new copy beside the old one and swap by renames, so
    // Claude Code never sees a half-written plugin, and a failure part
    // way leaves either the old copy or the new one in place — never a
    // marker-less leftover that would read as the user's own folder.
    let tmp = scratch_dir(paths, name, "tmp");
    let old = scratch_dir(paths, name, "old");
    remove_if_exists(&tmp)?;
    let _ = remove_if_exists(&old);
    fsops::copy_tree(staged, &tmp)?;
    let marker = json!({ "marketplace": marketplace, "managedBy": "crewkit" });
    let text = serde_json::to_string_pretty(&marker).expect("json serializes");
    fsops::atomic_write(&tmp.join(MARKER), text.as_bytes())?;
    if dir.exists() {
        std::fs::rename(&dir, &old).map_err(io_ctx(format!("moving {} aside", dir.display())))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &dir) {
        let _ = std::fs::rename(&old, &dir);
        return Err(io_ctx(format!("moving into {}", dir.display()))(e));
    }
    // A locked file can keep the old copy around; it is invisible to
    // Claude Code and the next run retries.
    let _ = remove_if_exists(&old);
    Ok(if current.is_some() {
        Placed::Updated
    } else {
        Placed::Installed
    })
}

/// Turn an explicit `false` for our id back on — installing the kit is
/// an explicit request for the plugin. Returns whether settings changed.
pub fn enable(paths: &Paths, name: &str) -> Result<bool> {
    if enabled_flag(paths, name) != Some(false) {
        return Ok(false);
    }
    // Write through a symlinked settings.json (dotfile setups) instead
    // of replacing the link with a file.
    let link = paths.claude_config_dir.join("settings.json");
    let path = std::fs::canonicalize(&link).unwrap_or(link);
    let mut settings: Value = fsops::read_json(&path)?.unwrap_or_else(|| json!({}));
    settings["enabledPlugins"][plugin_id(name)] = json!(true);
    let text = serde_json::to_string_pretty(&settings).expect("json serializes");
    fsops::atomic_write(&path, text.as_bytes())?;
    Ok(true)
}

/// The installer's no-CLI path: place every wanted plugin in the skills
/// directory and report a step per plugin. A plugin the CLI installed
/// from the marketplace earlier is left as it is. Returns whether
/// anything changed.
pub fn install_steps(
    paths: &Paths,
    kit: &Kit,
    plugins: &[&KitPlugin],
    marketplace_dir: &Path,
    snap: &mut Snapshotter,
    emit: &mut dyn FnMut(StepReport),
) -> bool {
    let mut changed = false;
    for &plugin in plugins {
        let id = kit.plugin_id(plugin);
        let report = |step: String, status: StepStatus, message: String| StepReport {
            step,
            client: "claude-code".into(),
            status,
            message,
        };
        let step = format!("Install plugin {id}");
        if claude_plugin_install(paths, &id).is_some() {
            emit(report(
                step,
                StepStatus::Skipped,
                "already installed from the marketplace".into(),
            ));
            continue;
        }
        let staged = marketplace_dir.join("plugins").join(&plugin.name);
        let mkt = &kit.marketplace_name;
        let where_ = format!(
            "copied to {} — loads as {} (no claude CLI found)",
            plugin_dir(paths, &plugin.name).display(),
            plugin_id(&plugin.name)
        );
        let placed = match install(paths, mkt, &plugin.name, &staged) {
            Ok(placed) => placed,
            Err(e) => {
                emit(report(step, StepStatus::Failed, e.to_string()));
                continue;
            }
        };
        // A switch-off the user left behind (it outlives a removal) would
        // keep even a fresh copy from loading: installing turns it back on.
        let mut switched_on = false;
        if !enabled(paths, &plugin.name) {
            let _ = snap.backup(&paths.claude_config_dir.join("settings.json"));
            match enable(paths, &plugin.name) {
                Ok(done) => switched_on = done,
                Err(e) => {
                    emit(report(step, StepStatus::Failed, e.to_string()));
                    continue;
                }
            }
        }
        let version = installed(paths, mkt, &plugin.name)
            .map(|(v, _)| v)
            .unwrap_or_default();
        changed |= switched_on || placed != Placed::Unchanged;
        emit(match placed {
            Placed::Installed => report(step, StepStatus::Ok, where_),
            Placed::Updated => report(
                format!("Update plugin {id} to v{version}"),
                StepStatus::Ok,
                where_,
            ),
            Placed::Unchanged if switched_on => {
                report(step, StepStatus::Ok, "switched back on".into())
            }
            Placed::Unchanged => report(
                step,
                StepStatus::Skipped,
                format!("already installed (v{version})"),
            ),
        });
    }
    changed
}

/// Remove CrewKit's copy. Returns whether anything was removed.
pub fn remove(paths: &Paths, marketplace: &str, name: &str) -> Result<bool> {
    if !owned(paths, marketplace, name) {
        return Ok(false);
    }
    // Rename first: if deleting fails part way (a locked file on
    // Windows), the leftover is a dot-dir Claude Code ignores, not a
    // marker-less folder that would read as the user's.
    let dir = plugin_dir(paths, name);
    let old = scratch_dir(paths, name, "old");
    let _ = remove_if_exists(&old);
    std::fs::rename(&dir, &old).map_err(io_ctx(format!("removing {}", dir.display())))?;
    let _ = remove_if_exists(&old);
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_plain_names_become_directories() {
        for ok in ["toolbox", "my-kit_2"] {
            assert!(super::valid_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", ".hidden", "-x", "a/b", "/abs", "a\\b", "a b"] {
            assert!(!super::valid_name(bad), "{bad}");
        }
    }
}
