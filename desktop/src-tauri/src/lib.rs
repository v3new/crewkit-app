use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crewkit_core::kits::{self, KitRegistry, KitSource};
use crewkit_core::translate::FrontmatterMap;
use crewkit_core::{Adapter, Engine, InstallReport, InstallScope, Kit, Paths, ScanReport};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_updater::UpdaterExt;

/// Client adapters and the built-in kit are declarative data files in the
/// repo, embedded into the binary. Additional kits are added by signed
/// manifest URL and live in CrewKit's kit registry.
const ADAPTER_SOURCES: &[&str] = &[
    include_str!("../../../adapters/claude-code.json"),
    include_str!("../../../adapters/claude-desktop.json"),
    include_str!("../../../adapters/codex.json"),
    include_str!("../../../adapters/chatgpt-desktop.json"),
];
const FRONTMATTER_MAP: &str = include_str!("../../../adapters/frontmatter-map.json");
/// One cadence for everything that stays fresh in the background:
/// kit re-installs and the signed app self-update check.
const BACKGROUND_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);

fn crewkit_dir() -> PathBuf {
    Paths::from_env().crewkit_dir()
}

fn adapters() -> Result<Vec<Adapter>, String> {
    ADAPTER_SOURCES
        .iter()
        .map(|json| Adapter::load(json))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// Registry access. The app starts with NO kits: users add manifests
/// themselves (URL field or a crewkit:// deep link). Legacy embedded-kit
/// rows from pre-URL builds are dropped.
fn registry() -> Result<KitRegistry, String> {
    let dir = crewkit_dir();
    let mut registry = KitRegistry::load(&dir).map_err(|e| e.to_string())?;
    let before = registry.kits.len();
    registry.kits.retain(|k| k.source != "builtin");
    if registry.kits.len() != before {
        registry.save(&dir).map_err(|e| e.to_string())?;
    }
    Ok(registry)
}

fn cache_path(kit_id: &str) -> PathBuf {
    crewkit_dir()
        .join("kits-cache")
        .join(format!("{kit_id}.json"))
}

/// Load a kit from the local cache written at add/refresh time
/// (offline scans must not hit the network).
fn load_kit(_app: &AppHandle, source: &KitSource) -> Result<(Kit, PathBuf), String> {
    let cached = std::fs::read_to_string(cache_path(&source.id))
        .map_err(|_| format!("kit `{}` has no local cache — refresh it first", source.id))?;
    let kit = Kit::load(&cached).map_err(|e| e.to_string())?;
    Ok((kit, crewkit_dir().join("artifacts").join(&source.id)))
}

/// Pin the publisher key after the first successful fetch of a source
/// that was added without one (e.g. the seeded default kit).
fn pin_key(kit_id: &str, key: &Option<String>) -> Result<(), String> {
    let Some(key) = key else { return Ok(()) };
    let dir = crewkit_dir();
    let mut reg = registry()?;
    if let Some(source) = reg.kits.iter_mut().find(|k| k.id == kit_id) {
        if source.pinned_key.is_none() {
            source.pinned_key = Some(key.clone());
            reg.save(&dir).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Re-fetch a remote kit's manifest and artifacts, verify the signature
/// against the pinned publisher key, and refresh the local cache.
fn refresh_remote(source: &KitSource) -> Result<Kit, String> {
    let fetched = kits::fetch_kit(&source.source, source.pinned_key.as_deref(), &crewkit_dir())
        .map_err(|e| e.to_string())?;
    if fetched.kit.id != source.id {
        return Err(format!(
            "manifest id changed: expected `{}`, got `{}`",
            source.id, fetched.kit.id
        ));
    }
    let json = serde_json::to_string_pretty(&fetched.kit).map_err(|e| e.to_string())?;
    crewkit_core::fsops::atomic_write(&cache_path(&source.id), json.as_bytes())
        .map_err(|e| e.to_string())?;
    pin_key(&source.id, &fetched.kit.publisher_key)?;
    Ok(fetched.kit)
}

fn engine_for(app: &AppHandle, source: &KitSource) -> Result<Engine, String> {
    let resources = app.path().resource_dir().map_err(|e| e.to_string())?;
    let (mut kit, zips_dir) = load_kit(app, source)?;
    if let Some(bundle) = &source.bundle {
        kit.apply_bundle(bundle).map_err(|e| e.to_string())?;
    }
    Ok(Engine {
        paths: Paths::from_env(),
        adapters: adapters()?,
        kit,
        zips_dir,
        bridge_source: resources
            .join("bin")
            .join(crewkit_core::bridge::BRIDGE_BIN_NAME),
        frontmatter_map: FrontmatterMap::load(FRONTMATTER_MAP).map_err(|e| e.to_string())?,
    })
}

fn source_for(kit_id: &str) -> Result<KitSource, String> {
    registry()?
        .kits
        .into_iter()
        .find(|k| k.id == kit_id)
        .ok_or_else(|| format!("unknown kit: {kit_id}"))
}

// --- Commands ---

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct KitCard {
    kit: Kit,
    source: String,
    channel: String,
    bundle: Option<String>,
    /// Why this kit could not be loaded (unreachable manifest on first
    /// fetch, broken cache…). A degraded card is still rendered — the UI
    /// must never dead-end on a single kit's failure.
    error: Option<String>,
}

/// A minimal stand-in so a failing kit still renders as a card.
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
fn list_kits(app: AppHandle) -> Result<Vec<KitCard>, String> {
    let mut cards = Vec::new();
    for source in registry()?.kits {
        // First run: no cache yet — fetch, verify and pin. A failure
        // degrades this card only; other kits and the UI keep working.
        let fetch_error = if cache_path(&source.id).exists() {
            None
        } else {
            refresh_remote(&source).err()
        };
        let (kit, error) = match load_kit(&app, &source) {
            Ok((kit, _)) => (kit, None),
            Err(load_error) => (
                placeholder_kit(&source.id),
                Some(fetch_error.unwrap_or(load_error)),
            ),
        };
        cards.push(KitCard {
            kit,
            source: source.source.clone(),
            channel: source.channel.clone(),
            bundle: source.bundle.clone(),
            error,
        });
    }
    Ok(cards)
}

#[tauri::command]
async fn scan_kit(app: AppHandle, kit_id: String) -> Result<ScanReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        engine_for(&app, &source_for(&kit_id)?)?
            .scan()
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn install_kit(app: AppHandle, kit_id: String) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || install_kit_blocking(&app, &kit_id, true))
        .await
        .map_err(|e| e.to_string())?
}

fn install_kit_blocking(
    app: &AppHandle,
    kit_id: &str,
    emit_steps: bool,
) -> Result<InstallReport, String> {
    let source = source_for(kit_id)?;
    // Refresh from the manifest URL; an unreachable server falls back to
    // the verified local cache so installs keep working offline.
    let refresh_error = refresh_remote(&source).err();
    if let Some(error) = &refresh_error {
        if !cache_path(kit_id).exists() {
            return Err(error.clone());
        }
        if emit_steps {
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
    }
    let engine = engine_for(app, &source)?;
    let report = engine
        .install(|step| {
            if emit_steps {
                let _ = app.emit("install-step", step);
            }
        })
        .map_err(|e| e.to_string())?;
    send_telemetry(&engine, &report);
    Ok(report)
}

/// Disclosed install telemetry (the UI shows the notice on the kit card).
fn send_telemetry(engine: &Engine, report: &InstallReport) {
    let items: Vec<_> = report
        .scan
        .items
        .iter()
        .map(|i| {
            serde_json::json!({
                "kind": i.kind, "id": i.id, "client": i.client,
                "status": i.status, "version": i.version,
            })
        })
        .collect();
    kits::send_install_report(
        &engine.kit,
        &crewkit_dir(),
        serde_json::json!({
            "event": "install",
            "appVersion": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "items": items,
        }),
    );
}

/// One kit item addressed from the UI ("plugin" or "mcp" + its id).
#[derive(Deserialize, Clone)]
struct ItemKey {
    kind: String,
    id: String,
}

fn scope_of(clients: Option<Vec<String>>, items: Option<Vec<ItemKey>>) -> InstallScope {
    InstallScope {
        clients: clients.map(|c| c.into_iter().collect()),
        items: items.map(|i| i.into_iter().map(|k| (k.kind, k.id)).collect()),
    }
}

/// Scoped install: specific items and/or specific clients only. Uses the
/// verified local cache (no refresh) so cell-level actions stay instant.
#[tauri::command]
async fn install_items(
    app: AppHandle,
    kit_id: String,
    clients: Option<Vec<String>>,
    items: Option<Vec<ItemKey>>,
) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let engine = engine_for(&app, &source_for(&kit_id)?)?;
        let report = engine
            .install_scoped(&scope_of(clients, items), |step| {
                let _ = app.emit("install-step", step);
            })
            .map_err(|e| e.to_string())?;
        send_telemetry(&engine, &report);
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Scoped removal: the given items, from the given clients (None = all).
#[tauri::command]
async fn remove_items(
    app: AppHandle,
    kit_id: String,
    clients: Option<Vec<String>>,
    items: Vec<ItemKey>,
) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let engine = engine_for(&app, &source_for(&kit_id)?)?;
        let targets: Option<HashSet<String>> = clients.map(|c| c.into_iter().collect());
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
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn add_kit(app: AppHandle, url: String) -> Result<KitCard, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = crewkit_dir();
        let fetched = kits::fetch_kit(&url, None, &dir).map_err(|e| e.to_string())?;
        let mut reg = registry()?;
        if reg.kits.iter().any(|k| k.id == fetched.kit.id) {
            return Err(format!("kit `{}` is already added", fetched.kit.id));
        }
        // One marketplace name = one kit: a second kit reusing the name
        // would overwrite the first kit's staged marketplace directory.
        for existing in &reg.kits {
            if let Ok(text) = std::fs::read_to_string(cache_path(&existing.id)) {
                if let Ok(kit) = Kit::load(&text) {
                    if kit.marketplace_name == fetched.kit.marketplace_name {
                        return Err(format!(
                            "marketplace name `{}` is already used by kit `{}`",
                            kit.marketplace_name, existing.id
                        ));
                    }
                }
            }
        }
        let source = KitSource {
            id: fetched.kit.id.clone(),
            source: url.clone(),
            channel: "stable".into(),
            pinned_key: fetched.kit.publisher_key.clone(),
            bundle: None,
        };
        let json = serde_json::to_string_pretty(&fetched.kit).map_err(|e| e.to_string())?;
        crewkit_core::fsops::atomic_write(&cache_path(&fetched.kit.id), json.as_bytes())
            .map_err(|e| e.to_string())?;
        reg.kits.push(source.clone());
        reg.save(&dir).map_err(|e| e.to_string())?;
        let _ = app.emit("kits-changed", ());
        Ok(KitCard {
            kit: fetched.kit,
            source: source.source,
            channel: source.channel,
            bundle: None,
            error: None,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn remove_kit(kit_id: String) -> Result<(), String> {
    let dir = crewkit_dir();
    let mut reg = registry()?;
    let before = reg.kits.len();
    reg.kits.retain(|k| k.id != kit_id);
    if reg.kits.len() == before {
        return Err(format!("unknown kit: {kit_id}"));
    }
    reg.save(&dir).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(cache_path(&kit_id));
    let _ = std::fs::remove_dir_all(dir.join("artifacts").join(&kit_id));
    Ok(())
}

#[tauri::command]
async fn set_channel(app: AppHandle, kit_id: String, channel: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = crewkit_dir();
        let mut reg = registry()?;
        let source = reg
            .kits
            .iter_mut()
            .find(|k| k.id == kit_id)
            .ok_or_else(|| format!("unknown kit: {kit_id}"))?;
        let (kit, _) = load_kit(&app, source)?;
        let target = kit
            .channels
            .get(&channel)
            .ok_or_else(|| format!("kit has no `{channel}` channel"))?;
        let url = kits::resolve_url(&source.source, target);
        let fetched =
            kits::fetch_kit(&url, source.pinned_key.as_deref(), &dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_string_pretty(&fetched.kit).map_err(|e| e.to_string())?;
        crewkit_core::fsops::atomic_write(&cache_path(&kit_id), json.as_bytes())
            .map_err(|e| e.to_string())?;
        source.source = url;
        source.channel = channel;
        reg.save(&dir).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn set_bundle(kit_id: String, bundle: Option<String>) -> Result<(), String> {
    let dir = crewkit_dir();
    let mut reg = registry()?;
    let source = reg
        .kits
        .iter_mut()
        .find(|k| k.id == kit_id)
        .ok_or_else(|| format!("unknown kit: {kit_id}"))?;
    source.bundle = bundle;
    reg.save(&dir).map_err(|e| e.to_string())
}

#[tauri::command]
async fn remove_item(
    app: AppHandle,
    kit_id: String,
    kind: String,
    id: String,
) -> Result<InstallReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        engine_for(&app, &source_for(&kit_id)?)?
            .remove_item(&kind, &id, |step| {
                let _ = app.emit("install-step", step);
            })
            .map_err(|e| e.to_string())
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

#[tauri::command]
async fn authorize(server_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Longer than the bridge's own 300s login deadline: the bridge
        // must time out first and report properly, not die on kill().
        run_bridge(&["login", &server_id], Duration::from_secs(330))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn deauthorize(server_id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_bridge(&["logout", &server_id], Duration::from_secs(60))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Self-update check via the updater plugin (signed endpoints from
/// tauri.conf.json); returns the newer version's number if any.
/// Being offline or having no update published is not an error.
async fn newer_app_version(app: &AppHandle) -> Option<String> {
    let updater = app.updater().ok()?;
    match updater.check().await {
        Ok(Some(update)) => Some(update.version.clone()),
        _ => None,
    }
}

#[tauri::command]
async fn check_app_update(app: AppHandle) -> Result<Option<String>, String> {
    Ok(newer_app_version(&app).await)
}

/// Download the signed update, verify it against the pinned public key,
/// install it in place and restart into the new version.
#[tauri::command]
async fn install_app_update(app: AppHandle) -> Result<(), String> {
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

// --- Background updates (tray) ---

/// Quietly re-install every kit (idempotent: updates what changed, skips
/// the rest), then let an open window refresh itself.
fn background_update(app: &AppHandle) {
    let Ok(reg) = registry() else { return };
    for source in reg.kits {
        let _ = install_kit_blocking(app, &source.id, false);
    }
    let _ = app.emit("kits-updated", ());
}

fn show_main_window(app: &AppHandle) {
    // Back to a regular app: Dock icon returns while the window is open.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Hide into the tray: window hidden, Dock icon gone, process alive.
fn hide_to_tray(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            list_kits,
            scan_kit,
            install_kit,
            add_kit,
            remove_kit,
            set_channel,
            set_bundle,
            remove_item,
            install_items,
            remove_items,
            authorize,
            deauthorize,
            check_app_update,
            install_app_update
        ])
        .setup(|app| {
            // Tray: CrewKit keeps kits fresh in the background.
            let open = MenuItem::with_id(app, "open", "Open CrewKit", true, None::<&str>)?;
            let update = MenuItem::with_id(app, "update", "Update kits now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit CrewKit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &update, &quit])?;
            // Monochrome template glyph: macOS tints it for light/dark
            // menu bars and for the pressed state.
            let tray_icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main_window(app),
                    "update" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn_blocking(move || background_update(&app));
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // crewkit://add?kit=<manifest-url> → the UI confirms and adds.
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    if url.scheme() == "crewkit" {
                        let kit_url = url
                            .query_pairs()
                            .find(|(k, _)| k == "kit")
                            .map(|(_, v)| v.into_owned());
                        if let Some(kit_url) = kit_url {
                            show_main_window(&handle);
                            let _ = handle.emit("deep-link-add-kit", kit_url);
                        }
                    }
                }
            });

            // Background refresh: keep every kit current while the app
            // runs, and surface a signed app update when one is published.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio_sleep(BACKGROUND_INTERVAL).await;
                    let kits_handle = handle.clone();
                    let _ = tauri::async_runtime::spawn_blocking(move || {
                        background_update(&kits_handle)
                    })
                    .await;
                    if let Some(version) = newer_app_version(&handle).await {
                        let _ = handle.emit("app-update-available", version);
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps CrewKit alive in the tray.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                hide_to_tray(window.app_handle());
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            match event {
                // Cmd+Q (no exit code) hides into the tray instead of quitting;
                // the tray menu's Quit calls app.exit(0), which carries a code
                // and is allowed through.
                tauri::RunEvent::ExitRequested { api, code, .. } => {
                    if code.is_none() {
                        api.prevent_exit();
                        hide_to_tray(app);
                    }
                }
                // Clicking the Dock icon brings the window back.
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => show_main_window(app),
                _ => {}
            }
        });
}

async fn tokio_sleep(duration: Duration) {
    tauri::async_runtime::spawn_blocking(move || std::thread::sleep(duration))
        .await
        .ok();
}
