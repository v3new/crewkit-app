//! crewkit-bridge: a local stdio MCP server that proxies to a remote
//! Streamable HTTP MCP server and owns the OAuth session.
//!
//! Every AI client on the machine gets the same tiny config entry —
//! `crewkit-bridge <server-id>` — and they all share one authorization:
//! the bridge runs the OAuth flow (PKCE + dynamic client registration)
//! in the browser once and caches the tokens for all clients.
//!
//! Usage:
//!   crewkit-bridge <server-id>          run the proxy (what clients launch)
//!   crewkit-bridge login <server-id>    run the OAuth flow interactively
//!   crewkit-bridge logout <server-id>   revoke (best-effort) and drop the session
//!   crewkit-bridge status <server-id>   print {"authorized": bool}
//!
//! Server ids resolve to URLs via `<crewkit dir>/servers.json`, written
//! by the CrewKit installer.

mod proxy;

use std::collections::BTreeMap;

/// Product token every outbound HTTP request identifies itself with.
/// The proxy extends it with the connected client's own token once the
/// MCP `initialize` request has declared one.
pub const USER_AGENT: &str = concat!("crewkit-bridge/", env!("CARGO_PKG_VERSION"));

use crewkit_core::auth::AuthSession;
use crewkit_core::paths::Paths;
use serde::Deserialize;

#[derive(Deserialize)]
struct ServersConfig {
    servers: BTreeMap<String, ServerEntry>,
}

#[derive(Deserialize)]
struct ServerEntry {
    url: String,
}

fn resolve_url(paths: &Paths, id: &str) -> Result<String, String> {
    let path = paths.crewkit_dir().join("servers.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let config: ServersConfig =
        serde_json::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    config
        .servers
        .get(id)
        .map(|s| s.url.clone())
        .ok_or_else(|| format!("unknown server id `{id}` in {}", path.display()))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let paths = Paths::from_env();

    let result = match args.as_slice() {
        [id] => resolve_url(&paths, id).and_then(|url| proxy::serve(&paths, id, &url)),
        // An explicit login preempts any background one holding the lock:
        // the user asked for a browser tab now, not for a silent wait.
        [cmd, id] if cmd == "login" => resolve_url(&paths, id).and_then(|url| {
            AuthSession::for_mcp(&paths.crewkit_dir(), id, &url)
                .interactive_login(true)
                .map(|_| eprintln!("crewkit-bridge: authorized `{id}`"))
        }),
        [cmd, id] if cmd == "logout" => resolve_url(&paths, id).and_then(|url| {
            AuthSession::for_mcp(&paths.crewkit_dir(), id, &url)
                .logout()
                .map(|existed| {
                    if existed {
                        eprintln!("crewkit-bridge: logged out of `{id}`");
                    } else {
                        eprintln!("crewkit-bridge: `{id}` had no session");
                    }
                })
        }),
        [cmd, id] if cmd == "status" => resolve_url(&paths, id).map(|url| {
            let authorized = AuthSession::for_mcp(&paths.crewkit_dir(), id, &url).has_tokens();
            println!("{}", serde_json::json!({ "authorized": authorized }));
        }),
        _ => Err("usage: crewkit-bridge [login|logout|status] <server-id>".to_string()),
    };

    if let Err(message) = result {
        eprintln!("crewkit-bridge: {message}");
        std::process::exit(1);
    }
}
