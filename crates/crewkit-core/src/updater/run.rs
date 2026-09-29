use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Result;
use crate::installer::{Engine, InstallScope, StepReport, StepStatus};
use crate::inventory::Status;
use crate::kits::{self, Auth, KitRegistry, KitSource};
use crate::lock::FileLock;
use crate::paths::Paths;
use crate::updater::diff::{diff, ItemRef, KitDiff};
use crate::updater::state::{now_unix, Event, UpdateState};

const LOCK_STALE_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Startup,
    Hourly,
    Bridge,
    Manual,
}

impl Trigger {
    fn throttled(self) -> bool {
        matches!(self, Trigger::Hourly | Trigger::Bridge)
    }
}

#[derive(Debug, Default)]
pub struct KitOutcome {
    pub kit: String,
    pub diff: KitDiff,
    pub steps: Vec<StepReport>,
    pub restart_needed: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct UpdateReport {
    pub kits: Vec<KitOutcome>,
}

fn lock_path(crewkit_dir: &Path) -> PathBuf {
    crewkit_dir.join("update.lock")
}

/// Every write into the clients happens under this lock — UI actions,
/// the app's timer and the bridge's updater alike.
pub fn lock(crewkit_dir: &Path) -> Option<FileLock> {
    std::fs::create_dir_all(crewkit_dir).ok()?;
    FileLock::acquire(lock_path(crewkit_dir), LOCK_STALE_AFTER)
}

pub fn lock_held(crewkit_dir: &Path) -> bool {
    FileLock::holder_alive(&lock_path(crewkit_dir))
}

/// Another process is writing into the clients right now.
pub fn in_progress(crewkit_dir: &Path) -> bool {
    let path = lock_path(crewkit_dir);
    FileLock::holder_alive(&path) && FileLock::holder(&path) != Some(std::process::id())
}

/// Check every kit and apply what changed. `None` means nothing ran:
/// a throttled trigger came too early, or another process holds the lock.
pub fn run(
    paths: &Paths,
    trigger: Trigger,
    bridge_source: Option<&Path>,
    mut on_step: impl FnMut(&StepReport),
) -> Result<Option<UpdateReport>> {
    let crewkit_dir = paths.crewkit_dir();
    if trigger.throttled() && !UpdateState::load(&crewkit_dir)?.is_due(now_unix()) {
        return Ok(None);
    }
    let Some(_lock) = lock(&crewkit_dir) else {
        return Ok(None);
    };
    let mut report = UpdateReport::default();
    for source in KitRegistry::load(&crewkit_dir)?.kits {
        let outcome = update_kit(paths, &source, trigger, bridge_source, &mut on_step);
        report.kits.push(outcome);
    }
    UpdateState::modify(&crewkit_dir, |state| state.last_check_unix = now_unix())?;
    Ok(Some(report))
}

fn update_kit(
    paths: &Paths,
    source: &KitSource,
    trigger: Trigger,
    bridge_source: Option<&Path>,
    on_step: &mut impl FnMut(&StepReport),
) -> KitOutcome {
    let mut outcome = KitOutcome {
        kit: source.id.clone(),
        ..Default::default()
    };
    match apply(paths, source, trigger, bridge_source, on_step, &mut outcome) {
        Ok(()) => {}
        Err(e) => outcome.error = Some(e.to_string()),
    }
    outcome
}

fn apply(
    paths: &Paths,
    source: &KitSource,
    trigger: Trigger,
    bridge_source: Option<&Path>,
    on_step: &mut impl FnMut(&StepReport),
    outcome: &mut KitOutcome,
) -> Result<()> {
    let crewkit_dir = paths.crewkit_dir();
    let before = kits::load_cached(&crewkit_dir, source).ok();
    let manifest = kits::fetch_verified(source, &crewkit_dir, Auth::Silent)?;
    let mut fresh = manifest.clone();
    if let Some(bundle) = &source.bundle {
        fresh.apply_bundle(bundle)?;
    }
    if let Some(before) = &before {
        outcome.diff = diff(before, &fresh);
    }
    if !outcome.diff.changes_installed() && trigger != Trigger::Manual {
        kits::write_cache(&crewkit_dir, &manifest)?;
        return announce(&crewkit_dir, source, outcome, &[]);
    }

    let engine = Engine::new(
        paths.clone(),
        fresh,
        kits::artifacts_dir(&crewkit_dir, &source.id),
        bridge_source.map(Path::to_path_buf),
    )?;
    let scan = engine.scan()?;
    let mut clients: Vec<String> = scan
        .items
        .iter()
        .filter(|i| i.status == Status::Installed)
        .map(|i| i.client.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    clients.sort();
    for client in clients {
        let items: HashSet<(String, String)> = scan
            .items
            .iter()
            .filter(|i| i.client == client && i.status == Status::Installed)
            .map(|i| (i.kind.clone(), i.id.clone()))
            .collect();
        let scope = InstallScope {
            clients: Some(HashSet::from([client])),
            items: Some(items),
        };
        let report = engine.install_scoped(&scope, &mut *on_step)?;
        outcome.steps.extend(report.steps);
        merge_restart(&mut outcome.restart_needed, report.restart_needed);
    }
    for item in &outcome.diff.removed {
        let report = engine.remove_item(&item.kind, &item.id, &mut *on_step)?;
        outcome.steps.extend(report.steps);
        merge_restart(&mut outcome.restart_needed, report.restart_needed);
    }
    let after = engine.scan()?;
    let installed: Vec<ItemRef> = after
        .items
        .iter()
        .filter(|i| i.status == Status::Installed)
        .map(|i| ItemRef {
            kind: i.kind.clone(),
            id: i.id.clone(),
        })
        .collect();
    kits::report_install(&engine.kit, &crewkit_dir, &after);
    let clean = !outcome.steps.iter().any(|s| s.status == StepStatus::Failed);
    if clean {
        kits::write_cache(&crewkit_dir, &manifest)?;
    }
    announce(&crewkit_dir, source, outcome, &installed)
}

fn announce(
    crewkit_dir: &Path,
    source: &KitSource,
    outcome: &KitOutcome,
    installed: &[ItemRef],
) -> Result<()> {
    let applied = outcome.steps.iter().any(|s| s.status == StepStatus::Ok);
    UpdateState::modify(crewkit_dir, |state| {
        let fresh = state.add_new_items(&source.id, &outcome.diff.added);
        state.forget_installed(&source.id, installed);
        if !outcome.diff.updated.is_empty() && applied {
            state.notify(Event::KitUpdated {
                kit: source.id.clone(),
                items: outcome
                    .diff
                    .updated
                    .iter()
                    .map(|c| format!("{} {} → {}", c.name, c.from, c.to))
                    .collect(),
            });
        }
        if !fresh.is_empty() {
            state.notify(Event::NewItems {
                kit: source.id.clone(),
                items: fresh,
            });
        }
        if !outcome.restart_needed.is_empty() {
            state.notify(Event::RestartNeeded {
                clients: outcome.restart_needed.clone(),
            });
        }
    })
}

fn merge_restart(into: &mut Vec<String>, more: Vec<String>) {
    for name in more {
        if !into.contains(&name) {
            into.push(name);
        }
    }
}
