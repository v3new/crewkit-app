//! The proxy loop: newline-delimited JSON-RPC on stdio (what every AI
//! client speaks) forwarded verbatim to a remote Streamable HTTP MCP
//! server. Messages are not interpreted beyond what transport plumbing
//! requires (session id, protocol version, response correlation), so the
//! bridge does not lag behind MCP protocol evolution.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crewkit_core::paths::Paths;
use serde_json::Value;

use crate::auth::AuthSession;

type Result<T> = std::result::Result<T, String>;

struct Shared {
    auth: AuthSession,
    url: String,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    stdout: Mutex<std::io::Stdout>,
    listener_started: AtomicBool,
}

impl Shared {
    fn write_line(&self, line: &str) {
        let mut stdout = self.stdout.lock().expect("stdout lock");
        let _ = stdout.write_all(line.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}

pub fn serve(paths: &Paths, server_id: &str, url: &str) -> Result<()> {
    let shared = Arc::new(Shared {
        auth: AuthSession::new(paths, server_id, url),
        url: url.to_string(),
        session_id: Mutex::new(None),
        protocol_version: Mutex::new(None),
        stdout: Mutex::new(std::io::stdout()),
        listener_started: AtomicBool::new(false),
    });

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("stdin read failed: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        // One thread per message: a slow streaming tool call must not
        // block the next request. Clients correlate responses by id.
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || handle_message(&shared, &line));
    }
    // stdin closed: the client is gone, so is our job.
    Ok(())
}

fn handle_message(shared: &Arc<Shared>, line: &str) {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("crewkit-bridge: dropping malformed message: {e}");
            return;
        }
    };
    let request_id = message.get("id").cloned();
    let is_initialize = message.get("method").and_then(|m| m.as_str()) == Some("initialize");

    // Interactive auth is allowed here: the very first client connection
    // may trigger the one browser tab (deduplicated across processes).
    let token = match shared.auth.access_token(true) {
        Ok(token) => token,
        Err(error) => return fail_request(shared, request_id, &error),
    };

    match forward(shared, line, &token, is_initialize) {
        Ok(()) => {}
        Err(ForwardError::Unauthorized) => {
            // Token went stale server-side — reauthorize once and retry.
            match shared.auth.reauthorize(true) {
                Ok(token) => {
                    if let Err(error) = forward(shared, line, &token, is_initialize) {
                        fail_request(shared, request_id, &error.to_string());
                    }
                }
                Err(error) => fail_request(shared, request_id, &error),
            }
        }
        Err(error) => fail_request(shared, request_id, &error.to_string()),
    }
}

/// A request must never be left dangling: on transport failure the client
/// gets a proper JSON-RPC error (notifications fail silently to stderr).
fn fail_request(shared: &Shared, request_id: Option<Value>, error: &str) {
    eprintln!("crewkit-bridge: {error}");
    if let Some(id) = request_id {
        if !id.is_null() {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": format!("crewkit-bridge: {error}") },
            });
            shared.write_line(&response.to_string());
        }
    }
}

enum ForwardError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            ForwardError::Other(message) => write!(f, "{message}"),
        }
    }
}

fn forward(
    shared: &Arc<Shared>,
    body: &str,
    token: &str,
    is_initialize: bool,
) -> std::result::Result<(), ForwardError> {
    let mut request = ureq::post(&shared.url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .set("Authorization", &format!("Bearer {token}"));
    if let Some(session) = shared.session_id.lock().expect("session lock").clone() {
        request = request.set("Mcp-Session-Id", &session);
    }
    if let Some(version) = shared
        .protocol_version
        .lock()
        .expect("protocol lock")
        .clone()
    {
        request = request.set("MCP-Protocol-Version", &version);
    }

    let response = match request.send_string(body) {
        Ok(response) => response,
        Err(ureq::Error::Status(401, _)) => return Err(ForwardError::Unauthorized),
        Err(ureq::Error::Status(code, response)) => {
            let detail = response.into_string().unwrap_or_default();
            let detail: String = detail.chars().take(300).collect();
            return Err(ForwardError::Other(format!(
                "upstream HTTP {code}: {detail}"
            )));
        }
        Err(e) => return Err(ForwardError::Other(format!("request failed: {e}"))),
    };

    if let Some(session) = response.header("Mcp-Session-Id") {
        *shared.session_id.lock().expect("session lock") = Some(session.to_string());
    }

    let content_type = response.content_type().to_string();
    if content_type.starts_with("text/event-stream") {
        forward_sse(response.into_reader(), shared, is_initialize);
    } else if content_type.starts_with("application/json") {
        let mut text = String::new();
        if response.into_reader().read_to_string(&mut text).is_ok() && !text.trim().is_empty() {
            if is_initialize {
                note_initialize_result(shared, text.trim());
            }
            shared.write_line(text.trim());
        }
    }
    // 202/empty responses (notifications) produce no output.

    if is_initialize {
        start_server_stream(shared);
    }
    Ok(())
}

/// Forward every SSE `data:` payload as one stdio line.
fn forward_sse(reader: impl Read, shared: &Shared, is_initialize: bool) {
    let mut data = String::new();
    let flush = |data: &mut String| {
        if !data.is_empty() {
            if is_initialize {
                note_initialize_result(shared, data);
            }
            shared.write_line(data);
            data.clear();
        }
    };
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            flush(&mut data);
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // event:/id:/retry:/comment lines carry no payload — ignored.
    }
    flush(&mut data);
}

/// Remember the negotiated protocol version from the initialize result;
/// later requests echo it in the MCP-Protocol-Version header.
fn note_initialize_result(shared: &Shared, payload: &str) {
    if let Ok(value) = serde_json::from_str::<Value>(payload) {
        if let Some(version) = value
            .get("result")
            .and_then(|r| r.get("protocolVersion"))
            .and_then(|v| v.as_str())
        {
            *shared.protocol_version.lock().expect("protocol lock") = Some(version.to_string());
        }
    }
}

/// Open the server→client SSE stream (server-initiated notifications and
/// requests). Optional per spec — a 4xx means the server doesn't use it.
fn start_server_stream(shared: &Arc<Shared>) {
    if shared.listener_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        let mut failures = 0;
        while failures < 3 {
            let Ok(token) = shared.auth.access_token(false) else {
                return;
            };
            let mut request = ureq::get(&shared.url)
                .set("Accept", "text/event-stream")
                .set("Authorization", &format!("Bearer {token}"));
            if let Some(session) = shared.session_id.lock().expect("session lock").clone() {
                request = request.set("Mcp-Session-Id", &session);
            }
            match request.call() {
                Ok(response) if response.content_type().starts_with("text/event-stream") => {
                    failures = 0;
                    forward_sse(response.into_reader(), &shared, false);
                    // Stream ended; reconnect after a pause.
                    std::thread::sleep(Duration::from_secs(1));
                }
                Ok(_) | Err(ureq::Error::Status(_, _)) => return,
                Err(_) => {
                    failures += 1;
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
}
