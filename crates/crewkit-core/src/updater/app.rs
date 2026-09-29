use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;

use crate::bridge;
use crate::error::{io_ctx, Error, Result};
use crate::lock::FileLock;
use crate::updater::state::{AppInfo, Event, UpdateState};

/// The same endpoint and key the desktop app's Tauri updater uses
/// (`desktop/src-tauri/tauri.conf.json`), so the bridge can ship the
/// same signed release while the app is not running.
pub const ENDPOINT: &str =
    "https://github.com/v3new/crewkit-app/releases/latest/download/latest.json";
pub const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDczQkQ3NDQzMzlDRjA0MEUKUldRT0JNODVRM1M5YzgzR3NnK3E2WXhpTEUrYmNwdk13M0RrTVoraXNadHJDbjhmS1MxenFaR1cK";

const DOWNLOAD_CAP: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub signature: String,
}

#[derive(Deserialize)]
struct Latest {
    version: String,
    platforms: BTreeMap<String, Platform>,
}

#[derive(Deserialize)]
struct Platform {
    signature: String,
    url: String,
}

fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

pub fn latest() -> Result<Release> {
    let latest: Latest = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .get(ENDPOINT)
        .call()
        .map_err(|e| Error::Invalid(format!("GET {ENDPOINT} failed: {e}")))?
        .into_json()
        .map_err(|e| Error::Invalid(format!("{ENDPOINT}: {e}")))?;
    let key = platform_key();
    let platform = latest
        .platforms
        .get(&key)
        .ok_or_else(|| Error::Invalid(format!("no `{key}` build in {ENDPOINT}")))?;
    Ok(Release {
        version: latest.version,
        url: platform.url.clone(),
        signature: platform.signature.clone(),
    })
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    parse(candidate) > parse(current)
}

pub fn verify(bytes: &[u8], signature_b64: &str) -> Result<()> {
    verify_with(PUBKEY, bytes, signature_b64)
}

pub fn verify_with(pubkey_b64: &str, bytes: &[u8], signature_b64: &str) -> Result<()> {
    let decode = |what: &str, text: &str| {
        B64.decode(text.trim())
            .ok()
            .and_then(|raw| String::from_utf8(raw).ok())
            .ok_or_else(|| Error::Invalid(format!("release {what} is not base64 text")))
    };
    let public_key = PublicKey::decode(&decode("public key", pubkey_b64)?)
        .map_err(|e| Error::Invalid(format!("release public key: {e}")))?;
    let signature = Signature::decode(&decode("signature", signature_b64)?)
        .map_err(|e| Error::Invalid(format!("release signature: {e}")))?;
    public_key
        .verify(bytes, &signature, false)
        .map_err(|_| Error::Invalid("release signature verification FAILED".into()))
}

pub fn app_lock_path(crewkit_dir: &Path) -> PathBuf {
    crewkit_dir.join("app.lock")
}

/// Held by the desktop app for its whole lifetime: the bridge's updater
/// leaves a running app to update itself.
pub fn hold_app_lock(crewkit_dir: &Path) -> Option<FileLock> {
    std::fs::create_dir_all(crewkit_dir).ok()?;
    FileLock::acquire(app_lock_path(crewkit_dir), Duration::MAX)
}

pub fn app_running(crewkit_dir: &Path) -> bool {
    FileLock::holder_alive(&app_lock_path(crewkit_dir))
}

/// Install the latest signed release over the app recorded in the state
/// file. Returns the version installed; `None` when nothing was needed
/// or the app is running and will update itself.
pub fn check_and_install(crewkit_dir: &Path) -> Result<Option<String>> {
    let Some(app) = UpdateState::load(crewkit_dir)?.app else {
        return Ok(None);
    };
    let release = latest()?;
    if !is_newer(&release.version, &app.version) {
        return Ok(None);
    }
    if app_running(crewkit_dir) {
        UpdateState::modify(crewkit_dir, |state| {
            state.app_update_available = Some(release.version)
        })?;
        return Ok(None);
    }
    let archive = download(&release.url, &crewkit_dir.join("downloads"))?;
    let bytes = std::fs::read(&archive).map_err(io_ctx("reading downloaded release"))?;
    verify(&bytes, &release.signature)?;
    install(&archive, &app.app_path)?;
    let _ = std::fs::remove_file(&archive);
    bridge::install_bridge(&app.bridge_source, crewkit_dir)?;
    UpdateState::modify(crewkit_dir, |state| {
        state.app = Some(AppInfo {
            version: release.version.clone(),
            ..app
        });
        state.app_update_available = None;
        state.notify(Event::AppUpdated {
            version: release.version.clone(),
        });
    })?;
    Ok(Some(release.version))
}

fn download(url: &str, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(io_ctx(format!("creating {}", dir.display())))?;
    let name = url.rsplit('/').next().unwrap_or("release");
    let dest = dir.join(name);
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10 * 60))
        .build()
        .get(url)
        .call()
        .map_err(|e| Error::Invalid(format!("GET {url} failed: {e}")))?;
    let mut file =
        std::fs::File::create(&dest).map_err(io_ctx(format!("creating {}", dest.display())))?;
    let mut body = response.into_reader().take(DOWNLOAD_CAP);
    std::io::copy(&mut body, &mut file).map_err(io_ctx(format!("downloading {url}")))?;
    Ok(dest)
}

#[cfg(target_os = "macos")]
fn install(archive: &Path, app_path: &Path) -> Result<()> {
    let parent = app_path
        .parent()
        .ok_or_else(|| Error::Invalid(format!("{} has no parent", app_path.display())))?;
    let staging = parent.join(".crewkit-update");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(io_ctx(format!("creating {}", staging.display())))?;
    let output = crate::cli::run(
        Path::new("/usr/bin/tar"),
        &[
            "-xzf",
            &archive.to_string_lossy(),
            "-C",
            &staging.to_string_lossy(),
        ],
        &[],
        Duration::from_secs(5 * 60),
    )?;
    if !output.success() {
        return Err(Error::Invalid(format!(
            "extracting release: {}",
            output.combined()
        )));
    }
    let bundle_name = app_path.file_name().unwrap_or_default();
    let new_app = staging.join(bundle_name);
    if !new_app.is_dir() {
        return Err(Error::Invalid(format!(
            "release archive has no {}",
            bundle_name.to_string_lossy()
        )));
    }
    let retired = parent.join(format!("{}.old", bundle_name.to_string_lossy()));
    let _ = std::fs::remove_dir_all(&retired);
    std::fs::rename(app_path, &retired).map_err(io_ctx("retiring the current app"))?;
    if let Err(e) = std::fs::rename(&new_app, app_path) {
        let _ = std::fs::rename(&retired, app_path);
        return Err(io_ctx("moving the new app into place")(e));
    }
    let _ = std::fs::remove_dir_all(&retired);
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

#[cfg(windows)]
fn install(archive: &Path, _app_path: &Path) -> Result<()> {
    let output = crate::cli::run(archive, &["/S"], &[], Duration::from_secs(10 * 60))?;
    if output.success() {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "installer failed: {}",
            output.combined()
        )))
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn install(_archive: &Path, _app_path: &Path) -> Result<()> {
    Err(Error::Invalid(
        "app self-update is not supported on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("1.8.0", "1.7.0"));
        assert!(is_newer("2.0.0", "1.99.9"));
        assert!(!is_newer("1.7.0", "1.7.0"));
        assert!(!is_newer("1.6.9", "1.7.0"));
        assert!(is_newer("v1.7.1-beta.1", "1.7.0"));
    }

    #[test]
    fn signature_roundtrip_and_tamper_detection() {
        let keys = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let data = b"CrewKit release bytes";
        let signature = minisign::sign(None, &keys.sk, &data[..], None, None).unwrap();
        let pubkey_b64 = B64.encode(keys.pk.to_box().unwrap().into_string());
        let signature_b64 = B64.encode(signature.into_string());
        verify_with(&pubkey_b64, data, &signature_b64).unwrap();
        assert!(verify_with(&pubkey_b64, b"tampered", &signature_b64).is_err());
    }

    #[test]
    fn constants_match_the_desktop_updater_config() {
        let conf = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../desktop/src-tauri/tauri.conf.json"),
        )
        .unwrap();
        let conf: serde_json::Value = serde_json::from_str(&conf).unwrap();
        let updater = &conf["plugins"]["updater"];
        assert_eq!(updater["pubkey"], PUBKEY);
        assert_eq!(updater["endpoints"][0], ENDPOINT);
    }
}
