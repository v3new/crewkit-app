use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fsops;
use crate::lock::FileLock;
use crate::updater::diff::ItemRef;

pub const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
const DUE_AFTER: Duration = Duration::from_secs(55 * 60);
const STATE_LOCK_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateState {
    pub last_check_unix: u64,
    pub app: Option<AppInfo>,
    pub app_update_available: Option<String>,
    pub new_items: BTreeMap<String, Vec<ItemRef>>,
    pub notifications: Vec<Notification>,
}

/// Where the desktop app lives, written by the app itself on every
/// start so the bridge can update it while it is not running.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub app_path: PathBuf,
    pub bridge_source: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub at_unix: u64,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Event {
    KitUpdated { kit: String, items: Vec<String> },
    NewItems { kit: String, items: Vec<ItemRef> },
    RestartNeeded { clients: Vec<String> },
    AppUpdated { version: String },
}

impl UpdateState {
    pub fn path(crewkit_dir: &Path) -> PathBuf {
        crewkit_dir.join("update-state.json")
    }

    pub fn load(crewkit_dir: &Path) -> Result<Self> {
        Ok(fsops::read_json(&Self::path(crewkit_dir))?
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default())
    }

    fn save(&self, crewkit_dir: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).expect("update state serializes");
        fsops::atomic_write(&Self::path(crewkit_dir), json.as_bytes())
    }

    /// Every write goes through a short-lived lock: the app and the
    /// bridge's updater both edit this file, and neither may lose the
    /// other's notifications.
    pub fn modify<T>(crewkit_dir: &Path, change: impl FnOnce(&mut Self) -> T) -> Result<T> {
        std::fs::create_dir_all(crewkit_dir).map_err(crate::error::io_ctx(format!(
            "creating {}",
            crewkit_dir.display()
        )))?;
        let lock_path = crewkit_dir.join("update-state.lock");
        let deadline = std::time::Instant::now() + STATE_LOCK_WAIT;
        let _lock = loop {
            if let Some(lock) = FileLock::acquire(lock_path.clone(), STATE_LOCK_WAIT) {
                break lock;
            }
            if std::time::Instant::now() > deadline {
                return Err(Error::Invalid(
                    "update state is locked by another process".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut state = Self::load(crewkit_dir)?;
        let before = serde_json::to_string(&state).expect("update state serializes");
        let result = change(&mut state);
        if serde_json::to_string(&state).expect("update state serializes") != before {
            state.save(crewkit_dir)?;
        }
        Ok(result)
    }

    pub fn is_due(&self, now_unix: u64) -> bool {
        self.last_check_unix == 0
            || now_unix < self.last_check_unix
            || now_unix - self.last_check_unix >= DUE_AFTER.as_secs()
    }

    pub fn notify(&mut self, event: Event) {
        self.notifications.push(Notification {
            at_unix: now_unix(),
            event,
        });
    }

    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.notifications)
    }

    /// Returns the items not announced before.
    pub fn add_new_items(&mut self, kit: &str, items: &[ItemRef]) -> Vec<ItemRef> {
        let known = self.new_items.entry(kit.to_string()).or_default();
        let mut fresh = Vec::new();
        for item in items {
            if !known.contains(item) {
                known.push(item.clone());
                fresh.push(item.clone());
            }
        }
        fresh
    }

    pub fn forget_installed(&mut self, kit: &str, installed: &[ItemRef]) {
        if let Some(items) = self.new_items.get_mut(kit) {
            items.retain(|item| !installed.contains(item));
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_after_fifty_five_minutes() {
        let state = UpdateState {
            last_check_unix: 10_000,
            ..Default::default()
        };
        assert!(!state.is_due(10_000 + 54 * 60));
        assert!(state.is_due(10_000 + 55 * 60));
        assert!(state.is_due(9_000));
        assert!(UpdateState::default().is_due(0));
    }

    #[test]
    fn new_items_dedupe_and_forget() {
        let mut state = UpdateState::default();
        let item = ItemRef::plugin("a@mkt");
        assert_eq!(
            state
                .add_new_items("kit", &[item.clone(), item.clone()])
                .len(),
            1
        );
        assert!(state
            .add_new_items("kit", std::slice::from_ref(&item))
            .is_empty());
        assert_eq!(state.new_items["kit"].len(), 1);
        state.forget_installed("kit", &[item]);
        assert!(state.new_items["kit"].is_empty());
    }

    #[test]
    fn notifications_roundtrip_through_json() {
        let mut state = UpdateState::default();
        state.notify(Event::RestartNeeded {
            clients: vec!["Claude Desktop".into()],
        });
        let json = serde_json::to_string(&state).unwrap();
        let back: UpdateState = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back.notifications[0].event,
            Event::RestartNeeded { .. }
        ));
    }
}
