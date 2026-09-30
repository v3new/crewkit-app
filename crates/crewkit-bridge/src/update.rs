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
    let mut log: Option<std::fs::File> = None;
    let mut line = |text: String| {
        if log.is_none() {
            log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(crewkit_dir.join("updater.log"))
                .ok();
        }
        if let Some(log) = log.as_mut() {
            let _ = writeln!(log, "{text}");
        }
    };
    let report = updater::run(paths, Trigger::Bridge, None, |step| {
        line(format!(
            "{:?} {} {}: {}",
            step.status, step.client, step.step, step.message
        ));
    })
    .map_err(|e| e.to_string())?;
    let Some(report) = report else {
        return Ok(());
    };
    line(format!(
        "crewkit-bridge {} update",
        env!("CARGO_PKG_VERSION")
    ));
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
    match updater::update_app(&crewkit_dir) {
        Ok(Some(version)) => line(format!("app updated to {version}")),
        Ok(None) => {}
        Err(e) => line(format!("app update: {e}")),
    }
    Ok(())
}
