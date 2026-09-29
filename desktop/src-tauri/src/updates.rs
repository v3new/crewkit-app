//! What keeps CrewKit current while the app runs: kits and plugins at
//! start and every hour, the app itself through the signed updater, and
//! a minute-by-minute look at what the bridge's updater did meanwhile.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crewkit_core::bridge;
use crewkit_core::updater::{self, AppInfo, Trigger, UpdateState, CHECK_INTERVAL};
use crewkit_core::{Paths, StepReport};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

use crate::kits::{bridge_source, crewkit_dir};

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundReport {
    steps: Vec<StepReport>,
    restart_needed: Vec<String>,
}

pub fn start(app: AppHandle) {
    let startup = app.clone();
    tauri::async_runtime::spawn(async move {
        let handle = startup.clone();
        let _ = tauri::async_runtime::spawn_blocking(move || register(&handle)).await;
        run_kits(&startup, Trigger::Startup).await;
        self_update(&startup).await;
    });

    let hourly = app.clone();
    tauri::async_runtime::spawn(async move {
        let jitter = Duration::from_secs(u64::from(std::process::id() % 300));
        loop {
            sleep(CHECK_INTERVAL + jitter).await;
            run_kits(&hourly, Trigger::Hourly).await;
            self_update(&hourly).await;
        }
    });

    tauri::async_runtime::spawn(async move { watch_state(app).await });
}

/// Record where this app lives so the bridge can update it while it is
/// closed, hold the "app is running" lock, and deploy this build's bridge.
fn register(app: &AppHandle) -> Result<(), String> {
    let dir = crewkit_dir();
    std::mem::forget(updater::hold_app_lock(&dir));
    let source = bridge_source(app)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    UpdateState::modify(&dir, |state| {
        state.app = Some(AppInfo {
            app_path: bundle_root(&exe),
            bridge_source: source.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
        });
        state.app_update_available = None;
    })
    .map_err(|e| e.to_string())?;
    let _lock = wait_for_lock(&dir, 24).ok_or("update in progress")?;
    bridge::install_bridge(&source, &dir)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn wait_for_lock(dir: &Path, attempts: u32) -> Option<crewkit_core::lock::FileLock> {
    for _ in 0..attempts {
        if let Some(lock) = updater::lock(dir) {
            return Some(lock);
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    None
}

fn bundle_root(exe: &Path) -> PathBuf {
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|ext| ext == "app"))
        .unwrap_or(exe)
        .to_path_buf()
}

pub async fn run_kits(app: &AppHandle, trigger: Trigger) {
    let handle = app.clone();
    let _ = app.emit("background-update", true);
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let source = bridge_source(&handle).ok();
        updater::run(&Paths::from_env(), trigger, source.as_deref(), |step| {
            let _ = handle.emit("install-step", step);
        })
    })
    .await;
    let _ = app.emit("background-update", false);
    if let Ok(Ok(Some(report))) = outcome {
        let mut summary = BackgroundReport::default();
        for kit in report.kits {
            summary.steps.extend(kit.steps);
            for name in kit.restart_needed {
                if !summary.restart_needed.contains(&name) {
                    summary.restart_needed.push(name);
                }
            }
            if let Some(error) = kit.error {
                summary.steps.push(StepReport {
                    step: format!("Refresh kit {}", kit.kit),
                    client: "crewkit".into(),
                    status: crewkit_core::StepStatus::Skipped,
                    message: error,
                });
            }
        }
        let _ = app.emit("background-report", summary);
    }
    let _ = app.emit("kits-updated", ());
    drain_notifications(app);
}

fn drain_notifications(app: &AppHandle) {
    let taken = UpdateState::modify(&crewkit_dir(), UpdateState::take_notifications);
    if let Ok(notifications) = taken {
        if !notifications.is_empty() {
            let _ = app.emit("notifications", notifications);
        }
    }
}

/// Silent self-update: download, verify against the pinned key, install,
/// and restart as soon as no install is writing into the clients.
async fn self_update(app: &AppHandle) {
    let Ok(updater) = app.updater() else { return };
    let Ok(Some(update)) = updater.check().await else {
        return;
    };
    let version = update.version.clone();
    if let Err(e) = update.download_and_install(|_, _| {}, || {}).await {
        let _ = app.emit("app-update-available", format!("{version}: {e}"));
        return;
    }
    let dir = crewkit_dir();
    let _ = tauri::async_runtime::spawn_blocking(move || wait_for_lock(&dir, 120)).await;
    app.restart();
}

/// Retry of a failed silent update from the banner's button.
#[tauri::command]
pub async fn install_app_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no update available")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

#[tauri::command]
pub async fn update_now(app: AppHandle) {
    run_kits(&app, Trigger::Manual).await;
    self_update(&app).await;
}

async fn watch_state(app: AppHandle) {
    let dir = crewkit_dir();
    let state_path = UpdateState::path(&dir);
    let mut seen = modified(&state_path);
    let mut busy = false;
    loop {
        sleep(Duration::from_secs(60)).await;
        let now_busy = updater::in_progress(&dir);
        if now_busy != busy {
            busy = now_busy;
            let _ = app.emit("background-update", busy);
        }
        let stamp = modified(&state_path);
        if stamp == seen {
            continue;
        }
        seen = stamp;
        drain_notifications(&app);
        let _ = app.emit("kits-updated", ());
        let pending = UpdateState::load(&dir)
            .map(|s| s.app_update_available.is_some())
            .unwrap_or(false);
        if pending {
            self_update(&app).await;
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

async fn sleep(duration: Duration) {
    tauri::async_runtime::spawn_blocking(move || std::thread::sleep(duration))
        .await
        .ok();
}
