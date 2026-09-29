mod events;
mod items;
mod kits;
mod updates;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;

#[derive(Serialize, Clone)]
struct DeepLinkAdd {
    url: String,
    channel: Option<String>,
    bundle: Option<String>,
}

/// `crewkit://add?kit=<manifest-url>[&channel=…][&bundle=…]`
fn parse_add_link(url: &url::Url) -> Option<DeepLinkAdd> {
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    Some(DeepLinkAdd {
        url: param("kit")?,
        channel: param("channel"),
        bundle: param("bundle"),
    })
}

fn show_main_window(app: &AppHandle) {
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
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            kits::list_kits,
            kits::inspect_kit,
            kits::add_kit,
            kits::remove_kit,
            kits::authorize_kit,
            kits::deauthorize_kit,
            items::scan_kit,
            items::install_kit,
            items::install_items,
            items::remove_items,
            items::remove_item,
            items::authorize,
            items::deauthorize,
            items::update_in_progress,
            updates::install_app_update,
            updates::update_now,
            events::load_event_log,
            events::save_event_log
        ])
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "Open CrewKit", true, None::<&str>)?;
            let update = MenuItem::with_id(app, "update", "Update now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit CrewKit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &update, &quit])?;
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
                        tauri::async_runtime::spawn(async move { updates::update_now(app).await });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    if url.scheme() != "crewkit" {
                        continue;
                    }
                    if let Some(add) = parse_add_link(&url) {
                        show_main_window(&handle);
                        let _ = handle.emit("deep-link-add-kit", add);
                    }
                }
            });

            updates::start(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                hide_to_tray(window.app_handle());
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::ExitRequested { api, code, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                    hide_to_tray(app);
                }
            }
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show_main_window(app),
            _ => {}
        });
}
