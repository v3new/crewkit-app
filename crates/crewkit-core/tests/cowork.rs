//! Cowork's per-org plugin store must always hold maps the app can read.

use crewkit_core::{cowork, Paths};
use serde_json::json;
use serde_json::Value;

fn profile(root: &std::path::Path) -> cowork::Profile {
    let paths = Paths::rooted(root);
    let org = paths
        .claude_desktop_dir()
        .join("local-agent-mode-sessions/account/org");
    std::fs::create_dir_all(org.join("cowork_plugins")).unwrap();
    cowork::profiles(&paths).remove(0)
}

fn read(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn removing_from_an_empty_profile_writes_no_null_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = profile(tmp.path());
    assert!(!cowork::remove_plugin(&profile, "mkt", "tool").unwrap());

    let installed = read(&profile.plugins_dir().join("installed_plugins.json"));
    assert!(installed.get("plugins").is_none(), "{installed}");
    let settings = read(&profile.dir.join("cowork_settings.json"));
    assert!(settings.get("enabledPlugins").is_none(), "{settings}");
}

#[test]
fn repair_replaces_null_maps_and_leaves_good_files_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = profile(tmp.path());
    let installed = profile.plugins_dir().join("installed_plugins.json");
    let settings = profile.dir.join("cowork_settings.json");
    std::fs::write(&installed, r#"{"plugins": null}"#).unwrap();
    std::fs::write(&settings, r#"{"enabledPlugins": null, "other": 1}"#).unwrap();

    cowork::repair(&profile).unwrap();
    assert_eq!(read(&installed)["plugins"], json!({}));
    assert_eq!(read(&settings)["enabledPlugins"], json!({}));
    assert_eq!(read(&settings)["other"], 1);
}
