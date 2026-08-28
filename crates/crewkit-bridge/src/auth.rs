//! OAuth 2.1 for remote MCP servers, owned by CrewKit instead of by each
//! AI client: discovery (RFC 9728 / RFC 8414), dynamic client
//! registration (RFC 7591), authorization-code + PKCE in the system
//! browser, token refresh. Tokens are cached per server id under the
//! CrewKit data directory (0600), so one login serves every client.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use crewkit_core::paths::Paths;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

type Result<T> = std::result::Result<T, String>;

/// Discovery/registration/token calls get a hard timeout — a silent
/// network hang here would freeze a login while it holds the lock.
fn http() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` means the server did not report a lifetime.
    pub expires_at: Option<u64>,
    pub token_endpoint: String,
    pub client_id: String,
    /// The MCP URL, sent as the RFC 8707 `resource` parameter.
    pub resource: String,
}

impl Tokens {
    fn access_is_fresh(&self) -> bool {
        match self.expires_at {
            Some(at) => at > now_unix() + 60,
            None => true,
        }
    }
}

pub struct AuthSession {
    server_id: String,
    mcp_url: String,
    crewkit_dir: PathBuf,
    auth_dir: PathBuf,
}

impl AuthSession {
    pub fn new(paths: &Paths, server_id: &str, mcp_url: &str) -> Self {
        let crewkit_dir = paths.crewkit_dir();
        Self {
            server_id: server_id.to_string(),
            mcp_url: mcp_url.to_string(),
            auth_dir: crewkit_dir.join("auth"),
            crewkit_dir,
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.auth_dir.join(format!("{}.lock", self.server_id))
    }

    pub fn has_tokens(&self) -> bool {
        self.load_tokens().is_some()
    }

    // Tokens live in the platform credential store (macOS Keychain /
    // Windows Credential Manager); see crewkit_core::bridge::session.
    fn load_tokens(&self) -> Option<Tokens> {
        let text = crewkit_core::bridge::session::load(&self.crewkit_dir, &self.server_id)?;
        serde_json::from_str(&text).ok()
    }

    fn save_tokens(&self, tokens: &Tokens) -> Result<()> {
        let json = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
        crewkit_core::bridge::session::save(&self.crewkit_dir, &self.server_id, &json)
            .map_err(|e| e.to_string())
    }

    /// A bearer token ready to use. Refreshes when stale; when there is
    /// no session at all and `interactive` is allowed, runs the browser
    /// flow (deduplicated across processes — several clients starting at
    /// once must produce ONE browser tab, not one each).
    pub fn access_token(&self, interactive: bool) -> Result<String> {
        if let Some(tokens) = self.load_tokens() {
            if tokens.access_is_fresh() {
                return Ok(tokens.access_token);
            }
            if let Ok(refreshed) = self.refresh(&tokens) {
                self.save_tokens(&refreshed)?;
                return Ok(refreshed.access_token);
            }
        }
        if !interactive {
            return Err(format!(
                "not authorized; run `crewkit-bridge login {}`",
                self.server_id
            ));
        }
        self.interactive_login(false).map(|t| t.access_token)
    }

    /// Force-invalidate the access token (after a 401) and get a new one.
    pub fn reauthorize(&self, interactive: bool) -> Result<String> {
        if let Some(mut tokens) = self.load_tokens() {
            tokens.expires_at = Some(0);
            let _ = self.save_tokens(&tokens);
        }
        self.access_token(interactive)
    }

    /// Drop the cached session: best-effort server-side revocation
    /// (RFC 7009, when the server advertises a revocation endpoint),
    /// then delete the local token cache. Returns whether a session existed.
    pub fn logout(&self) -> Result<bool> {
        if let Some(tokens) = self.load_tokens() {
            self.try_revoke(&tokens);
        }
        let _ = std::fs::remove_file(self.lock_path());
        Ok(crewkit_core::bridge::session::delete(
            &self.crewkit_dir,
            &self.server_id,
        ))
    }

    fn try_revoke(&self, tokens: &Tokens) {
        let Ok(issuer) = origin_of(&tokens.token_endpoint) else {
            return;
        };
        let Ok(metadata) = fetch_auth_server_metadata(&issuer) else {
            return;
        };
        let Some(revocation_endpoint) =
            metadata.get("revocation_endpoint").and_then(|v| v.as_str())
        else {
            return;
        };
        let mut targets = vec![tokens.access_token.clone()];
        if let Some(refresh) = &tokens.refresh_token {
            targets.push(refresh.clone());
        }
        for token in targets {
            let _ = http()
                .post(revocation_endpoint)
                .send_form(&[("token", &token), ("client_id", &tokens.client_id)]);
        }
    }

    /// `preempt` distinguishes an explicit login (the `login` command —
    /// UI button or CLI) from a background one (a proxy serving a client).
    /// An explicit login must always reach the browser: if a background
    /// login already holds the lock (its tab lost or ignored), take the
    /// lock over instead of waiting on it. Background logins deduplicate:
    /// they wait for whichever flow the user completes.
    pub fn interactive_login(&self, preempt: bool) -> Result<Tokens> {
        std::fs::create_dir_all(&self.auth_dir).map_err(|e| e.to_string())?;
        match FileLock::acquire(self.lock_path()) {
            Some(_lock) => self.run_browser_flow(),
            None if preempt => {
                eprintln!(
                    "crewkit-bridge: a login for `{}` is already in progress elsewhere — taking over",
                    self.server_id
                );
                let _lock = FileLock::steal(self.lock_path());
                self.run_browser_flow()
            }
            // Another process is already showing the browser tab — wait
            // for the tokens it produces instead of opening a second one.
            None => self.wait_for_other_login(),
        }
    }

    fn wait_for_other_login(&self) -> Result<Tokens> {
        eprintln!(
            "crewkit-bridge: a login for `{}` is already in progress in another process — waiting for it",
            self.server_id
        );
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(tokens) = self.load_tokens() {
                if tokens.access_is_fresh() {
                    return Ok(tokens);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err("timed out waiting for the login running in another process".into())
    }

    fn run_browser_flow(&self) -> Result<Tokens> {
        let endpoints = self.discover()?;

        // Bind the callback listener first so the exact redirect URI is
        // known before the client is registered.
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("cannot bind callback: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let client_id = self.register_client(&endpoints, &redirect_uri)?;

        let verifier = random_b64url(48);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_b64url(16);

        let mut auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&resource={}",
            endpoints.authorization_endpoint,
            urlencode(&client_id),
            urlencode(&redirect_uri),
            urlencode(&state),
            urlencode(&challenge),
            urlencode(&self.mcp_url),
        );
        if let Some(scope) = &endpoints.scope {
            auth_url.push_str(&format!("&scope={}", urlencode(scope)));
        }

        eprintln!(
            "crewkit-bridge: authorizing `{}` — opening the browser…",
            self.server_id
        );
        // A concurrent login (e.g. a preempting explicit one) may finish
        // while this flow waits for its own tab; comparing against the
        // token present now lets the wait recognize that and yield.
        let previous_token = self.load_tokens().map(|t| t.access_token);
        open_browser(&auth_url);

        let code = match self.wait_for_callback(&listener, &state, previous_token.as_deref())? {
            Callback::Code(code) => code,
            Callback::OtherLoginWon(tokens) => return Ok(tokens),
        };

        let response = http()
            .post(&endpoints.token_endpoint)
            .send_form(&[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect_uri),
                ("client_id", &client_id),
                ("code_verifier", &verifier),
                ("resource", &self.mcp_url),
            ])
            .map_err(|e| format!("token exchange failed: {e}"))?;
        let body: Value = response
            .into_json()
            .map_err(|e| format!("token exchange returned invalid JSON: {e}"))?;

        let tokens = self.tokens_from_response(&body, &endpoints.token_endpoint, &client_id)?;
        self.save_tokens(&tokens)?;
        Ok(tokens)
    }

    fn refresh(&self, tokens: &Tokens) -> Result<Tokens> {
        let refresh_token = tokens
            .refresh_token
            .as_deref()
            .ok_or("no refresh token")?
            .to_string();
        let response = http()
            .post(&tokens.token_endpoint)
            .send_form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", &tokens.client_id),
                ("resource", &tokens.resource),
            ])
            .map_err(|e| format!("refresh failed: {e}"))?;
        let body: Value = response.into_json().map_err(|e| e.to_string())?;
        let mut refreshed =
            self.tokens_from_response(&body, &tokens.token_endpoint, &tokens.client_id)?;
        // Servers may omit the refresh token on rotation — keep the old one.
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some(refresh_token);
        }
        Ok(refreshed)
    }

    fn tokens_from_response(
        &self,
        body: &Value,
        token_endpoint: &str,
        client_id: &str,
    ) -> Result<Tokens> {
        let access_token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("no access_token in token response: {body}"))?
            .to_string();
        Ok(Tokens {
            access_token,
            refresh_token: body
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .map(String::from),
            expires_at: body
                .get("expires_in")
                .and_then(|v| v.as_u64())
                .map(|s| now_unix() + s.saturating_sub(30)),
            token_endpoint: token_endpoint.to_string(),
            client_id: client_id.to_string(),
            resource: self.mcp_url.clone(),
        })
    }

    // --- Discovery ---

    fn discover(&self) -> Result<AuthEndpoints> {
        let resource_metadata = self.fetch_resource_metadata();
        let (issuer, scope) = match &resource_metadata {
            Some(metadata) => {
                let issuer = metadata
                    .get("authorization_servers")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .ok_or("resource metadata has no authorization_servers")?
                    .trim_end_matches('/')
                    .to_string();
                let scope = metadata
                    .get("scopes_supported")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .filter(|s| !s.is_empty());
                (issuer, scope)
            }
            // Pre-RFC9728 servers: the MCP origin acts as the issuer.
            None => (origin_of(&self.mcp_url)?, None),
        };

        let metadata = fetch_auth_server_metadata(&issuer)?;
        Ok(AuthEndpoints {
            authorization_endpoint: metadata
                .get("authorization_endpoint")
                .and_then(|v| v.as_str())
                .ok_or("no authorization_endpoint in auth server metadata")?
                .to_string(),
            token_endpoint: metadata
                .get("token_endpoint")
                .and_then(|v| v.as_str())
                .ok_or("no token_endpoint in auth server metadata")?
                .to_string(),
            registration_endpoint: metadata
                .get("registration_endpoint")
                .and_then(|v| v.as_str())
                .map(String::from),
            scope,
        })
    }

    /// RFC 9728: prefer the URL the server advertises in WWW-Authenticate,
    /// then fall back to the well-known locations.
    fn fetch_resource_metadata(&self) -> Option<Value> {
        if let Some(url) = self.probe_www_authenticate() {
            if let Some(value) = get_json(&url) {
                return Some(value);
            }
        }
        let origin = origin_of(&self.mcp_url).ok()?;
        let path = self.mcp_url.strip_prefix(&origin).unwrap_or("");
        for candidate in [
            format!("{origin}/.well-known/oauth-protected-resource{path}"),
            format!("{origin}/.well-known/oauth-protected-resource"),
        ] {
            if let Some(value) = get_json(&candidate) {
                return Some(value);
            }
        }
        None
    }

    fn probe_www_authenticate(&self) -> Option<String> {
        let response = http()
            .post(&self.mcp_url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .send_string(r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#);
        let header = match response {
            Err(ureq::Error::Status(401, resp)) => resp.header("WWW-Authenticate")?.to_string(),
            _ => return None,
        };
        // e.g. Bearer resource_metadata="https://…"
        let marker = "resource_metadata=\"";
        let start = header.find(marker)? + marker.len();
        let end = header[start..].find('"')? + start;
        Some(header[start..end].to_string())
    }

    /// Serve the OAuth redirect: accept connections until the /callback
    /// request with a matching state arrives, then hand back the code.
    /// Also watches the token cache — if a concurrent login for the same
    /// server completes first (its tokens differ from `previous_token`),
    /// this flow yields to it instead of waiting out its own tab.
    fn wait_for_callback(
        &self,
        listener: &TcpListener,
        expected_state: &str,
        previous_token: Option<&str>,
    ) -> Result<Callback> {
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        let mut polls: u32 = 0;
        while Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(conn) => conn,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    polls += 1;
                    // Token reads shell out to the Keychain — check every
                    // ~2s, not on every 100ms accept poll.
                    if polls.is_multiple_of(20) {
                        if let Some(tokens) = self.load_tokens() {
                            if tokens.access_is_fresh()
                                && previous_token != Some(tokens.access_token.as_str())
                            {
                                eprintln!(
                                    "crewkit-bridge: `{}` was authorized by another login — done",
                                    self.server_id
                                );
                                return Ok(Callback::OtherLoginWon(tokens));
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(format!("callback listener failed: {e}")),
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let target = request.split_whitespace().nth(1).unwrap_or("");

            if let Some(query) = target.strip_prefix("/callback?") {
                let get = |key: &str| {
                    query.split('&').find_map(|pair| {
                        pair.strip_prefix(&format!("{key}="))
                            .map(|v| urldecode(v.split('#').next().unwrap_or(v)))
                    })
                };
                if get("state").as_deref() != Some(expected_state) {
                    respond(
                        &mut stream,
                        400,
                        "State mismatch — close this tab and retry.",
                    );
                    continue;
                }
                match get("code") {
                    Some(code) => {
                        respond(
                            &mut stream,
                            200,
                            "You can close this tab and return to your AI client.",
                        );
                        return Ok(Callback::Code(code));
                    }
                    None => {
                        let error = get("error").unwrap_or_else(|| "unknown error".into());
                        respond(&mut stream, 400, &error);
                        return Err(format!("authorization failed: {error}"));
                    }
                }
            }
            respond(&mut stream, 404, "This page does not exist.");
        }
        Err("timed out waiting for the browser authorization".into())
    }

    fn register_client(&self, endpoints: &AuthEndpoints, redirect_uri: &str) -> Result<String> {
        let registration_endpoint = endpoints
            .registration_endpoint
            .as_deref()
            .ok_or("server does not support dynamic client registration")?;
        let mut registration = serde_json::json!({
            "client_name": "CrewKit",
            "client_uri": "https://github.com/v3new/crewkit-app",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        // The scopes requested later must be granted to the client at
        // registration time, or the auth request fails with invalid_scope.
        if let Some(scope) = &endpoints.scope {
            registration["scope"] = serde_json::json!(scope);
        }
        let response = http()
            .post(registration_endpoint)
            .send_json(registration)
            .map_err(|e| format!("client registration failed: {e}"))?;
        let body: Value = response.into_json().map_err(|e| e.to_string())?;
        body.get("client_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| format!("no client_id in registration response: {body}"))
    }
}

struct AuthEndpoints {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scope: Option<String>,
}

enum Callback {
    /// The browser redirect delivered an authorization code.
    Code(String),
    /// A concurrent login for the same server finished first.
    OtherLoginWon(Tokens),
}

fn fetch_auth_server_metadata(issuer: &str) -> Result<Value> {
    let origin = origin_of(issuer)?;
    let path = issuer.strip_prefix(&origin).unwrap_or("");
    let candidates = [
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{issuer}/.well-known/oauth-authorization-server"),
        format!("{origin}/.well-known/openid-configuration{path}"),
        format!("{issuer}/.well-known/openid-configuration"),
    ];
    for candidate in &candidates {
        if let Some(value) = get_json(candidate) {
            return Ok(value);
        }
    }
    Err(format!(
        "no OAuth authorization server metadata found for {issuer}"
    ))
}

fn get_json(url: &str) -> Option<Value> {
    http()
        .get(url)
        .set("Accept", "application/json")
        .call()
        .ok()?
        .into_json()
        .ok()
}

/// The page shown in the browser tab after the OAuth redirect, in the
/// crewkit-landing design language (paper/ink palette, Archivo + IBM
/// Plex Mono, hard-shadow card). Self-contained except the Google Fonts
/// stylesheet, which degrades to the system stacks offline.
fn respond(stream: &mut std::net::TcpStream, status: u16, message: &str) {
    let reason = if status == 200 { "OK" } else { "Error" };
    let (mark_class, mark, title) = match status {
        200 => ("ok", "✓", "Authorized"),
        404 => ("dim", "?", "Not found"),
        _ => ("err", "!", "Authorization failed"),
    };
    let message = html_escape(message);
    let body = format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CrewKit — {title}</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Archivo:wdth,wght@75..125,400..900&family=IBM+Plex+Mono:wght@400;500;600&display=swap" rel="stylesheet">
<style>
  :root{{
    --paper:#EDECE4; --ink:#171F1A; --muted:#5B6259; --line:#C9CBBE;
    --card:#FFFFFF; --signal:#F2B705; --ok:#1E7A4C; --err:#A3352B;
    --mono:'IBM Plex Mono',ui-monospace,monospace;
    --sans:'Archivo',system-ui,sans-serif;
  }}
  *{{margin:0;padding:0;box-sizing:border-box}}
  body{{background:var(--paper);color:var(--ink);font-family:var(--sans);min-height:100vh;display:grid;place-items:center;padding:24px;-webkit-font-smoothing:antialiased}}
  .card{{background:var(--card);border:2px solid var(--ink);border-radius:14px;box-shadow:6px 6px 0 rgba(23,31,26,.12);transform:rotate(.6deg);max-width:27em;width:100%;padding:34px 38px 0}}
  .eyebrow{{font-family:var(--mono);font-size:12.5px;letter-spacing:.14em;text-transform:uppercase;color:var(--muted);margin-bottom:22px}}
  .mark{{width:46px;height:46px;border-radius:10px;display:grid;place-items:center;font-size:22px;font-weight:900;color:#fff;margin-bottom:18px;transform:rotate(-4deg)}}
  .mark.ok{{background:var(--ok)}}
  .mark.err{{background:var(--err)}}
  .mark.dim{{background:var(--muted)}}
  h1{{font-size:28px;font-weight:800;letter-spacing:-.01em;line-height:1.2;margin-bottom:10px}}
  h1 .hl{{background:var(--signal);padding:0 .12em;box-decoration-break:clone;-webkit-box-decoration-break:clone}}
  p{{color:var(--muted);font-size:16px;line-height:1.55;overflow-wrap:break-word}}
  .foot{{margin:28px -38px 0;padding:13px 38px;background:var(--paper);border-top:2px solid var(--ink);border-radius:0 0 12px 12px;font-family:var(--mono);font-size:12px;letter-spacing:.05em;color:var(--muted);display:flex;justify-content:space-between;gap:8px}}
  .foot .status{{color:var(--ok);font-weight:600}}
</style>
</head>
<body>
<main class="card">
  <p class="eyebrow">CrewKit</p>
  <div class="mark {mark_class}">{mark}</div>
  <h1><span class="hl">{title}</span></h1>
  <p>{message}</p>
  <div class="foot"><span>crewkit-bridge</span><span class="status">one login · every client</span></div>
</main>
</body>
</html>"##
    );
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

/// The failure message can carry text from the redirect query string —
/// escape it so the callback page cannot be used to inject markup.
fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Cross-process login lock: the holder runs the browser flow, everyone
/// else waits for the tokens file. Stale locks (crashed process) expire.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    fn acquire(path: PathBuf) -> Option<Self> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                // Record the holder so a crashed process's lock can be
                // recognized as stale immediately, not after a timeout.
                let _ = write!(file, "{}", std::process::id());
                Some(Self { path })
            }
            Err(_) => {
                let holder_alive = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .map(process_alive)
                    .unwrap_or(false);
                let expired = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > LOGIN_TIMEOUT)
                    .unwrap_or(true);
                if !holder_alive || expired {
                    let _ = std::fs::remove_file(&path);
                    return Self::acquire(path);
                }
                None
            }
        }
    }

    /// Take the lock over from a live holder — for explicit logins, which
    /// must always reach the browser. The previous holder keeps waiting on
    /// its own callback listener and resolves from whichever flow the
    /// user completes; its guarded `Drop` leaves this lock alone.
    fn steal(path: PathBuf) -> Self {
        let _ = std::fs::remove_file(&path);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
        {
            let _ = write!(file, "{}", std::process::id());
        }
        Self { path }
    }
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    // tasklist prints a table row for a live pid and an info message
    // otherwise; matching the pid in the output separates the two.
    crewkit_core::cli::command("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Remove the file only while it is still ours: a preempting login
        // may have replaced it, and that lock must survive our exit.
        let mine = std::fs::read_to_string(&self.path)
            .map(|s| s.trim() == std::process::id().to_string())
            .unwrap_or(false);
        if mine {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// --- Small helpers ---

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_b64url(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn origin_of(url: &str) -> Result<String> {
    let scheme_end = url
        .find("://")
        .ok_or_else(|| format!("invalid URL: {url}"))?;
    let rest = &url[scheme_end + 3..];
    let host_end = rest.find('/').unwrap_or(rest.len());
    Ok(url[..scheme_end + 3 + host_end].to_string())
}

fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("/usr/bin/open", &[]);
    // rundll32 hands the URL to the default browser without going through
    // cmd.exe, whose argument parsing mangles `&` in query strings.
    #[cfg(windows)]
    let (program, args): (&str, &[&str]) = ("rundll32", &["url.dll,FileProtocolHandler"]);
    #[cfg(not(any(target_os = "macos", windows)))]
    let (program, args): (&str, &[&str]) = ("xdg-open", &[]);
    if crewkit_core::cli::command(program)
        .args(args)
        .arg(url)
        .spawn()
        .is_err()
    {
        eprintln!("crewkit-bridge: could not open a browser; open this URL manually:\n{url}");
    }
}
