//! Claude Cowork's local plugin store.
//!
//! Cowork does not read Claude Code's `~/.claude` plugins. Each Cowork
//! profile (`<Claude data dir>/local-agent-mode-sessions/<account>/<org>/`)
//! has its own Claude-Code-shaped store under `cowork_plugins/`, plus an
//! `enabledPlugins` map in `cowork_settings.json`. Sessions read it on start
//! and pass every enabled plugin to Claude Code as `--plugin-dir`. Install
//! paths must lie inside `cowork_plugins/` and be real directories, so the
//! staged marketplace is copied in rather than linked.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::error::{io_ctx, Result};
use crate::fsops;
use crate::inventory::iso_to_epoch_ms;
use crate::paths::Paths;

const SESSIONS_DIR: &str = "local-agent-mode-sessions";
const SKILLS_CACHE: &str = "skills-plugin";

/// One `<account>/<org>` profile. `dir` is where the files live; `seen`
/// is the same directory as the app addresses it (they differ only under
/// the Microsoft Store build's filesystem virtualization).
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub dir: PathBuf,
    pub seen: PathBuf,
}

impl Profile {
    pub fn plugins_dir(&self) -> PathBuf {
        self.dir.join("cowork_plugins")
    }

    fn installed_file(&self) -> PathBuf {
        self.plugins_dir().join("installed_plugins.json")
    }

    fn known_file(&self) -> PathBuf {
        self.plugins_dir().join("known_marketplaces.json")
    }

    fn settings_file(&self) -> PathBuf {
        self.dir.join("cowork_settings.json")
    }

    pub fn marketplace_dir(&self, marketplace: &str) -> PathBuf {
        self.plugins_dir().join("marketplaces").join(marketplace)
    }

    pub fn plugin_dir(&self, marketplace: &str, plugin: &str) -> PathBuf {
        self.marketplace_dir(marketplace)
            .join("plugins")
            .join(plugin)
    }

    /// A path under `dir`, spelled the way the app sees it.
    fn as_seen(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.dir).unwrap_or(path);
        self.seen.join(rel).to_string_lossy().into_owned()
    }
}

/// Every Cowork profile on this machine. Empty until Cowork has been
/// opened once (the app creates the profile directory on first use).
pub fn profiles(paths: &Paths) -> Vec<Profile> {
    let root = paths.claude_desktop_dir().join(SESSIONS_DIR);
    let seen_root = paths.claude_desktop_dir_seen_by_app().join(SESSIONS_DIR);
    let mut out = Vec::new();
    for account in subdirs(&root) {
        if account.file_name().is_some_and(|n| n == SKILLS_CACHE) {
            continue;
        }
        for org in subdirs(&account) {
            let rel = org.strip_prefix(&root).unwrap_or(&org).to_path_buf();
            out.push(Profile {
                seen: seen_root.join(&rel),
                dir: org,
            });
        }
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// Installed version and last-update time of a plugin, if recorded.
pub fn installed(profile: &Profile, plugin_id: &str) -> Option<(String, Option<u64>)> {
    let registry = fsops::read_json(&profile.installed_file()).ok().flatten()?;
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

pub fn enabled(profile: &Profile, plugin_id: &str) -> bool {
    fsops::read_json(&profile.settings_file())
        .ok()
        .flatten()
        .and_then(|s| s.get("enabledPlugins")?.get(plugin_id)?.as_bool())
        .unwrap_or(false)
}

/// Replace the profile's copy of the marketplace with the staged one and
/// register it. Plugins are enabled separately, per plugin.
pub fn sync_marketplace(profile: &Profile, marketplace: &str, staged: &Path) -> Result<()> {
    let dir = profile.marketplace_dir(marketplace);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(io_ctx(format!("removing {}", dir.display())))?;
    }
    fsops::copy_tree(staged, &dir)?;
    let seen = profile.as_seen(&dir);
    update_json(&profile.known_file(), |known| {
        known[marketplace] = json!({
            "source": { "source": "directory", "path": seen },
            "installLocation": seen,
            "lastUpdated": iso_now(),
        });
    })
}

/// Record and enable one plugin already present in the marketplace copy.
pub fn install_plugin(
    profile: &Profile,
    marketplace: &str,
    plugin: &str,
    version: &str,
) -> Result<()> {
    let plugin_id = format!("{plugin}@{marketplace}");
    let install_path = profile.as_seen(&profile.plugin_dir(marketplace, plugin));
    let now = iso_now();
    update_json(&profile.installed_file(), |registry| {
        registry["version"] = json!(2);
        let installed_at = registry["plugins"][&plugin_id]
            .get(0)
            .and_then(|e| e.get("installedAt"))
            .cloned()
            .unwrap_or_else(|| json!(now));
        registry["plugins"][&plugin_id] = json!([{
            "scope": "user",
            "installPath": install_path,
            "version": version,
            "installedAt": installed_at,
            "lastUpdated": now,
        }]);
    })?;
    update_json(&profile.settings_file(), |settings| {
        settings["enabledPlugins"][&plugin_id] = json!(true);
    })
}

/// Remove a plugin from the profile. The marketplace copy goes too once
/// no plugin of it is left. Returns whether anything was removed.
pub fn remove_plugin(profile: &Profile, marketplace: &str, plugin: &str) -> Result<bool> {
    let plugin_id = format!("{plugin}@{marketplace}");
    let mut removed = false;
    update_json(&profile.installed_file(), |registry| {
        if let Some(plugins) = registry["plugins"].as_object_mut() {
            removed |= plugins.remove(&plugin_id).is_some();
        }
    })?;
    update_json(&profile.settings_file(), |settings| {
        if let Some(enabled) = settings["enabledPlugins"].as_object_mut() {
            removed |= enabled.remove(&plugin_id).is_some();
        }
    })?;
    let dir = profile.plugin_dir(marketplace, plugin);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(io_ctx(format!("removing {}", dir.display())))?;
        removed = true;
    }
    let suffix = format!("@{marketplace}");
    let others_left = fsops::read_json(&profile.installed_file())?
        .and_then(|r| {
            r.get("plugins")?
                .as_object()
                .map(|p| p.keys().any(|k| k.ends_with(&suffix)))
        })
        .unwrap_or(false);
    if !others_left {
        let mkt = profile.marketplace_dir(marketplace);
        if mkt.exists() {
            std::fs::remove_dir_all(&mkt).map_err(io_ctx(format!("removing {}", mkt.display())))?;
        }
        update_json(&profile.known_file(), |known| {
            known.as_object_mut().map(|k| k.remove(marketplace));
        })?;
    }
    Ok(removed)
}

fn update_json(path: &Path, edit: impl FnOnce(&mut Value)) -> Result<()> {
    let mut value = fsops::read_json(path)?.unwrap_or_else(|| json!({}));
    edit(&mut value);
    let text = serde_json::to_string_pretty(&value).expect("json serializes");
    fsops::atomic_write(path, text.as_bytes())
}

/// `YYYY-MM-DDTHH:MM:SSZ` for now (Howard Hinnant's civil-from-days).
fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_now_roundtrips_through_the_inventory_parser() {
        let now = iso_now();
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let parsed = iso_to_epoch_ms(&now).unwrap() / 1000;
        assert!(secs.abs_diff(parsed) <= 1, "{now} vs {secs}");
    }
}
