use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::kits::crewkit_dir;

/// The Details journal outlives sessions as a plain file next to the kit
/// registry. Entry shape is owned by the UI; the backend just stores it.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EventLog {
    #[serde(default)]
    app_version: Option<String>,
    #[serde(default)]
    entries: Vec<serde_json::Value>,
    #[serde(default)]
    detected: Option<String>,
}

fn events_path() -> PathBuf {
    crewkit_dir().join("events.json")
}

#[tauri::command]
pub fn load_event_log() -> EventLog {
    std::fs::read_to_string(events_path())
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

#[tauri::command]
pub fn save_event_log(log: EventLog) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&log).map_err(|e| e.to_string())?;
    crewkit_core::fsops::atomic_write(&events_path(), json.as_bytes()).map_err(|e| e.to_string())
}
