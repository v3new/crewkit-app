use std::path::Path;
use std::process::Stdio;

use crate::updater::state::{now_unix, UpdateState};

/// Kick off `<program> update` as a process of its own when the hourly
/// check is due. The caller (a bridge serving a client) never waits: the
/// updater outlives the client's session and takes the lock itself.
pub fn spawn_if_due(crewkit_dir: &Path, program: &Path) -> bool {
    let due = UpdateState::load(crewkit_dir)
        .map(|state| state.is_due(now_unix()))
        .unwrap_or(true);
    if !due {
        return false;
    }
    spawn_detached(program)
}

#[cfg(unix)]
fn spawn_detached(program: &Path) -> bool {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(program)
        .arg("update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .is_ok()
}

#[cfg(windows)]
fn spawn_detached(program: &Path) -> bool {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        let spawned = std::process::Command::new(program)
            .arg("update")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn();
        if spawned.is_ok() {
            return true;
        }
    }
    false
}
