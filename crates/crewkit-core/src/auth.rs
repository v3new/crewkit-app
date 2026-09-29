//! OAuth 2.1 for the remote resources CrewKit talks to — MCP servers and
//! kits published behind a login — owned by CrewKit instead of by each AI
//! client: discovery (RFC 9728 / RFC 8414), dynamic client registration
//! (RFC 7591), authorization-code + PKCE in the system browser, token
//! refresh. Tokens are cached per session id in the platform credential
//! store, so one login serves every client.
//!
//! Two kinds of caller, one flow:
//! - the bridge authorizes an MCP server, and discovers the protected
//!   resource by pinging it (`AuthSession::for_mcp`);
//! - `kits::fetch_kit` authorizes a kit, and already holds the
//!   `WWW-Authenticate` challenge from the 401 that sent it here
//!   (`AuthSession::for_resource`).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::lock::FileLock;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// A renewal is two HTTP calls; a lock older than this belongs to a
/// process that died holding it.
const RENEWAL_TIMEOUT: Duration = Duration::from_secs(45);

/// Callback ports tried in order. A stable redirect URI is what lets one
/// registered OAuth client serve every later login, so the user is not
/// sent through a consent screen each time.
const CALLBACK_PORTS: [u16; 8] = [33418, 33419, 33420, 33421, 33422, 33423, 33424, 33425];

/// Product token every outbound authorization request identifies itself
/// with; the proxy keeps its own for the MCP traffic it forwards.
const USER_AGENT: &str = concat!("crewkit/", env!("CARGO_PKG_VERSION"));

type Result<T> = std::result::Result<T, String>;

/// What a stored session can answer with: the tokens, or why not.
type SessionResult<T> = std::result::Result<T, SessionError>;

/// Why a stored session could not produce a token. Only `Rejected` means
/// the session is really over and the user has to sign in again.
enum SessionError {
    Missing,
    Rejected(String),
    /// Network, DNS, TLS, a 5xx. Clients start with the machine, often
    /// before the network is up, and that must not cost a browser tab.
    Unavailable(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Missing => write!(f, "no session stored"),
            SessionError::Rejected(why) => write!(f, "the server refused the session: {why}"),
            SessionError::Unavailable(why) => write!(f, "authorization server unreachable: {why}"),
        }
    }
}

/// A refused grant ends the session; anything else is the network or the
/// server having a moment.
fn classify(error: ureq::Error) -> SessionError {
    match error {
        ureq::Error::Status(code, response) if code < 500 && code != 429 => {
            let detail = response.into_string().unwrap_or_default();
            let detail: String = detail.chars().take(200).collect();
            SessionError::Rejected(format!("HTTP {code}: {detail}"))
        }
        other => SessionError::Unavailable(other.to_string()),
    }
}

/// An OAuth client registered once and reused for every later login.
#[derive(Clone, Serialize, Deserialize)]
struct RegisteredClient {
    client_id: String,
    registration_endpoint: String,
    redirect_uris: Vec<String>,
}

/// Discovery/registration/token calls get a hard timeout — a silent
/// network hang here would freeze a login while it holds the lock.
fn http() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        // Auth is one shared session for all clients, so these requests
        // carry CrewKit's own token, never a client's.
        .user_agent(USER_AGENT)
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

/// Whether a stored session can renew itself. Without a refresh token the
/// server will ask the user to sign in again once this one runs out.
pub fn session_renews(stored: &str) -> bool {
    serde_json::from_str::<Tokens>(stored).is_ok_and(|tokens| tokens.refresh_token.is_some())
}

pub struct AuthSession {
    session_id: String,
    /// What we authorize against: the MCP endpoint, or — for a kit — the
    /// URL that answered 401. Discovery replaces it with the canonical
    /// `resource` the protected-resource metadata declares.
    resource: String,
    /// Whether discovery may ping the URL with an MCP `ping` to read its
    /// `WWW-Authenticate`. Only true for MCP endpoints: a kit's challenge
    /// already came with the 401 that sent us here.
    probe_mcp: bool,
    challenge: Option<String>,
    crewkit_dir: PathBuf,
    auth_dir: PathBuf,
}

impl AuthSession {
    /// A session for a remote MCP server, keyed by its server id.
    pub fn for_mcp(crewkit_dir: &Path, server_id: &str, mcp_url: &str) -> Self {
        Self::build(crewkit_dir, server_id, mcp_url, true)
    }

    /// A session for any other protected resource — today a kit behind a
    /// login. `challenge` is the `WWW-Authenticate` header of the 401 that
    /// made the caller authorize, so discovery needs no probe of its own.
    pub fn for_resource(
        crewkit_dir: &Path,
        session_id: &str,
        url: &str,
        challenge: Option<String>,
    ) -> Self {
        let mut session = Self::build(crewkit_dir, session_id, url, false);
        session.challenge = challenge;
        session
    }

    fn build(crewkit_dir: &Path, session_id: &str, url: &str, probe_mcp: bool) -> Self {
        let crewkit_dir = crewkit_dir.to_path_buf();
        Self {
            session_id: session_id.to_string(),
            resource: url.to_string(),
            probe_mcp,
            challenge: None,
            auth_dir: crewkit_dir.join("auth"),
            crewkit_dir,
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.auth_dir.join(format!("{}.lock", self.session_id))
    }

    fn renewal_lock_path(&self) -> PathBuf {
        self.auth_dir
            .join(format!("{}.renewal.lock", self.session_id))
    }

    /// Machine-wide: one login tab is open at a time, whichever server
    /// asked for it.
    fn browser_lock_path(&self) -> PathBuf {
        self.auth_dir.join("browser.lock")
    }

    fn clients_path(&self) -> PathBuf {
        self.auth_dir.join("clients.json")
    }

    pub fn has_tokens(&self) -> bool {
        self.load_tokens().is_some()
    }

    // Tokens live in the platform credential store (macOS Keychain /
    // Windows Credential Manager); see crate::bridge::session.
    fn load_tokens(&self) -> Option<Tokens> {
        let text = crate::bridge::session::load(&self.crewkit_dir, &self.session_id)?;
        serde_json::from_str(&text).ok()
    }

    fn save_tokens(&self, tokens: &Tokens) -> Result<()> {
        let json = serde_json::to_string(tokens).map_err(|e| e.to_string())?;
        crate::bridge::session::save(&self.crewkit_dir, &self.session_id, &json)
            .map_err(|e| e.to_string())
    }

    /// A bearer token ready to use. Renews when stale; when the session
    /// is over and `interactive` is allowed, runs the browser flow
    /// (deduplicated across processes — several clients starting at once
    /// must produce ONE browser tab, not one each).
    pub fn access_token(&self, interactive: bool) -> Result<String> {
        match self.usable_tokens() {
            Ok(tokens) => Ok(tokens.access_token),
            // A server we cannot reach has not logged anyone out: a
            // browser tab would not help and nobody asked for one.
            Err(SessionError::Unavailable(why)) => Err(why),
            Err(_) if interactive => self.interactive_login(false).map(|t| t.access_token),
            Err(_) => Err(self.sign_in_first()),
        }
    }

    /// A bearer token from the cache, renewed when stale — never a
    /// browser tab. `None` means the caller has to ask the user to sign in.
    pub fn silent_access_token(&self) -> Option<String> {
        self.usable_tokens().ok().map(|t| t.access_token)
    }

    /// The stored session, renewed when its access token has gone stale.
    fn usable_tokens(&self) -> SessionResult<Tokens> {
        let tokens = self.load_tokens().ok_or(SessionError::Missing)?;
        if tokens.access_is_fresh() {
            return Ok(tokens);
        }
        self.renew(&tokens)
    }

    /// Replace an access token the server rejected before it expired on
    /// paper. Nothing is written until the replacement is in hand — a
    /// failed attempt must not spoil a session that still works.
    pub fn reauthorize(&self, interactive: bool) -> Result<String> {
        if let Some(tokens) = self.load_tokens() {
            match self.renew(&tokens) {
                Ok(renewed) => return Ok(renewed.access_token),
                Err(SessionError::Unavailable(why)) => return Err(why),
                Err(_) => {}
            }
        }
        if !interactive {
            return Err(self.sign_in_first());
        }
        self.interactive_login(false).map(|t| t.access_token)
    }

    fn sign_in_first(&self) -> String {
        format!("not authorized for `{}` — sign in first", self.session_id)
    }

    /// Drop the cached session: best-effort server-side revocation
    /// (RFC 7009, when the server advertises a revocation endpoint),
    /// then delete the local token cache. Returns whether a session existed.
    pub fn logout(&self) -> Result<bool> {
        if let Some(tokens) = self.load_tokens() {
            self.try_revoke(&tokens);
        }
        let _ = std::fs::remove_file(self.lock_path());
        self.forget_client();
        Ok(crate::bridge::session::delete(
            &self.crewkit_dir,
            &self.session_id,
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
        let _login = match FileLock::acquire(self.lock_path(), LOGIN_TIMEOUT) {
            Some(lock) => lock,
            None if preempt => {
                eprintln!(
                    "crewkit: a login for `{}` is already in progress elsewhere — taking over",
                    self.session_id
                );
                FileLock::steal(self.lock_path())
            }
            // Another process is already showing the browser tab — wait
            // for the tokens it produces instead of opening a second one.
            None => return self.wait_for_other_login(),
        };
        match self.wait_for_browser_turn(preempt)? {
            Turn::Ours(_turn) => self.run_browser_flow(),
            Turn::AlreadyDone(tokens) => Ok(tokens),
        }
    }

    /// Sessions that were created together end together, so servers queue
    /// for the one tab the machine shows at a time instead of flooding
    /// the screen. An explicit login jumps the queue — the user asked for
    /// a tab now.
    fn wait_for_browser_turn(&self, preempt: bool) -> Result<Turn> {
        let path = self.browser_lock_path();
        if let Some(turn) = FileLock::acquire(path.clone(), LOGIN_TIMEOUT) {
            return Ok(Turn::Ours(turn));
        }
        if preempt {
            return Ok(Turn::Ours(FileLock::steal(path)));
        }
        eprintln!(
            "crewkit: another sign-in is open — `{}` waits its turn",
            self.session_id
        );
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            if let Some(tokens) = self.load_tokens().filter(Tokens::access_is_fresh) {
                return Ok(Turn::AlreadyDone(tokens));
            }
            if let Some(turn) = FileLock::acquire(path.clone(), LOGIN_TIMEOUT) {
                return Ok(Turn::Ours(turn));
            }
        }
        Err("timed out waiting for another sign-in to finish".into())
    }

    fn wait_for_other_login(&self) -> Result<Tokens> {
        eprintln!(
            "crewkit: a login for `{}` is already in progress in another process — waiting for it",
            self.session_id
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
        // The token is bound to the resource the server declares, which is
        // not necessarily the URL we requested (a kit manifest sits under
        // its resource root).
        let resource = endpoints
            .resource
            .clone()
            .unwrap_or_else(|| self.resource.clone());

        // Bind the callback listener first so the exact redirect URI is
        // known before the client is registered.
        let listener = bind_callback()?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let client_id = self.client_id(&endpoints, &redirect_uri)?;

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
            urlencode(&resource),
        );
        if let Some(scope) = &endpoints.scope {
            auth_url.push_str(&format!("&scope={}", urlencode(scope)));
        }

        eprintln!(
            "crewkit: authorizing `{}` — opening the browser…",
            self.session_id
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
                ("resource", &resource),
            ])
            .map_err(|e| {
                // The client we authorized as is the likeliest suspect;
                // the next login registers a fresh one.
                self.forget_client();
                format!("token exchange failed: {e}")
            })?;
        let body: Value = response
            .into_json()
            .map_err(|e| format!("token exchange returned invalid JSON: {e}"))?;

        let tokens =
            self.tokens_from_response(&body, &endpoints.token_endpoint, &client_id, &resource)?;
        self.save_tokens(&tokens)?;
        Ok(tokens)
    }

    /// Renew the session, once per machine. A refresh token is
    /// single-use: a second caller presenting the same one looks like a
    /// stolen token, and the server answers by revoking the whole
    /// session. So whoever takes the lock talks to the server, and
    /// everyone else reads what it stored.
    fn renew(&self, stale: &Tokens) -> SessionResult<Tokens> {
        let _ = std::fs::create_dir_all(&self.auth_dir);
        let Some(_lock) = FileLock::acquire(self.renewal_lock_path(), RENEWAL_TIMEOUT) else {
            return self.wait_for_renewal(stale);
        };
        if let Some(renewed) = self.renewed_elsewhere(stale) {
            return Ok(renewed);
        }
        let renewed = self.refresh_twice(stale).inspect_err(|error| {
            eprintln!("crewkit: could not renew `{}`: {error}", self.session_id);
            if matches!(error, SessionError::Rejected(_)) {
                self.forget_client();
            }
        })?;
        self.save_tokens(&renewed)
            .map_err(SessionError::Unavailable)?;
        eprintln!("crewkit: renewed `{}`", self.session_id);
        Ok(renewed)
    }

    /// One retry, because a client launched with the machine can beat its
    /// own network by a second or two.
    fn refresh_twice(&self, tokens: &Tokens) -> SessionResult<Tokens> {
        match self.refresh(tokens) {
            Err(SessionError::Unavailable(_)) => {
                std::thread::sleep(Duration::from_secs(2));
                self.refresh(tokens)
            }
            result => result,
        }
    }

    /// The stored session, when another process has already replaced the
    /// one we are holding.
    fn renewed_elsewhere(&self, stale: &Tokens) -> Option<Tokens> {
        self.load_tokens()
            .filter(|current| current.access_token != stale.access_token)
            .filter(Tokens::access_is_fresh)
    }

    fn wait_for_renewal(&self, stale: &Tokens) -> SessionResult<Tokens> {
        let deadline = Instant::now() + RENEWAL_TIMEOUT;
        let mut polls: u32 = 0;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            polls += 1;
            let holder_done = !self.renewal_lock_path().exists();
            // Reading the session shells out to the credential store, so
            // look once a second — or the moment the holder lets go.
            if holder_done || polls.is_multiple_of(4) {
                if let Some(renewed) = self.renewed_elsewhere(stale) {
                    return Ok(renewed);
                }
            }
            if holder_done {
                break;
            }
        }
        // Whoever held the lock has logged why it failed; this caller just
        // steps aside rather than presenting the same refresh token again.
        Err(SessionError::Unavailable(format!(
            "another process is renewing `{}`",
            self.session_id
        )))
    }

    fn refresh(&self, tokens: &Tokens) -> SessionResult<Tokens> {
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| SessionError::Rejected("the server issued no refresh token".into()))?;
        let response = http()
            .post(&tokens.token_endpoint)
            .send_form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", &tokens.client_id),
                ("resource", &tokens.resource),
            ])
            .map_err(classify)?;
        let body: Value = response
            .into_json()
            .map_err(|e| SessionError::Unavailable(e.to_string()))?;
        let mut refreshed = self
            .tokens_from_response(
                &body,
                &tokens.token_endpoint,
                &tokens.client_id,
                &tokens.resource,
            )
            .map_err(SessionError::Rejected)?;
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
        resource: &str,
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
            resource: resource.to_string(),
        })
    }

    // --- Discovery ---

    fn discover(&self) -> Result<AuthEndpoints> {
        let resource_metadata = self.fetch_resource_metadata();
        let resource = resource_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("resource"))
            .and_then(|v| v.as_str())
            .map(String::from);
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
            None => (origin_of(&self.resource)?, None),
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
            resource,
        })
    }

    /// RFC 9728: prefer the URL the server advertises in WWW-Authenticate,
    /// then fall back to the well-known locations.
    fn fetch_resource_metadata(&self) -> Option<Value> {
        if let Some(url) = self.resource_metadata_url() {
            if let Some(value) = get_json(&url) {
                return Some(value);
            }
        }
        let origin = origin_of(&self.resource).ok()?;
        let path = self.resource.strip_prefix(&origin).unwrap_or("");
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

    /// The `resource_metadata` URL for this session: from the challenge the
    /// caller already holds, or — for an MCP endpoint — from one it provokes.
    fn resource_metadata_url(&self) -> Option<String> {
        if let Some(challenge) = &self.challenge {
            if let Some(url) = resource_metadata_of(challenge) {
                return Some(url);
            }
        }
        if !self.probe_mcp {
            return None;
        }
        self.probe_www_authenticate()
    }

    fn probe_www_authenticate(&self) -> Option<String> {
        let response = http()
            .post(&self.resource)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .send_string(r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#);
        let header = match response {
            Err(ureq::Error::Status(401, resp)) => resp.header("WWW-Authenticate")?.to_string(),
            _ => return None,
        };
        resource_metadata_of(&header)
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
                                    "crewkit: `{}` was authorized by another login — done",
                                    self.session_id
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

    /// The OAuth client to authorize as: the one registered earlier when
    /// it still fits this server and callback, a fresh one otherwise.
    /// Registering per login would mean a consent screen every time and a
    /// pile of abandoned clients on the authorization server.
    fn client_id(&self, endpoints: &AuthEndpoints, redirect_uri: &str) -> Result<String> {
        let registration_endpoint = endpoints
            .registration_endpoint
            .as_deref()
            .ok_or("server does not support dynamic client registration")?;
        let known = self.stored_client().filter(|client| {
            client.registration_endpoint == registration_endpoint
                && client.redirect_uris.iter().any(|uri| uri == redirect_uri)
        });
        if let Some(client) = known {
            return Ok(client.client_id);
        }
        let redirect_uris = callback_uris(redirect_uri);
        let client_id = self.register_client(endpoints, registration_endpoint, &redirect_uris)?;
        self.store_client(&RegisteredClient {
            client_id: client_id.clone(),
            registration_endpoint: registration_endpoint.to_string(),
            redirect_uris,
        });
        Ok(client_id)
    }

    fn stored_client(&self) -> Option<RegisteredClient> {
        let text = std::fs::read_to_string(self.clients_path()).ok()?;
        serde_json::from_str::<BTreeMap<String, RegisteredClient>>(&text)
            .ok()?
            .remove(&self.session_id)
    }

    fn store_client(&self, client: &RegisteredClient) {
        self.write_clients(|clients| {
            clients.insert(self.session_id.clone(), client.clone());
        });
    }

    fn forget_client(&self) {
        self.write_clients(|clients| {
            clients.remove(&self.session_id);
        });
    }

    fn write_clients(&self, edit: impl FnOnce(&mut BTreeMap<String, RegisteredClient>)) {
        let mut clients = std::fs::read_to_string(self.clients_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        edit(&mut clients);
        if let Ok(text) = serde_json::to_string_pretty(&clients) {
            let _ = std::fs::write(self.clients_path(), text);
        }
    }

    fn register_client(
        &self,
        endpoints: &AuthEndpoints,
        registration_endpoint: &str,
        redirect_uris: &[String],
    ) -> Result<String> {
        let mut registration = serde_json::json!({
            "client_name": "CrewKit",
            "client_uri": "https://github.com/v3new/crewkit-app",
            "redirect_uris": redirect_uris,
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
    /// The `resource` the protected-resource metadata declares (RFC 8707
    /// audience). For an MCP server it equals the endpoint URL; for a kit
    /// it is the resource root, not the manifest we happened to request.
    resource: Option<String>,
}

enum Callback {
    /// The browser redirect delivered an authorization code.
    Code(String),
    /// A concurrent login for the same server finished first.
    OtherLoginWon(Tokens),
}

/// The outcome of queueing for the machine's one login tab.
enum Turn {
    Ours(FileLock),
    /// The wait ended because the session arrived by other means.
    AlreadyDone(Tokens),
}

/// Prefer a well-known callback port, so the registered OAuth client
/// stays reusable; a random one is the fallback when all are taken.
fn bind_callback() -> Result<TcpListener> {
    for port in CALLBACK_PORTS {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            return Ok(listener);
        }
    }
    TcpListener::bind("127.0.0.1:0").map_err(|e| format!("cannot bind callback: {e}"))
}

/// Register every well-known port, not just the one bound now: the next
/// login may land on a different one and must still fit this client.
fn callback_uris(bound: &str) -> Vec<String> {
    let mut uris: Vec<String> = CALLBACK_PORTS
        .iter()
        .map(|port| format!("http://127.0.0.1:{port}/callback"))
        .collect();
    if !uris.iter().any(|uri| uri == bound) {
        uris.push(bound.to_string());
    }
    uris
}

/// Pull the `resource_metadata` URL out of a `WWW-Authenticate` header,
/// e.g. `Bearer realm="kit", resource_metadata="https://…"`.
pub fn resource_metadata_of(header: &str) -> Option<String> {
    let marker = "resource_metadata=\"";
    let start = header.find(marker)? + marker.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_string())
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
    if crate::cli::command(program)
        .args(args)
        .arg(url)
        .spawn()
        .is_err()
    {
        eprintln!("crewkit: could not open a browser; open this URL manually:\n{url}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_resource_metadata_url_from_a_challenge() {
        let header = "Bearer realm=\"kit\", resource_metadata=\"https://kits.example.com/.well-known/oauth-protected-resource/kit\", scope=\"kit:read\"";

        assert_eq!(
            resource_metadata_of(header).as_deref(),
            Some("https://kits.example.com/.well-known/oauth-protected-resource/kit")
        );
        assert_eq!(resource_metadata_of("Bearer realm=\"kit\""), None);
    }

    fn status(code: u16) -> SessionError {
        classify(ureq::Error::Status(
            code,
            ureq::Response::new(code, "Status", "{}").unwrap(),
        ))
    }

    /// A refused grant is the one case that costs the user a browser tab,
    /// so everything else must classify as reachable-again-later.
    #[test]
    fn only_a_refused_grant_ends_the_session() {
        assert!(matches!(status(400), SessionError::Rejected(_)));
        assert!(matches!(status(401), SessionError::Rejected(_)));
        assert!(matches!(status(429), SessionError::Unavailable(_)));
        assert!(matches!(status(502), SessionError::Unavailable(_)));
    }

    #[test]
    fn a_registered_client_covers_every_well_known_port() {
        let uris = callback_uris("http://127.0.0.1:33418/callback");

        assert_eq!(uris.len(), CALLBACK_PORTS.len());
        assert!(uris.contains(&"http://127.0.0.1:33425/callback".to_string()));
    }

    /// All well-known ports taken: the login still works, and the client
    /// is registered for the odd port too.
    #[test]
    fn a_fallback_port_is_registered_alongside_them() {
        let uris = callback_uris("http://127.0.0.1:51234/callback");

        assert_eq!(uris.len(), CALLBACK_PORTS.len() + 1);
        assert!(uris.contains(&"http://127.0.0.1:51234/callback".to_string()));
    }

    #[test]
    fn a_session_without_a_refresh_token_does_not_renew() {
        let with = r#"{"access_token":"a","refresh_token":"r","expires_at":null,
            "token_endpoint":"https://e/t","client_id":"c","resource":"https://e"}"#;
        let without = r#"{"access_token":"a","refresh_token":null,"expires_at":null,
            "token_endpoint":"https://e/t","client_id":"c","resource":"https://e"}"#;

        assert!(session_renews(with));
        assert!(!session_renews(without));
        assert!(!session_renews("not json"));
    }

    /// Two processes hitting a stale session at the same second: the
    /// second must not present the same single-use refresh token.
    #[test]
    fn only_one_process_renews_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let session = AuthSession::for_mcp(dir.path(), "srv", "https://e/mcp");
        std::fs::create_dir_all(&session.auth_dir).unwrap();
        let stale = Tokens {
            access_token: "old".into(),
            refresh_token: Some("r".into()),
            expires_at: Some(0),
            token_endpoint: "https://e/token".into(),
            client_id: "c".into(),
            resource: "https://e/mcp".into(),
        };

        let held = FileLock::acquire(session.renewal_lock_path(), RENEWAL_TIMEOUT);
        assert!(held.is_some());
        let lock_path = session.renewal_lock_path();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = std::fs::remove_file(lock_path);
        });

        assert!(matches!(
            session.renew(&stale),
            Err(SessionError::Unavailable(_))
        ));
    }

    /// The waiter takes the token the holder stored instead of asking the
    /// server for one of its own.
    #[test]
    fn a_renewal_elsewhere_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let id = format!("crewkit-selftest-{}", std::process::id());
        let session = AuthSession::for_mcp(dir.path(), &id, "https://e/mcp");
        let stale = Tokens {
            access_token: "old".into(),
            refresh_token: Some("r".into()),
            expires_at: Some(0),
            token_endpoint: "https://e/token".into(),
            client_id: "c".into(),
            resource: "https://e/mcp".into(),
        };

        assert!(session.renewed_elsewhere(&stale).is_none());

        let renewed = Tokens {
            access_token: "new".into(),
            expires_at: Some(now_unix() + 3600),
            ..stale.clone()
        };
        session.save_tokens(&renewed).unwrap();

        assert_eq!(
            session.renewed_elsewhere(&stale).map(|t| t.access_token),
            Some("new".to_string())
        );
        let _ = session.logout();
    }
}
