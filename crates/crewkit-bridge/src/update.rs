//! The bridge is what every AI client launches, so it is the process
//! that keeps kits fresh while the desktop app is closed. The check runs
//! in a detached child so serving the client never waits on the network.

use std::io::Write;

use crewkit_core::paths::Paths;
use crewkit_core::updater::{self, Trigger, CHECK_INTERVAL};

pub fn spawn_if_due(paths: &Paths) {
    let crewkit_dir = paths.crewkit_dir();
    let Ok(program) = std::env::current_exe() else {
        return;
    };
    updater::spawn_if_due(&crewkit_dir, &program);
    std::thread::spawn(move || loop {
        std::thread::sleep(CHECK_INTERVAL);
        updater::spawn_if_due(&crewkit_dir, &program);
    });
}

pub fn run(paths: &Paths) -> Result<(), String> {
    let crewkit_dir = paths.crewkit_dir();
    let mut log = std::fs::File::create(crewkit_dir.join("updater.log")).ok();
    let mut line = |text: String| {
        if let Some(log) = log.as_mut() {
            let _ = writeln!(log, "{text}");
        }
    };
    line(format!(
        "crewkit-bridge {} update",
        env!("CARGO_PKG_VERSION")
    ));
    let report = updater::run(paths, Trigger::Bridge, None, |step| {
        line(format!(
            "{:?} {} {}: {}",
            step.status, step.client, step.step, step.message
        ));
    })
    .map_err(|e| e.to_string())?;
    match report {
        None => line("skipped: not due or another updater holds the lock".into()),
        Some(report) => {
            for kit in report.kits {
                line(format!(
                    "kit {}: updated {}, added {}, removed {}{}",
                    kit.kit,
                    kit.diff.updated.len(),
                    kit.diff.added.len(),
                    kit.diff.removed.len(),
                    kit.error.map(|e| format!(" — {e}")).unwrap_or_default()
                ));
            }
        }
    }
    match updater::update_app(&crewkit_dir) {
        Ok(Some(version)) => line(format!("app updated to {version}")),
        Ok(None) => {}
        Err(e) => line(format!("app update: {e}")),
    }
    Ok(())
}
