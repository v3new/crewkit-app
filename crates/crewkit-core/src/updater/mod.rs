//! Background updates shared by the desktop app and crewkit-bridge: one
//! cross-process lock, one state file, one policy — installed items are
//! updated in place, new items in the chosen bundle are announced.

mod app;
mod diff;
mod run;
mod spawn;
mod state;

pub use app::{app_running, check_and_install as update_app, hold_app_lock, is_newer, Release};
pub use diff::{diff, Change, ItemRef, KitDiff};
pub use run::{in_progress, lock, lock_held, run, KitOutcome, Trigger, UpdateReport};
pub use spawn::spawn_if_due;
pub use state::{AppInfo, Event, Notification, UpdateState, CHECK_INTERVAL};
