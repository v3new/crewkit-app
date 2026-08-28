use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{io_ctx, Error, Result};

/// Write a file atomically: temp file in the same directory, then rename.
/// A partially written config can never be observed by a client.
pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Invalid(format!("no parent directory for {}", path.display())))?;
    std::fs::create_dir_all(dir).map_err(io_ctx(format!("creating {}", dir.display())))?;
    let tmp = dir.join(format!(
        ".crewkit-tmp-{}-{}",
        std::process::id(),
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, content).map_err(io_ctx(format!("writing {}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(io_ctx(format!("renaming into {}", path.display())))?;
    Ok(())
}

/// Copies every file it is about to modify into a timestamped snapshot
/// directory, once per file per run. Snapshots enable manual rollback.
pub struct Snapshotter {
    dir: PathBuf,
    taken: Vec<PathBuf>,
}

impl Snapshotter {
    pub fn new(crewkit_dir: &Path) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            dir: crewkit_dir.join("snapshots").join(stamp.to_string()),
            taken: Vec::new(),
        }
    }

    /// Back up `file` if it exists and was not already snapshotted this run.
    pub fn backup(&mut self, file: &Path) -> Result<Option<PathBuf>> {
        if !file.exists() || self.taken.iter().any(|t| t == file) {
            return Ok(None);
        }
        let name = flatten_path(file);
        let dest = self.dir.join(&name);
        if dest == file {
            return Err(Error::Invalid(format!(
                "snapshot destination resolves to the source file: {}",
                file.display()
            )));
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(io_ctx(format!("creating {}", self.dir.display())))?;
        std::fs::copy(file, &dest).map_err(io_ctx(format!("snapshotting {}", file.display())))?;
        self.taken.push(file.to_path_buf());

        // Record the original location so a snapshot can be rolled back.
        let index_path = self.dir.join("index.json");
        let mut index = read_json(&index_path)?.unwrap_or_else(|| serde_json::json!({}));
        index[&name] = serde_json::json!(file.to_string_lossy());
        atomic_write(
            &index_path,
            serde_json::to_string_pretty(&index)
                .expect("index")
                .as_bytes(),
        )?;
        Ok(Some(dest))
    }
}

/// Flatten an absolute path into a single file name. Every separator and
/// the Windows drive colon must go: a name that keeps a root (`C:\…`)
/// would make `dir.join(name)` resolve back to the source file itself,
/// and copying a file onto itself fails with a sharing violation.
fn flatten_path(file: &Path) -> String {
    let name: String = file
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '_',
            c => c,
        })
        .collect();
    name.trim_matches('_').to_string()
}

/// Restore every file from the most recent snapshot. Returns the restored
/// paths; an empty list means there was nothing to roll back.
pub fn rollback_latest(crewkit_dir: &Path) -> Result<Vec<PathBuf>> {
    let snapshots = crewkit_dir.join("snapshots");
    let Ok(entries) = std::fs::read_dir(&snapshots) else {
        return Ok(Vec::new());
    };
    let mut runs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("index.json").exists())
        .collect();
    runs.sort();
    let Some(latest) = runs.pop() else {
        return Ok(Vec::new());
    };

    let index = read_json(&latest.join("index.json"))?.unwrap_or_default();
    let mut restored = Vec::new();
    if let Some(map) = index.as_object() {
        for (name, original) in map {
            let Some(original) = original.as_str() else {
                continue;
            };
            let bytes = std::fs::read(latest.join(name))
                .map_err(io_ctx(format!("reading snapshot {name}")))?;
            atomic_write(Path::new(original), &bytes)?;
            restored.push(PathBuf::from(original));
        }
    }
    // A consumed snapshot is retired so repeated rollbacks walk backwards.
    let _ = std::fs::rename(&latest, latest.with_extension("restored"));
    Ok(restored)
}

/// Extract a zip archive using the platform's native tool.
pub fn extract_zip(zip: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(io_ctx(format!("creating {}", dest.display())))?;
    let mut command;
    #[cfg(target_os = "macos")]
    {
        command = crate::cli::command("/usr/bin/ditto");
        command.args(["-x", "-k"]).arg(zip).arg(dest);
    }
    #[cfg(windows)]
    {
        // System bsdtar (ships with Windows 10+) extracts zip archives.
        command = crate::cli::command("tar");
        command.arg("-xf").arg(zip).arg("-C").arg(dest);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        command = crate::cli::command("unzip");
        command.args(["-o", "-q"]).arg(zip).arg("-d").arg(dest);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .map_err(io_ctx(format!("running {program}")))?;
    if !output.status.success() {
        return Err(Error::Cli {
            command: format!("{program} {}", zip.display()),
            output: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Recursively copy a directory tree.
pub fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).map_err(io_ctx(format!("creating {}", dest.display())))?;
    let entries = std::fs::read_dir(src).map_err(io_ctx(format!("reading {}", src.display())))?;
    for entry in entries {
        let entry = entry.map_err(io_ctx(format!("reading {}", src.display())))?;
        let target = dest.join(entry.file_name());
        let file_type = entry.file_type().map_err(io_ctx("stat"))?;
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(io_ctx(format!("copying {}", entry.path().display())))?;
        }
    }
    Ok(())
}

/// Read a JSON file if it exists; `None` when the file is absent.
pub fn read_json(path: &Path) -> Result<Option<serde_json::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).map_err(io_ctx(format!("reading {}", path.display())))?;
    let value = serde_json::from_str(&text).map_err(|e| Error::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_path_strips_every_separator_and_drive_prefix() {
        // Regression: a Windows path has no `/` to replace, so the old
        // flattening left it absolute and `dir.join(name)` resolved to
        // the source file itself (fs::copy onto itself → os error 32).
        for raw in [
            r"C:\Users\anton\.codex\config.toml",
            "/Users/anton/.codex/config.toml",
            r"\\server\share\config.toml",
        ] {
            let name = flatten_path(Path::new(raw));
            assert!(!name.is_empty(), "{raw}");
            assert!(!name.contains(['/', '\\', ':']), "{raw} -> {name}");
            let joined = Path::new("base").join(&name);
            assert!(joined.starts_with("base"), "{raw} -> {joined:?}");
        }
    }

    #[test]
    fn backup_lands_inside_the_snapshot_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "original = true\n").unwrap();

        let crewkit_dir = tmp.path().join("crewkit");
        let mut snap = Snapshotter::new(&crewkit_dir);
        let dest = snap.backup(&file).unwrap().expect("first backup taken");

        assert_ne!(dest, file);
        assert!(dest.starts_with(crewkit_dir.join("snapshots")));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "original = true\n");
        // Same file again in one run: already covered, no second copy.
        assert!(snap.backup(&file).unwrap().is_none());
    }
}
