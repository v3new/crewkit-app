//! Client detection against the shipped adapters, in a sandbox root.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crewkit_core::{assets, detect, Paths};

fn fake_cli(bin: &Path, version: &str) {
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(
        bin,
        format!("#!/bin/sh\necho \"{version} (Claude Code)\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Current Claude Desktop builds keep the bundled CLI one level deeper,
/// under a hash directory: claude-code/<ver>/<hash>/claude.app. The
/// shipped adapter must find it there, and still rank it against a copy
/// in the older flat layout by version. Detection still probes the real
/// PATH and /opt/homebrew, so the fake versions sit far above any real
/// release.
#[test]
fn claude_cli_found_in_hash_dir_bundle() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::rooted(root.path());
    let bundles = paths.app_support.join("Claude/claude-code");
    let exe = "claude.app/Contents/MacOS/claude";
    fake_cli(&bundles.join("900.0.0").join(exe), "900.0.0");
    fake_cli(&bundles.join("900.1.0/4fa9694b7975").join(exe), "900.1.0");

    let adapter = assets::adapters()
        .unwrap()
        .into_iter()
        .find(|a| a.id == "claude-code")
        .unwrap();
    let client = detect::detect_all(&[adapter], &paths).remove(0);

    assert_eq!(client.cli_version.as_deref(), Some("900.1.0"));
    assert!(client
        .cli_path
        .unwrap()
        .ends_with(Path::new("900.1.0/4fa9694b7975").join(exe)));
}
