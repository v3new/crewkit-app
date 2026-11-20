//! The app itself, through the signed updater: a check from the tray,
//! and the silent install kept behind `AUTO_APP_UPDATE`.

use std::sync::atomic::{AtomicBool, Ordering};

use crewkit_core::StepReport;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::kits::crewkit_dir;
use crate::updates::{wait_for_lock, BackgroundReport};

/// A newer release that was found but did not install; the UI's banner
/// names it and offers a retry.
#[derive(Serialize, Clone)]
pub struct FailedUpdate {
    version: String,
    error: String,
}

enum UpdateError {
    /// The release feed could not be read, so nothing is known about a
    /// newer version.
    Check(String),
    Install(FailedUpdate),
}

impl UpdateError {
    /// The banner already names the version, so only the reason.
    fn message(self) -> String {
        match self {
            UpdateError::Check(error) => error,
            UpdateError::Install(failed) => failed.error,
        }
    }
}

static SELF_UPDATING: AtomicBool = AtomicBool::new(false);

/// Silent self-update, used only when `AUTO_APP_UPDATE` is on.
pub async fn self_update(app: &AppHandle) {
    if SELF_UPDATING.swap(true, Ordering::SeqCst) {
        return;
    }
    match apply(app).await {
        Ok(()) => {
            let _ = app.emit("app-update-current", ());
        }
        Err(UpdateError::Check(error)) => {
            let _ = app.emit(
                "background-report",
                BackgroundReport {
                    steps: vec![StepReport {
                        step: "Check for CrewKit update".into(),
                        client: "crewkit".into(),
                        status: crewkit_core::StepStatus::Skipped,
                        message: error,
                    }],
                    restart_needed: Vec::new(),
                },
            );
        }
        Err(UpdateError::Install(failed)) => {
            let _ = app.emit("app-update-available", failed);
        }
    }
    SELF_UPDATING.store(false, Ordering::SeqCst);
}

/// The tray's "Check for Updates…": every outcome is answered in a dialog,
/// and nothing installs until the user agrees.
pub async fn check_for_updates(app: AppHandle) {
    if SELF_UPDATING.swap(true, Ordering::SeqCst) {
        return;
    }
    let current = env!("CARGO_PKG_VERSION");
    match find(&app).await {
        Ok(None) => {
            let _ = app.emit("app-update-current", ());
            let text = format!("CrewKit {current} is the latest version.");
            show(
                &app,
                MessageDialogKind::Info,
                text,
                MessageDialogButtons::Ok,
            )
            .await;
        }
        Ok(Some(update)) => {
            let text = format!(
                "CrewKit {} is available (you have {current}). Update and restart now?",
                update.version
            );
            let buttons =
                MessageDialogButtons::OkCancelCustom("Update and Restart".into(), "Later".into());
            if show(&app, MessageDialogKind::Info, text, buttons).await {
                if let Err(failed) = install(&app, update).await {
                    let _ = app.emit("app-update-available", failed.clone());
                    let text = format!("Couldn't install the update: {}", failed.error);
                    show(
                        &app,
                        MessageDialogKind::Error,
                        text,
                        MessageDialogButtons::Ok,
                    )
                    .await;
                }
            }
        }
        Err(error) => {
            let text = format!("Couldn't check for updates: {}", error.message());
            show(
                &app,
                MessageDialogKind::Error,
                text,
                MessageDialogButtons::Ok,
            )
            .await;
        }
    }
    SELF_UPDATING.store(false, Ordering::SeqCst);
}

/// Retry of a failed update from the banner's button. `Ok` means there
/// is nothing to install any more; on success the app restarts.
#[tauri::command]
pub async fn install_app_update(app: AppHandle) -> Result<(), String> {
    if SELF_UPDATING.swap(true, Ordering::SeqCst) {
        return Err("update already running".into());
    }
    let result = apply(&app).await;
    SELF_UPDATING.store(false, Ordering::SeqCst);
    if let Err(UpdateError::Install(failed)) = &result {
        // The retry may have found a newer release than the banner names.
        let _ = app.emit("app-update-available", failed.clone());
    }
    result.map_err(UpdateError::message)
}

async fn apply(app: &AppHandle) -> Result<(), UpdateError> {
    match find(app).await? {
        Some(update) => install(app, update).await.map_err(UpdateError::Install),
        None => Ok(()),
    }
}

async fn find(app: &AppHandle) -> Result<Option<Update>, UpdateError> {
    let updater = app
        .updater()
        .map_err(|e| UpdateError::Check(e.to_string()))?;
    updater
        .check()
        .await
        .map_err(|e| UpdateError::Check(e.to_string()))
}

/// Download, verify against the pinned key, install, and restart — all
/// while holding the update lock, so no install is writing into the
/// clients when the process goes away.
async fn install(app: &AppHandle, update: Update) -> Result<(), FailedUpdate> {
    let failed = |error: String| FailedUpdate {
        version: update.version.clone(),
        error,
    };
    let dir = crewkit_dir();
    let lock = tauri::async_runtime::spawn_blocking(move || wait_for_lock(&dir, 120))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| failed("an install is still running".into()))?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| failed(e.to_string()))?;
    drop(lock);
    app.restart();
}

async fn show(
    app: &AppHandle,
    kind: MessageDialogKind,
    text: String,
    buttons: MessageDialogButtons,
) -> bool {
    // A tray-only app is never frontmost, so a parentless alert opens
    // behind other apps; showing the window brings CrewKit forward.
    crate::show_main_window(app);
    let mut dialog = app
        .dialog()
        .message(text)
        .title("CrewKit")
        .kind(kind)
        .buttons(buttons);
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.parent(&window);
    }
    tauri::async_runtime::spawn_blocking(move || dialog.blocking_show())
        .await
        .unwrap_or(false)
}
