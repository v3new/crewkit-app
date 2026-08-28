//! Deployment of the crewkit-bridge binary and the data it needs.
//!
//! The bridge is a standalone stdio⇄HTTP MCP proxy that owns OAuth at the
//! CrewKit level: every client gets the same `crewkit-bridge <server-id>`
//! entry, and one browser login serves them all. It lives at a stable
//! path under the CrewKit data directory — client config entries survive
//! app updates and even the app being moved.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

use crate::error::{io_ctx, Result};
use crate::fsops;
use crate::kit::Kit;

#[cfg(windows)]
pub const BRIDGE_BIN_NAME: &str = "crewkit-bridge.exe";
#[cfg(not(windows))]
pub const BRIDGE_BIN_NAME: &str = "crewkit-bridge";

/// Stable installed location: `<crewkit dir>/bin/crewkit-bridge`.
pub fn bridge_path(crewkit_dir: &Path) -> PathBuf {
    crewkit_dir.join("bin").join(BRIDGE_BIN_NAME)
}

/// Copy the bundled bridge binary to its stable path (atomic, executable).
/// Returns whether anything changed.
pub fn install_bridge(source: &Path, crewkit_dir: &Path) -> Result<bool> {
    let dest = bridge_path(crewkit_dir);
    let bytes = std::fs::read(source).map_err(io_ctx(format!("reading {}", source.display())))?;
    if let Ok(existing) = std::fs::read(&dest) {
        if existing == bytes {
            return Ok(false);
        }
    }
    fsops::atomic_write(&dest, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .map_err(io_ctx(format!("chmod {}", dest.display())))?;
    }
    Ok(true)
}

/// Merge this kit's servers into the shared id → URL mapping the bridge
/// resolves from. Several kits share one file: only this kit's active
/// servers are upserted and only its removed ones are dropped.
pub fn write_servers_config(kit: &Kit, crewkit_dir: &Path) -> Result<()> {
    let path = crewkit_dir.join("servers.json");
    let mut config = fsops::read_json(&path)?.unwrap_or_else(|| json!({ "servers": {} }));
    let servers = config
        .as_object_mut()
        .and_then(|c| {
            if !c.contains_key("servers") {
                c.insert("servers".into(), json!({}));
            }
            c.get_mut("servers")
        })
        .and_then(|s| s.as_object_mut())
        .ok_or_else(|| crate::error::Error::Invalid("servers.json is malformed".into()))?;
    for server in kit.active_mcp_servers().filter(|s| s.transport_supported()) {
        servers.insert(server.id.clone(), json!({ "url": server.url }));
    }
    for server in kit.mcp_servers.iter().filter(|s| s.remove) {
        servers.remove(&server.id);
    }
    let text = serde_json::to_string_pretty(&config).expect("servers config serializes");
    fsops::atomic_write(&path, text.as_bytes())
}

/// The stdio entry JSON-config clients get (Claude Desktop).
pub fn stdio_entry(bridge: &Path, server_id: &str) -> serde_json::Value {
    json!({
        "command": bridge.to_string_lossy(),
        "args": [server_id],
    })
}

/// Whether a config entry launches the bridge (i.e. is CrewKit-shaped).
pub fn is_bridge_command(command: Option<&str>) -> bool {
    command.is_some_and(|c| c.ends_with(BRIDGE_BIN_NAME))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthState {
    pub id: String,
    pub authorized: bool,
}

/// Which servers already have a cached CrewKit-level session.
pub fn auth_status(kit: &Kit, crewkit_dir: &Path) -> Vec<AuthState> {
    // Open endpoints (`"auth": "none"`) have no session to report, so
    // they are absent here and the UI offers no authorize step.
    kit.active_mcp_servers()
        .filter(|s| s.uses_oauth())
        .map(|s| AuthState {
            id: s.id.clone(),
            authorized: session::load(crewkit_dir, &s.id).is_some(),
        })
        .collect()
}

/// MCP session storage: macOS Keychain first (service "CrewKit MCP",
/// account = server id, via the `security` CLI), with the pre-Keychain
/// 0600 file as fallback and one-way migration source. Shared by the
/// engine (status/removal) and the crewkit-bridge binary (login/serve).
pub mod session {
    use std::path::{Path, PathBuf};

    // Only the macOS Keychain and Windows Credential Manager paths
    // reference this; other platforms use the file fallback and would
    // flag it as dead code.
    #[cfg(any(target_os = "macos", windows))]
    const SERVICE: &str = "CrewKit MCP";

    fn legacy_path(crewkit_dir: &Path, server_id: &str) -> PathBuf {
        crewkit_dir.join("auth").join(format!("{server_id}.json"))
    }

    #[cfg(target_os = "macos")]
    fn keychain_load(server_id: &str) -> Option<String> {
        let output = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                SERVICE,
                "-a",
                server_id,
                "-w",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let secret = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!secret.is_empty()).then_some(secret)
    }

    #[cfg(target_os = "macos")]
    fn keychain_save(server_id: &str, secret: &str) -> bool {
        std::process::Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                SERVICE,
                "-a",
                server_id,
                "-w",
                secret,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(target_os = "macos")]
    fn keychain_delete(server_id: &str) -> bool {
        std::process::Command::new("/usr/bin/security")
            .args(["delete-generic-password", "-s", SERVICE, "-a", server_id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Windows Credential Manager, generic credentials via wincred.
    /// Target name is `CrewKit MCP/<server id>`, so entries are legible
    /// in the system Credential Manager UI. The OS caps a generic blob
    /// at 2560 bytes; an oversized save fails and the caller falls back
    /// to the token file, so large sessions keep working.
    #[cfg(windows)]
    mod wincred {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::Security::Credentials::{
            CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
            CRED_TYPE_GENERIC,
        };

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        fn target(server_id: &str) -> Vec<u16> {
            wide(&format!("{}/{}", super::SERVICE, server_id))
        }

        pub fn load(server_id: &str) -> Option<String> {
            let target = target(server_id);
            let mut handle: *mut CREDENTIALW = std::ptr::null_mut();
            // SAFETY: `target` is NUL-terminated; on success `handle`
            // points to an OS-allocated credential freed via CredFree.
            unsafe {
                if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut handle) == 0 {
                    return None;
                }
                let cred = &*handle;
                let blob = std::slice::from_raw_parts(
                    cred.CredentialBlob,
                    cred.CredentialBlobSize as usize,
                );
                let secret = String::from_utf8(blob.to_vec()).ok();
                CredFree(handle.cast());
                secret.filter(|s| !s.is_empty())
            }
        }

        pub fn save(server_id: &str, secret: &str) -> bool {
            let target = target(server_id);
            let mut user = wide("crewkit");
            let blob = secret.as_bytes();
            let credential = CREDENTIALW {
                Flags: 0,
                Type: CRED_TYPE_GENERIC,
                TargetName: target.as_ptr() as *mut u16,
                Comment: std::ptr::null_mut(),
                LastWritten: FILETIME {
                    dwLowDateTime: 0,
                    dwHighDateTime: 0,
                },
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_ptr() as *mut u8,
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                AttributeCount: 0,
                Attributes: std::ptr::null_mut(),
                TargetAlias: std::ptr::null_mut(),
                UserName: user.as_mut_ptr(),
            };
            // SAFETY: every pointer in `credential` outlives this call;
            // CredWriteW copies what it needs before returning.
            unsafe { CredWriteW(&credential, 0) != 0 }
        }

        pub fn delete(server_id: &str) -> bool {
            let target = target(server_id);
            // SAFETY: `target` is a NUL-terminated wide string.
            unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) != 0 }
        }
    }

    #[cfg(windows)]
    fn keychain_load(server_id: &str) -> Option<String> {
        wincred::load(server_id)
    }
    #[cfg(windows)]
    fn keychain_save(server_id: &str, secret: &str) -> bool {
        wincred::save(server_id, secret)
    }
    #[cfg(windows)]
    fn keychain_delete(server_id: &str) -> bool {
        wincred::delete(server_id)
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn keychain_load(_: &str) -> Option<String> {
        None
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    fn keychain_save(_: &str, _: &str) -> bool {
        false
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    fn keychain_delete(_: &str) -> bool {
        false
    }

    /// Load a session, migrating tokens saved in the pre-Keychain token file.
    pub fn load(crewkit_dir: &Path, server_id: &str) -> Option<String> {
        if let Some(secret) = keychain_load(server_id) {
            return Some(secret);
        }
        let legacy = legacy_path(crewkit_dir, server_id);
        let secret = std::fs::read_to_string(&legacy).ok()?;
        if keychain_save(server_id, &secret) {
            let _ = std::fs::remove_file(&legacy);
        }
        Some(secret)
    }

    pub fn save(crewkit_dir: &Path, server_id: &str, secret: &str) -> std::io::Result<()> {
        if keychain_save(server_id, secret) {
            let _ = std::fs::remove_file(legacy_path(crewkit_dir, server_id));
            return Ok(());
        }
        // Non-mac (or Keychain unavailable): 0600 file fallback.
        let path = legacy_path(crewkit_dir, server_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, secret)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Drop a session everywhere; returns whether one existed.
    pub fn delete(crewkit_dir: &Path, server_id: &str) -> bool {
        let in_keychain = keychain_delete(server_id);
        let legacy = legacy_path(crewkit_dir, server_id);
        let in_file = std::fs::remove_file(&legacy).is_ok();
        in_keychain || in_file
    }
}

#[cfg(all(test, windows))]
mod wincred_tests {
    use super::session;

    #[test]
    fn credential_manager_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let id = format!("crewkit-selftest-{}", std::process::id());

        session::save(tmp.path(), &id, "s3cret").unwrap();
        // The secret lives in the Credential Manager, not the file fallback.
        assert!(!tmp.path().join("auth").join(format!("{id}.json")).exists());
        assert_eq!(session::load(tmp.path(), &id).as_deref(), Some("s3cret"));

        // Overwrite in place, then drop it.
        session::save(tmp.path(), &id, "rotated").unwrap();
        assert_eq!(session::load(tmp.path(), &id).as_deref(), Some("rotated"));
        assert!(session::delete(tmp.path(), &id));
        assert_eq!(session::load(tmp.path(), &id), None);
    }
}
