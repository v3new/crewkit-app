use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The spec major version this implementation understands. A manifest
/// declaring a different major is rejected; unknown minor additions are
/// tolerated (unknown fields are ignored by design — that is how the
/// spec evolves within a major version).
const SUPPORTED_SPEC_MAJOR: u32 = 1;

/// A kit is the unit a publisher ships: a named set of plugins and
/// MCP servers installed together into every detected client.
///
/// A kit is either embedded locally (payload zips on disk) or fetched
/// from a signed URL manifest: plugins then carry
/// `artifact` download descriptors instead of local `zip` names.
///
/// Unknown fields are deliberately NOT rejected: per the spec, minor
/// spec revisions add fields that older installers must ignore.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Kit {
    /// Spec version the manifest targets, "MAJOR.MINOR" (e.g. "1.0").
    /// Absent means "1.0" (hand-written manifests may omit it).
    #[serde(default)]
    pub spec: Option<String>,
    pub id: String,
    pub name: String,
    /// Version of the kit manifest itself (not of individual plugins).
    #[serde(default)]
    pub version: Option<String>,
    pub publisher: String,
    /// Publisher's ed25519 public key (base64), pinned on first add.
    #[serde(default)]
    pub publisher_key: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    /// Marketplace name registered in the clients; plugin ids become `<plugin>@<marketplaceName>`.
    pub marketplace_name: String,
    /// Alternate manifest URLs per release channel (e.g. stable/beta),
    /// resolved relative to the manifest's own URL.
    #[serde(default)]
    pub channels: BTreeMap<String, String>,
    /// Install telemetry the publisher collects. Always disclosed to the
    /// user before anything is sent.
    #[serde(default)]
    pub telemetry: Option<Telemetry>,
    /// Optional role bundles: named subsets of the kit to install.
    #[serde(default)]
    pub bundles: Vec<Bundle>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    #[serde(default)]
    pub plugins: Vec<KitPlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Telemetry {
    /// Install reports are POSTed here (kit id/version, item versions, OS).
    pub endpoint: String,
    /// Human-readable page describing exactly what is collected.
    #[serde(default)]
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

/// A downloadable plugin payload with integrity pinning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// Absolute, or relative to the manifest URL.
    pub url: String,
    /// Hex sha256 of the zip.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    pub id: String,
    pub url: String,
    /// Human-facing alias shown in the UI instead of the raw id.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Wire protocol of the endpoint. Spec 1.0 defines only "http"
    /// (MCP Streamable HTTP), the default. A server with a transport
    /// this installer does not support is skipped with a warning —
    /// never a whole-kit failure.
    #[serde(default)]
    pub transport: Option<String>,
    /// How the server authenticates users: "oauth" (default; the
    /// installer runs the client-side OAuth flow) or "none" (open
    /// endpoint, no authorize step is offered).
    #[serde(default)]
    pub auth: Option<String>,
    /// Human documentation page for this server (https).
    #[serde(default)]
    pub docs: Option<String>,
    /// Marked for removal: the installer cleans this server out of every
    /// client (and drops its cached session) instead of installing it.
    /// Lets a publisher retire old servers via a kit update.
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub description: String,
}

impl McpServer {
    pub fn transport(&self) -> &str {
        self.transport.as_deref().unwrap_or("http")
    }

    /// Whether this installer can wire the server into clients.
    pub fn transport_supported(&self) -> bool {
        self.transport() == "http"
    }

    /// False only for explicitly open endpoints (`"auth": "none"`).
    pub fn uses_oauth(&self) -> bool {
        self.auth.as_deref() != Some("none")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitPlugin {
    /// Canonical plugin name (lowercase-hyphen).
    pub name: String,
    /// Zip file name inside the kit's payload directory (embedded kits).
    /// Not needed when the plugin is marked for removal.
    #[serde(default)]
    pub zip: Option<String>,
    /// Download descriptor for URL-manifest kits; the fetcher resolves it
    /// into a local `zip` before installation.
    #[serde(default)]
    pub artifact: Option<Artifact>,
    /// Plugin version the manifest advertises (informational; the
    /// authoritative version lives in the plugin's own manifest).
    #[serde(default)]
    pub version: Option<String>,
    /// Human-facing alias shown in the UI instead of the raw name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Marked for removal: uninstalled from every client instead of installed.
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub description: String,
}

/// The spec requires https for every URL a manifest carries. Loopback
/// origins are exempt so publishers can test against a local server.
fn require_https(url: &str, what: &str) -> Result<()> {
    let loopback = url.starts_with("http://localhost")
        || url.starts_with("http://127.0.0.1")
        || url.starts_with("http://[::1]");
    if url.starts_with("https://") || loopback {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{what} must use https (got `{url}`)"
        )))
    }
}

impl Kit {
    pub fn load(json: &str) -> Result<Kit> {
        let kit: Kit = serde_json::from_str(json).map_err(|e| Error::Parse {
            path: PathBuf::from("kit.json"),
            message: e.to_string(),
        })?;

        // Spec version gate: same major = compatible (unknown fields from
        // newer minors are ignored); different major = reject loudly.
        if let Some(spec) = &kit.spec {
            let major = spec
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok())
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "invalid spec version `{spec}` (expected MAJOR.MINOR)"
                    ))
                })?;
            if major != SUPPORTED_SPEC_MAJOR {
                return Err(Error::Invalid(format!(
                    "manifest targets spec {spec}, but this installer supports \
                     {SUPPORTED_SPEC_MAJOR}.x — update the app to use this kit"
                )));
            }
        }

        for plugin in &kit.plugins {
            if !plugin.remove && plugin.zip.is_none() && plugin.artifact.is_none() {
                return Err(Error::Invalid(format!(
                    "plugin `{}` has neither a zip nor an artifact and is not marked for removal",
                    plugin.name
                )));
            }
            if let Some(artifact) = &plugin.artifact {
                // Relative artifact URLs inherit the (https) manifest origin.
                if artifact.url.contains("://") {
                    require_https(
                        &artifact.url,
                        &format!("artifact of plugin `{}`", plugin.name),
                    )?;
                }
            }
        }
        for server in kit.active_mcp_servers() {
            require_https(&server.url, &format!("MCP server `{}`", server.id))?;
            if let Some(docs) = &server.docs {
                require_https(docs, &format!("docs of MCP server `{}`", server.id))?;
            }
        }
        for (channel, url) in &kit.channels {
            if url.contains("://") {
                require_https(url, &format!("channel `{channel}`"))?;
            }
        }
        if let Some(telemetry) = &kit.telemetry {
            require_https(&telemetry.endpoint, "telemetry endpoint")?;
            if let Some(notice) = &telemetry.notice {
                require_https(notice, "telemetry notice")?;
            }
        }
        Ok(kit)
    }

    pub fn plugin_id(&self, plugin: &KitPlugin) -> String {
        format!("{}@{}", plugin.name, self.marketplace_name)
    }

    /// Items the installer should put in (not marked for removal).
    pub fn active_plugins(&self) -> impl Iterator<Item = &KitPlugin> {
        self.plugins.iter().filter(|p| !p.remove)
    }

    pub fn active_mcp_servers(&self) -> impl Iterator<Item = &McpServer> {
        self.mcp_servers.iter().filter(|s| !s.remove)
    }

    /// Narrow the kit to one role bundle: items outside the bundle are
    /// dropped from the manifest view (they are neither installed nor
    /// touched). Removal-marked items always stay so cleanup still runs.
    pub fn apply_bundle(&mut self, bundle_id: &str) -> Result<()> {
        let bundle = self
            .bundles
            .iter()
            .find(|b| b.id == bundle_id)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("unknown bundle: {bundle_id}")))?;
        self.plugins
            .retain(|p| p.remove || bundle.plugins.contains(&p.name));
        self.mcp_servers
            .retain(|s| s.remove || bundle.mcp_servers.contains(&s.id));
        Ok(())
    }
}
