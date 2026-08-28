use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::adapter::{resolve_glob, Adapter};
use crate::cli;
use crate::paths::Paths;

/// `--version` is a local, near-instant call; a CLI that cannot answer
/// it within this window is treated as version-unknown, not absent.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

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
    /// The desktop app's own version (distinct from the CLI's) — read
    /// from the bundle's Info.plist on macOS, None elsewhere.
    pub app_version: Option<String>,
    /// Resolved CLI binary — from PATH, or bundled inside the client's
    /// app package (clients ship their CLI even when the user never
    /// installed one; CrewKit drives that binary directly).
    pub cli_path: Option<PathBuf>,
    /// The version that binary reports, when it answers `--version`
    /// with something version-shaped.
    pub cli_version: Option<String>,
    pub files: Vec<FileProbe>,
    pub restart_required: bool,
    pub notes: Option<String>,
    pub present: bool,
}

pub fn detect_all(adapters: &[Adapter], paths: &Paths) -> Vec<DetectedClient> {
    adapters.iter().map(|a| detect_one(a, paths)).collect()
}

fn detect_one(adapter: &Adapter, paths: &Paths) -> DetectedClient {
    let app_path = adapter
        .app_paths
        .iter()
        .map(|p| paths.expand(p))
        .find(|p| p.exists());
    let app_installed = app_path.is_some();
    let app_version = app_path.as_deref().and_then(app_bundle_version);

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(spec) = adapter.cli.as_ref() {
        candidates.extend(find_on_path(&spec.path_names));
        for glob in &spec.bundled_globs {
            for hit in resolve_glob(&paths.expand(glob)) {
                if !candidates.contains(&hit) {
                    candidates.push(hit);
                }
            }
        }
    }
    let (cli_path, cli_version) = pick_cli(candidates);

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
        app_version,
        cli_path,
        cli_version,
        files,
        restart_required: adapter.restart_required,
        notes: adapter.notes.clone(),
        present,
    }
}

/// `CFBundleShortVersionString` of a macOS app bundle, via `defaults`
/// (Info.plist may be binary — `defaults` reads both, no plist parser
/// needed; apps are never launched for this). None off macOS and for
/// paths that are not `.app` bundles (Windows install dirs).
#[cfg(target_os = "macos")]
fn app_bundle_version(app_path: &std::path::Path) -> Option<String> {
    if app_path.extension().is_none_or(|e| e != "app") {
        return None;
    }
    let plist = app_path.join("Contents/Info");
    let output = cli::run(
        std::path::Path::new("/usr/bin/defaults"),
        &[
            "read",
            &plist.to_string_lossy(),
            "CFBundleShortVersionString",
        ],
        &[],
        VERSION_PROBE_TIMEOUT,
    )
    .ok()
    .filter(|o| o.success())?;
    let version = output.stdout.trim().to_string();
    (!version.is_empty()).then_some(version)
}

#[cfg(not(target_os = "macos"))]
fn app_bundle_version(_app_path: &std::path::Path) -> Option<String> {
    None
}

/// A ranked CLI candidate: version numbers, mtime, path, display version.
type CliCandidate = (
    Option<Vec<u64>>,
    Option<SystemTime>,
    PathBuf,
    Option<String>,
);

/// Choose among CLI candidates (the PATH hit plus bundled copies): the
/// highest reported `--version` wins, with mtime as the tiebreak — a
/// stale user-installed binary must not shadow a newer bundled one, and
/// hash-named bundle directories carry no version in their path. On a
/// complete tie the earlier candidate (PATH first) is kept.
fn pick_cli(candidates: Vec<PathBuf>) -> (Option<PathBuf>, Option<String>) {
    let mut best: Option<CliCandidate> = None;
    for path in candidates {
        let reported = cli::run(&path, &["--version"], &[], VERSION_PROBE_TIMEOUT)
            .ok()
            .filter(|o| o.success())
            .map(|o| o.combined());
        let (nums, display) = match reported.as_deref().and_then(version_token) {
            Some((nums, display)) => (Some(nums), Some(display)),
            None => (None, None),
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let better = match &best {
            None => true,
            Some((best_nums, best_mtime, _, _)) => (&nums, &mtime) > (best_nums, best_mtime),
        };
        if better {
            best = Some((nums, mtime, path, display));
        }
    }
    match best {
        Some((_, _, path, version)) => (Some(path), version),
        None => (None, None),
    }
}

/// First version-shaped token of a `--version` output: the numeric
/// components for ordering plus the token itself for display (e.g.
/// "0.150.0-alpha.12.2" orders as [0, 150, 0]; "2.1.246 (Claude Code)"
/// yields "2.1.246").
fn version_token(text: &str) -> Option<(Vec<u64>, String)> {
    for raw in text.split_whitespace() {
        let token = raw.trim_start_matches('v');
        let mut nums = Vec::new();
        for part in token.split(['.', '-']) {
            match part.parse::<u64>() {
                Ok(n) => nums.push(n),
                Err(_) => break,
            }
        }
        if nums.len() >= 2 {
            return Some((nums, token.to_string()));
        }
    }
    None
}

fn find_on_path(names: &[String]) -> Option<PathBuf> {
    // On Windows executables carry an extension (claude.exe from the
    // native installer, claude.cmd from an npm shim) — probe those too.
    #[cfg(windows)]
    const EXTENSIONS: &[&str] = &["exe", "cmd", "bat"];
    #[cfg(not(windows))]
    const EXTENSIONS: &[&str] = &[];

    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
            for ext in EXTENSIONS {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_token_shapes() {
        assert_eq!(
            version_token("2.1.246 (Claude Code)"),
            Some((vec![2, 1, 246], "2.1.246".into()))
        );
        assert_eq!(
            version_token("codex-cli 0.150.0-alpha.12.2"),
            Some((vec![0, 150, 0], "0.150.0-alpha.12.2".into()))
        );
        assert_eq!(version_token("v1.2"), Some((vec![1, 2], "1.2".into())));
        assert_eq!(version_token("no version here"), None);
        assert_eq!(version_token(""), None);
    }

    /// A stale binary earlier in the candidate list must lose to a newer
    /// one found later (the client-machine case: old ~/.local/bin/claude
    /// shadowing a fresh bundled copy).
    #[test]
    #[cfg(unix)]
    fn pick_cli_prefers_highest_version() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut fakes = Vec::new();
        for (name, version) in [("old", "1.0.30"), ("new", "2.1.246")] {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho \"{version} (Fake)\"\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            fakes.push(path);
        }
        let (best, version) = pick_cli(fakes.clone());
        assert_eq!(best.as_deref(), Some(fakes[1].as_path()));
        assert_eq!(version.as_deref(), Some("2.1.246"));
    }

    /// An app bundle's version comes from its Info.plist, without
    /// launching anything.
    #[test]
    #[cfg(target_os = "macos")]
    fn app_bundle_version_reads_info_plist() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(tmp.path());
        let app = paths.home.join("Applications/Fake.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleShortVersionString</key>
  <string>9.9.9</string>
</dict>
</plist>
"#,
        )
        .unwrap();
        let adapter = crate::adapter::Adapter::load(
            r#"{
              "id": "fake",
              "name": "Fake",
              "appPaths": ["${home}/Applications/Fake.app"]
            }"#,
        )
        .unwrap();
        let detected = detect_all(&[adapter], &paths);
        assert!(detected[0].app_installed);
        assert_eq!(detected[0].app_version.as_deref(), Some("9.9.9"));
    }

    /// A candidate that cannot answer `--version` is still usable when
    /// it is the only one.
    #[test]
    #[cfg(unix)]
    fn pick_cli_keeps_versionless_candidate() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mute");
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (best, version) = pick_cli(vec![path.clone()]);
        assert_eq!(best, Some(path));
        assert_eq!(version, None);
    }
}
