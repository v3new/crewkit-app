use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Cross-process lock: the holder does the work, everyone else waits for
/// what it stores. Stale locks (crashed process) expire.
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    pub fn acquire(path: PathBuf, stale_after: Duration) -> Option<Self> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                // Record the holder so a crashed process's lock can be
                // recognized as stale immediately, not after a timeout.
                let _ = write!(file, "{}", std::process::id());
                Some(Self { path })
            }
            Err(_) => {
                let stale = match Self::holder(&path) {
                    Some(pid) => !process_alive(pid),
                    None => std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().unwrap_or_default() > stale_after)
                        .unwrap_or(true),
                };
                if stale && Self::reclaim(&path) {
                    return Self::acquire(path, stale_after);
                }
                None
            }
        }
    }

    /// Only the contender whose rename wins may retry; the others see the
    /// lock as taken, which it is about to be.
    fn reclaim(path: &std::path::Path) -> bool {
        let claimed = path.with_extension(format!("stale.{}", std::process::id()));
        let won = std::fs::rename(path, &claimed).is_ok();
        let _ = std::fs::remove_file(&claimed);
        won
    }

    pub fn holder(path: &std::path::Path) -> Option<u32> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    }

    pub fn holder_alive(path: &std::path::Path) -> bool {
        Self::holder(path).is_some_and(process_alive)
    }

    /// Take the lock over from a live holder — for explicit logins, which
    /// must always reach the browser. The previous holder keeps waiting on
    /// its own callback listener and resolves from whichever flow the
    /// user completes; its guarded `Drop` leaves this lock alone.
    pub fn steal(path: PathBuf) -> Self {
        let _ = std::fs::remove_file(&path);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
        {
            let _ = write!(file, "{}", std::process::id());
        }
        Self { path }
    }
}

#[cfg(not(windows))]
pub fn process_alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    // tasklist prints a table row for a live pid and an info message
    // otherwise; matching the pid in the output separates the two.
    crate::cli::command("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Remove the file only while it is still ours: a preempting login
        // may have replaced it, and that lock must survive our exit.
        let mine = std::fs::read_to_string(&self.path)
            .map(|s| s.trim() == std::process::id().to_string())
            .unwrap_or(false);
        if mine {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
