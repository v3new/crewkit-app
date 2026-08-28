use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{io_ctx, Error, Result};

pub struct CliOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr).trim().to_string()
    }
}

/// A `Command` that never flashes a console window: CrewKit's GUI process
/// has no console on Windows, so every child console process would pop
/// its own window for a moment without CREATE_NO_WINDOW.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Run a client CLI with a hard timeout. Some client commands block on
/// network or interactive auth (observed with `codex mcp add`), and a
/// hung child process must never hang the installer.
pub fn run(
    program: &Path,
    args: &[&str],
    envs: &[(String, String)],
    timeout: Duration,
) -> Result<CliOutput> {
    let command_desc = format!("{} {}", program.display(), args.join(" "));
    let mut child = command(program)
        .args(args)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(io_ctx(format!("spawning {command_desc}")))?;

    // Drain pipes on threads so a chatty child cannot deadlock on a full pipe.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(io_ctx(format!("waiting for {command_desc}")))?
        {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::CliTimeout {
                command: command_desc,
                seconds: timeout.as_secs(),
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    Ok(CliOutput {
        code: status.code(),
        stdout: stdout_thread.join().unwrap_or_default(),
        stderr: stderr_thread.join().unwrap_or_default(),
    })
}
