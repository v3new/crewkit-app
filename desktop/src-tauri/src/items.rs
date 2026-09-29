use std::collections::HashSet;
use std::time::Duration;

use crewkit_core::kits::{self, Auth};
use crewkit_core::{updater, InstallReport, InstallScope, ScanReport};
use serde::Deserialize;
use tauri::{AppHandle, Emitter};

use crate::kits::{crewkit_dir, engine_for, source_for};

/// Every write into the clients shares one lock with the background
/// updaters; a run that finds it taken tells the user instead of racing.
pub fn with_update_lock<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _lock = updater::lock(&crewkit_dir())
        .ok_or("a background update is in progress — try again in a minute")?;
    work()
}

#[tauri::command]
pub fn update_in_progress() -> bool {
    updater::lock_held(&crewkit_dir())
}

#[tauri::command]
pub async fn scan_kit(app: AppHandle, kit_id: String) -> Result<ScanReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        engine_for(&app, &source_for(&kit_id)?)?
            .scan()
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Full install: refresh the manifest (a person is waiting, so a kit
/// behind a login may open the browser), then install everything in
/// the chosen bundle. An unreachable server falls back to the cache.
#[tauri::command]
pub async fn install_kit(app: AppHandle, kit_id: String) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = crewkit_dir();
        let source = source_for(&kit_id)?;
        if let Err(error) = kits::refresh(&source, &dir, Auth::Interactive) {
            if !kits::cache_path(&dir, &kit_id).exists() {
                return Err(error.to_string());
            }
            let _ = app.emit(
                "install-step",
                crewkit_core::StepReport {
                    step: "Refresh kit".into(),
                    client: "crewkit".into(),
                    status: crewkit_core::StepStatus::Skipped,
                    message: format!("using cached kit — {error}"),
                },
            );
        }
        let engine = engine_for(&app, &source)?;
        let report = with_update_lock(|| {
            engine
                .install(|step| {
                    let _ = app.emit("install-step", step);
                })
                .map_err(|e| e.to_string())
        })?;
        kits::report_install(&engine.kit, &dir, &report.scan);
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Deserialize, Clone)]
pub struct ItemKey {
    kind: String,
    id: String,
}

fn scope_of(clients: Option<Vec<String>>, items: Option<Vec<ItemKey>>) -> InstallScope {
    InstallScope {
        clients: clients.map(|c| c.into_iter().collect()),
        items: items.map(|i| i.into_iter().map(|k| (k.kind, k.id)).collect()),
    }
}

/// Scoped install from the verified local cache, so cell-level actions
/// stay instant.
#[tauri::command]
pub async fn install_items(
    app: AppHandle,
    kit_id: String,
    clients: Option<Vec<String>>,
    items: Option<Vec<ItemKey>>,
) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let engine = engine_for(&app, &source_for(&kit_id)?)?;
        let report = with_update_lock(|| {
            engine
                .install_scoped(&scope_of(clients, items), |step| {
                    let _ = app.emit("install-step", step);
                })
                .map_err(|e| e.to_string())
        })?;
        kits::report_install(&engine.kit, &crewkit_dir(), &report.scan);
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn remove_items(
    app: AppHandle,
    kit_id: String,
    clients: Option<Vec<String>>,
    items: Vec<ItemKey>,
) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let engine = engine_for(&app, &source_for(&kit_id)?)?;
        let targets: Option<HashSet<String>> = clients.map(|c| c.into_iter().collect());
        with_update_lock(|| {
            let mut steps = Vec::new();
            let mut restart_needed: Vec<String> = Vec::new();
            let mut scan = None;
            for item in items {
                let report = engine
                    .remove_item_scoped(&item.kind, &item.id, targets.as_ref(), |step| {
                        let _ = app.emit("install-step", step);
                    })
                    .map_err(|e| e.to_string())?;
                steps.extend(report.steps);
                for name in report.restart_needed {
                    if !restart_needed.contains(&name) {
                        restart_needed.push(name);
                    }
                }
                scan = Some(report.scan);
            }
            let scan = match scan {
                Some(scan) => scan,
                None => engine.scan().map_err(|e| e.to_string())?,
            };
            Ok(InstallReport {
                steps,
                restart_needed,
                scan,
            })
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn remove_item(
    app: AppHandle,
    kit_id: String,
    kind: String,
    id: String,
) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let engine = engine_for(&app, &source_for(&kit_id)?)?;
        with_update_lock(|| {
            engine
                .remove_item(&kind, &id, |step| {
                    let _ = app.emit("install-step", step);
                })
                .map_err(|e| e.to_string())
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

fn run_bridge(args: &[&str], timeout: Duration) -> Result<(), String> {
    let bridge = crewkit_core::bridge::bridge_path(&crewkit_dir());
    if !bridge.exists() {
        return Err("crewkit-bridge is not installed yet — run Install first".into());
    }
    let output = crewkit_core::cli::run(&bridge, args, &[], timeout).map_err(|e| e.to_string())?;
    if output.success() {
        Ok(())
    } else {
        Err(output.combined())
    }
}

/// Longer than the bridge's own 300s login deadline: the bridge must
/// time out first and report properly, not die on kill().
#[tauri::command]
pub async fn authorize(server_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_bridge(&["login", &server_id], Duration::from_secs(330))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn deauthorize(server_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_bridge(&["logout", &server_id], Duration::from_secs(60))
    })
    .await
    .map_err(|e| e.to_string())?
}
