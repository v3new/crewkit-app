use std::path::PathBuf;

use serde::Serialize;

use crate::adapter::{resolve_glob, Adapter};
use crate::paths::Paths;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileProbe {
    pub key: String,
    pub path: PathBuf,
    pub exists: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedClient {
    pub id: String,
    pub name: String,
    pub app_installed: bool,
    /// Resolved CLI binary — from PATH, or bundled inside the client's
    /// app package (clients ship their CLI even when the user never
    /// installed one; CrewKit drives that binary directly).
    pub cli_path: Option<PathBuf>,
    pub files: Vec<FileProbe>,
    pub restart_required: bool,
    pub notes: Option<String>,
    pub present: bool,
}

pub fn detect_all(adapters: &[Adapter], paths: &Paths) -> Vec<DetectedClient> {
    adapters.iter().map(|a| detect_one(a, paths)).collect()
}

fn detect_one(adapter: &Adapter, paths: &Paths) -> DetectedClient {
    let app_installed = adapter.app_paths.iter().any(|p| paths.expand(p).exists());

    let cli_path = adapter.cli.as_ref().and_then(|spec| {
        find_on_path(&spec.path_names).or_else(|| {
            spec.bundled_globs
                .iter()
                .flat_map(|g| resolve_glob(&paths.expand(g)))
                .last() // matches are sorted; last is the newest version
        })
    });

    let files: Vec<FileProbe> = adapter
        .files
        .iter()
        .map(|(key, template)| {
            let path = paths.expand(template);
            FileProbe {
                key: key.clone(),
                exists: path.exists(),
                path,
            }
        })
        .collect();

    // A helper CLI (e.g. npx) is a dependency of the install path, not
    // evidence the client itself is installed.
    let cli_is_evidence = adapter.cli.as_ref().is_some_and(|c| !c.helper);
    let present =
        app_installed || (cli_is_evidence && cli_path.is_some()) || files.iter().any(|f| f.exists);
    DetectedClient {
        id: adapter.id.clone(),
        name: adapter.name.clone(),
        app_installed,
        cli_path,
        files,
        restart_required: adapter.restart_required,
        notes: adapter.notes.clone(),
        present,
    }
}

fn find_on_path(names: &[String]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
