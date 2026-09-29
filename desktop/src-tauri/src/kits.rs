use std::path::PathBuf;

use crewkit_core::kits::{self, Auth, KitRegistry, KitSource};
use crewkit_core::updater::{ItemRef, UpdateState};
use crewkit_core::{Engine, Kit, Paths};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::items::with_update_lock;

pub fn crewkit_dir() -> PathBuf {
    Paths::from_env().crewkit_dir()
}

pub fn bridge_source(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .resource_dir()
        .map_err(|e| e.to_string())?
        .join("bin")
        .join(crewkit_core::bridge::BRIDGE_BIN_NAME))
}

pub fn engine_for(app: &AppHandle, source: &KitSource) -> Result<Engine, String> {
    let dir = crewkit_dir();
    let kit = kits::load_cached(&dir, source).map_err(|e| e.to_string())?;
    Engine::new(
        Paths::from_env(),
        kit,
        kits::artifacts_dir(&dir, &source.id),
        Some(bridge_source(app)?),
    )
    .map_err(|e| e.to_string())
}

pub fn source_for(kit_id: &str) -> Result<KitSource, String> {
    KitRegistry::load(&crewkit_dir())
        .map_err(|e| e.to_string())?
        .find(kit_id)
        .cloned()
        .map_err(|e| e.to_string())
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct KitCard {
    kit: Kit,
    source: String,
    channel: String,
    bundle: Option<String>,
    /// Items that appeared in the chosen bundle since the kit was added
    /// and are not installed yet.
    new_items: Vec<ItemRef>,
    /// Why this kit could not be loaded. A degraded card is still
    /// rendered — the UI never dead-ends on a single kit's failure.
    error: Option<String>,
    /// Published behind a login and this machine has no live session.
    needs_auth: bool,
}

fn placeholder_kit(id: &str) -> Kit {
    Kit {
        spec: None,
        id: id.into(),
        name: id.into(),
        version: None,
        publisher: String::new(),
        publisher_key: None,
        homepage: None,
        marketplace_name: id.into(),
        channels: Default::default(),
        telemetry: None,
        bundles: Vec::new(),
        mcp_servers: Vec::new(),
        plugins: Vec::new(),
    }
}

#[tauri::command]
pub fn list_kits() -> Result<Vec<KitCard>, String> {
    let dir = crewkit_dir();
    let state = UpdateState::load(&dir).map_err(|e| e.to_string())?;
    let mut cards = Vec::new();
    for source in KitRegistry::load(&dir).map_err(|e| e.to_string())?.kits {
        let fetch_error = if kits::cache_path(&dir, &source.id).exists() {
            None
        } else {
            kits::refresh(&source, &dir, Auth::Silent).err()
        };
        let (kit, error) = match kits::load_cached(&dir, &source) {
            Ok(kit) => (kit, None),
            Err(load_error) => (
                placeholder_kit(&source.id),
                Some(fetch_error.unwrap_or(load_error).to_string()),
            ),
        };
        let needs_auth = !kits::kit_is_authorized(&source.source, &dir)
            && error.as_deref().is_some_and(|m| m.contains("sign in"));
        cards.push(KitCard {
            kit,
            source: source.source.clone(),
            channel: source.channel.clone(),
            bundle: source.bundle.clone(),
            new_items: state.new_items.get(&source.id).cloned().unwrap_or_default(),
            error,
            needs_auth,
        });
    }
    Ok(cards)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct KitPreview {
    id: String,
    name: String,
    publisher: String,
    channels: Vec<String>,
    bundles: Vec<BundleOption>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BundleOption {
    id: String,
    display_name: Option<String>,
}

/// First step of adding a kit: fetch and verify the manifest, and tell
/// the UI which channel and bundle choices the publisher offers.
#[tauri::command]
pub async fn inspect_kit(url: String) -> Result<KitPreview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let kit = kits::fetch_kit(&url, None, &crewkit_dir(), Auth::Interactive)
            .map_err(|e| e.to_string())?
            .kit;
        Ok(KitPreview {
            id: kit.id,
            name: kit.name,
            publisher: kit.publisher,
            channels: kit.channels.keys().cloned().collect(),
            bundles: kit
                .bundles
                .iter()
                .map(|b| BundleOption {
                    id: b.id.clone(),
                    display_name: b.display_name.clone(),
                })
                .collect(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Second step: register the kit on the chosen channel and bundle. Both
/// are fixed for the kit's lifetime — changing them means re-adding it.
#[tauri::command]
pub async fn add_kit(
    app: AppHandle,
    url: String,
    channel: Option<String>,
    bundle: Option<String>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = crewkit_dir();
        let entry =
            kits::fetch_kit(&url, None, &dir, Auth::Interactive).map_err(|e| e.to_string())?;
        let channel = channel.unwrap_or_else(|| "stable".into());
        let manifest_url = match entry.kit.channels.get(&channel) {
            Some(target) => kits::resolve_url(&url, target),
            None if channel == "stable" => url.clone(),
            None => return Err(format!("kit has no `{channel}` channel")),
        };
        let fetched = if manifest_url == url {
            entry
        } else {
            kits::fetch_kit(
                &manifest_url,
                entry.kit.publisher_key.as_deref(),
                &dir,
                Auth::Interactive,
            )
            .map_err(|e| e.to_string())?
        };
        let kit = fetched.kit;
        match (&bundle, kit.bundles.is_empty()) {
            (None, false) => return Err("choose a bundle for this kit".into()),
            (Some(id), _) if !kit.bundles.iter().any(|b| &b.id == id) => {
                return Err(format!("kit has no `{id}` bundle"))
            }
            _ => {}
        }
        let mut registry = KitRegistry::load(&dir).map_err(|e| e.to_string())?;
        if registry.kits.iter().any(|k| k.id == kit.id) {
            return Err(format!("kit `{}` is already added", kit.id));
        }
        for existing in &registry.kits {
            if let Ok(other) = kits::load_cached(&dir, existing) {
                if other.marketplace_name == kit.marketplace_name {
                    return Err(format!(
                        "marketplace name `{}` is already used by kit `{}`",
                        kit.marketplace_name, existing.id
                    ));
                }
            }
        }
        kits::write_cache(&dir, &kit).map_err(|e| e.to_string())?;
        registry.kits.push(KitSource {
            id: kit.id,
            source: manifest_url,
            channel,
            pinned_key: kit.publisher_key,
            bundle,
        });
        registry.save(&dir).map_err(|e| e.to_string())?;
        let _ = app.emit("kits-changed", ());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Removing a kit uninstalls its items from every client first, so
/// re-adding it on another channel or bundle starts from a clean slate.
#[tauri::command]
pub async fn remove_kit(app: AppHandle, kit_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = crewkit_dir();
        let source = source_for(&kit_id)?;
        if let Ok(engine) = engine_for(&app, &source) {
            with_update_lock(|| {
                let items: Vec<(String, String)> = engine
                    .kit
                    .active_mcp_servers()
                    .map(|s| ("mcp".to_string(), s.id.clone()))
                    .chain(
                        engine
                            .kit
                            .active_plugins()
                            .map(|p| ("plugin".to_string(), engine.kit.plugin_id(p))),
                    )
                    .collect();
                for (kind, id) in items {
                    let report = engine
                        .remove_item(&kind, &id, |step| {
                            let _ = app.emit("install-step", step);
                        })
                        .map_err(|e| e.to_string())?;
                    let _ = app.emit("background-report", report);
                }
                Ok(())
            })?;
        }
        let mut registry = KitRegistry::load(&dir).map_err(|e| e.to_string())?;
        registry.kits.retain(|k| k.id != kit_id);
        registry.save(&dir).map_err(|e| e.to_string())?;
        let _ = UpdateState::modify(&dir, |state| {
            state.new_items.remove(&kit_id);
        });
        let _ = std::fs::remove_file(kits::cache_path(&dir, &kit_id));
        let _ = std::fs::remove_dir_all(kits::artifacts_dir(&dir, &kit_id));
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn authorize_kit(kit_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let source = source_for(&kit_id)?;
        kits::login_to_kit(&source.source, &crewkit_dir()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn deauthorize_kit(kit_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let source = source_for(&kit_id)?;
        kits::logout_from_kit(&source.source, &crewkit_dir())
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}
