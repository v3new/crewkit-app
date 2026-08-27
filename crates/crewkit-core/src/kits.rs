//! Remote kits: signed URL manifests, release channels, artifact
//! downloads, and the registry of kits added to this machine.
//!
//! A published kit is a JSON manifest at a URL plus a detached ed25519
//! signature at `<url>.sig` over the manifest's exact bytes. The
//! publisher's public key travels inside the manifest and is pinned on
//! first add (trust on first use): a later manifest signed with a
//! different key is rejected, so a compromised CDN cannot substitute a
//! different publisher.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::fsops;
use crate::kit::Kit;

fn http() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
}

// --- Registry ---

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KitRegistry {
    pub kits: Vec<KitSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitSource {
    pub id: String,
    /// `"builtin"` for the embedded kit, otherwise the manifest URL.
    pub source: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Publisher key pinned at first add (base64 ed25519).
    #[serde(default)]
    pub pinned_key: Option<String>,
    /// Chosen role bundle; `None` installs the whole kit.
    #[serde(default)]
    pub bundle: Option<String>,
}

fn default_channel() -> String {
    "stable".into()
}

impl KitRegistry {
    fn path(crewkit_dir: &Path) -> PathBuf {
        crewkit_dir.join("kits.json")
    }

    pub fn load(crewkit_dir: &Path) -> Result<Self> {
        match fsops::read_json(&Self::path(crewkit_dir))? {
            Some(value) => Ok(serde_json::from_value(value).unwrap_or_default()),
            None => Ok(Self::default()),
        }
    }

    pub fn save(&self, crewkit_dir: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).expect("registry serializes");
        fsops::atomic_write(&Self::path(crewkit_dir), json.as_bytes())
    }
}

// --- Fetch & verify ---

#[derive(Debug)]
pub struct FetchedKit {
    pub kit: Kit,
    /// Local directory the downloaded artifacts landed in; every plugin's
    /// `zip` field has been rewritten to point inside it.
    pub zips_dir: PathBuf,
    pub manifest_url: String,
}

/// Fetch, verify and materialize a kit from a signed URL manifest.
pub fn fetch_kit(url: &str, pinned_key: Option<&str>, crewkit_dir: &Path) -> Result<FetchedKit> {
    // The spec mandates https end to end (loopback allowed for dev).
    if !(url.starts_with("https://")
        || url.starts_with("http://localhost")
        || url.starts_with("http://127.0.0.1")
        || url.starts_with("http://[::1]"))
    {
        return Err(Error::Invalid(format!(
            "kit manifest URL must use https (got `{url}`)"
        )));
    }
    let manifest_bytes = get_bytes(url)?;
    let signature = get_string(&format!("{url}.sig"))
        .map_err(|e| Error::Invalid(format!("kit manifest has no detached signature: {e}")))?;

    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .map_err(|_| Error::Invalid(format!("{url}: manifest is not UTF-8")))?;
    let mut kit = Kit::load(manifest_text)?;

    let publisher_key = kit
        .publisher_key
        .clone()
        .ok_or_else(|| Error::Invalid(format!("{url}: manifest has no publisherKey")))?;
    if let Some(pinned) = pinned_key {
        if pinned != publisher_key {
            return Err(Error::Invalid(
                "publisher key changed since this kit was added — refusing to update \
                 (possible CDN compromise; remove and re-add the kit to trust the new key)"
                    .into(),
            ));
        }
    }
    verify_manifest(&publisher_key, &manifest_bytes, signature.trim())?;

    // Download artifacts (integrity-pinned by sha256, cached by digest).
    let cache = crewkit_dir.join("artifacts").join(&kit.id);
    for plugin in kit.plugins.iter_mut().filter(|p| !p.remove) {
        let Some(artifact) = plugin.artifact.clone() else {
            continue;
        };
        let digest_prefix: String = artifact.sha256.chars().take(12).collect();
        let file_name = format!("{}-{digest_prefix}.zip", plugin.name);
        let dest = cache.join(&file_name);
        let cached_ok = std::fs::read(&dest)
            .map(|bytes| sha256_hex(&bytes) == artifact.sha256.to_lowercase())
            .unwrap_or(false);
        if !cached_ok {
            let bytes = get_bytes(&resolve_url(url, &artifact.url))?;
            let digest = sha256_hex(&bytes);
            if digest != artifact.sha256.to_lowercase() {
                return Err(Error::Invalid(format!(
                    "artifact for `{}` failed integrity check (expected sha256 {}, got {digest})",
                    plugin.name, artifact.sha256
                )));
            }
            fsops::atomic_write(&dest, &bytes)?;
        }
        plugin.zip = Some(file_name);
    }

    Ok(FetchedKit {
        kit,
        zips_dir: cache,
        manifest_url: url.to_string(),
    })
}

/// Resolve a possibly-relative artifact/channel URL against the manifest URL.
pub fn resolve_url(base: &str, reference: &str) -> String {
    if reference.starts_with("http://") || reference.starts_with("https://") {
        return reference.to_string();
    }
    if let Some(scheme_end) = base.find("://") {
        let origin_end = base[scheme_end + 3..]
            .find('/')
            .map(|i| scheme_end + 3 + i)
            .unwrap_or(base.len());
        if let Some(rooted) = reference.strip_prefix('/') {
            return format!("{}/{}", &base[..origin_end], rooted);
        }
    }
    let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base);
    format!("{dir}/{}", reference.trim_start_matches("./"))
}

fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let response = http()
        .get(url)
        .call()
        .map_err(|e| Error::Invalid(format!("GET {url} failed: {e}")))?;
    let mut bytes = Vec::new();
    use std::io::Read;
    response
        .into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Invalid(format!("reading {url} failed: {e}")))?;
    Ok(bytes)
}

fn get_string(url: &str) -> Result<String> {
    let bytes = get_bytes(url)?;
    String::from_utf8(bytes).map_err(|_| Error::Invalid(format!("{url}: not UTF-8")))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// --- Signing (publisher side; used by `crewkit kit keygen/sign`) ---

/// Generate an ed25519 keypair; returns (secret, public) as base64.
pub fn generate_keypair() -> (String, String) {
    let signing = SigningKey::generate(&mut rand::rngs::OsRng);
    (
        B64.encode(signing.to_bytes()),
        B64.encode(signing.verifying_key().to_bytes()),
    )
}

pub fn sign_manifest(manifest: &[u8], secret_b64: &str) -> Result<String> {
    let bytes = B64
        .decode(secret_b64.trim())
        .map_err(|_| Error::Invalid("invalid secret key encoding".into()))?;
    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::Invalid("secret key must be 32 bytes".into()))?;
    let signing = SigningKey::from_bytes(&key_bytes);
    Ok(B64.encode(signing.sign(manifest).to_bytes()))
}

pub fn verify_manifest(public_b64: &str, manifest: &[u8], signature_b64: &str) -> Result<()> {
    let key_bytes: [u8; 32] = B64
        .decode(public_b64.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Invalid("invalid publisher key encoding".into()))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| Error::Invalid("invalid publisher key".into()))?;
    let sig_bytes: [u8; 64] = B64
        .decode(signature_b64)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Invalid("invalid signature encoding".into()))?;
    key.verify(manifest, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| Error::Invalid("manifest signature verification FAILED".into()))
}

// --- Telemetry (disclosed per-kit install reporting) ---

/// A stable anonymous id for install reports, generated once per machine.
pub fn anonymous_id(crewkit_dir: &Path) -> String {
    let path = crewkit_dir.join("telemetry-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }
    let mut bytes = [0u8; 16];
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = fsops::atomic_write(&path, id.as_bytes());
    id
}

/// Fire-and-forget install report; the UI disclosed the endpoint before
/// install. Never blocks or fails the installation.
pub fn send_install_report(kit: &Kit, crewkit_dir: &Path, report: serde_json::Value) {
    let Some(telemetry) = kit.telemetry.clone() else {
        return;
    };
    let mut payload = report;
    payload["anonymousId"] = serde_json::json!(anonymous_id(crewkit_dir));
    payload["kitId"] = serde_json::json!(kit.id);
    payload["kitVersion"] = serde_json::json!(kit.version);
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        let _ = agent.post(&telemetry.endpoint).send_json(payload);
    });
}
