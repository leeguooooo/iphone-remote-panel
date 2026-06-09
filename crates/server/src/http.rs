//! axum HTTP app: auth-gated routes for the WebRTC web client.
//!
//! Routes (contract from `web/index.html`):
//!   * `GET  /phone`       — auth-gated; serves the embedded web client.
//!   * `GET  /login`       — password form.
//!   * `POST /login`       — password check → set signed `phone_session` cookie.
//!   * `GET  /logout`      — clear the cookie.
//!   * `GET  /turn-creds`  — auth-gated; `{iceServers:[...]}` (STUN + env TURN).
//!   * `GET  /ws`          — auth-gated WebSocket; daemon-offerer signaling.
//!   * `GET  /`            — redirect to `/phone`.
//!
//! Auth: the `phone_session` cookie value is an HMAC session token minted by
//! [`core::auth::make_token`] and verified by [`core::auth::check_token`] using
//! the daemon secret. When no password is configured, all routes are open (LAN
//! dev mode) — the gate short-circuits to "authed".
//!
//! Security headers (v1 parity): `Cache-Control: no-store`, `X-Frame-Options:
//! DENY`, `Referrer-Policy: no-referrer` on every response.

use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::{ws::WebSocketUpgrade, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use serde::Deserialize;
use webrtc::ice_transport::ice_server::RTCIceServer;

use core::control::{Control, Lease};
use core::encode::VideoPipeline;

use crate::input_bridge::InputInjector;

/// The embedded web client served at `/phone`.
const INDEX_HTML: &str = include_str!("../../../web/index.html");

/// The cookie name the web client and daemon agree on.
const SESSION_COOKIE: &str = "phone_session";

/// ICE servers + their precomputed `/turn-creds` JSON, kept together so a TURN
/// refresh swaps both atomically (see [`AppState::ice`]).
pub struct IceState {
    /// ICE servers handed to each PeerConnection.
    pub servers: Vec<RTCIceServer>,
    /// JSON `iceServers` array body returned by `/turn-creds`.
    pub json: String,
}

impl IceState {
    /// Build from a server list, precomputing the `/turn-creds` JSON.
    pub fn new(servers: Vec<RTCIceServer>) -> Self {
        let json = ice_servers_json(&servers);
        Self { servers, json }
    }
}

/// Shared application state for all handlers.
pub struct AppState {
    /// Running video pipeline the WebRTC feed subscribes to.
    pub pipeline: Arc<dyn VideoPipeline>,
    /// ICE servers + `/turn-creds` JSON. Behind an `ArcSwap` so the Cloudflare
    /// TURN refresh task can hot-swap fresh ephemeral credentials without a
    /// restart; readers `load()` the current snapshot.
    pub ice: Arc<arc_swap::ArcSwap<IceState>>,
    /// Optional shared password; `None` = open (LAN dev) mode.
    pub password: Option<String>,
    /// Secret for signing session cookies (always present; generated if unset).
    pub secret: Vec<u8>,
    /// Session TTL in seconds.
    pub session_ttl_secs: u64,
    /// Whether to mark the cookie `Secure` (true behind TLS).
    pub cookie_secure: bool,
    /// Control lease arbitration (single shared cursor). Shared (same `Arc`) with
    /// the input injector's gate so a lease change is visible to both.
    pub control: Arc<Mutex<Control>>,
    /// The lease held by the current viewer (if any). Shared with the injector gate.
    pub current_lease: Arc<Mutex<Option<Lease>>>,
    /// Input injector (decoded events → CgEventSink on its own thread).
    pub injector: InputInjector,
    /// cua-driver target `(binary_path, pid, window_id)` of the Mirroring window,
    /// if found — used by the agent API to report `phone_target` (and, later, to
    /// capture screenshots). `None` = pointer-only (no key/text/shortcut target).
    pub cua_target: Option<(String, i32, u32)>,
}

/// Build the axum router for the daemon.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/phone", get(phone))
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", get(logout))
        .route("/turn-creds", get(turn_creds))
        .route("/ws", get(ws_upgrade))
        // Agent operation entry (connect-in; reuses the validated injector +
        // control lease). Bearer-token auth; see `agent_input` / `agent_status`.
        .route("/agent/status", get(agent_status))
        .route("/agent/input", post(agent_input))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Security headers
// ---------------------------------------------------------------------------

/// Apply v1 security headers to a response.
fn with_security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// Extract the `phone_session` cookie value from request headers.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return Some(value.to_string());
        }
    }
    None
}

/// Return `true` if the request is authenticated.
///
/// When no password is configured the daemon runs open (LAN dev) and every
/// request is treated as authed. Otherwise the `phone_session` cookie must carry
/// a valid, unexpired token signed by the daemon secret.
pub fn is_authed(state: &AppState, headers: &HeaderMap) -> bool {
    if state.password.is_none() {
        return true;
    }
    match session_cookie(headers) {
        Some(token) => core::auth::check_token(&state.secret, &token, now_secs()),
        None => false,
    }
}

/// True when the request reached us over HTTPS. The daemon itself always serves
/// plain HTTP; HTTPS is terminated by the Cloudflare tunnel, which forwards
/// `X-Forwarded-Proto: https`. We must decide `Secure` **per request** and NOT
/// from the bind host: a LAN bind (`0.0.0.0`) is still plain HTTP, and a `Secure`
/// cookie is rejected by browsers over plain HTTP — which silently breaks the
/// `/ws` auth (the cookie isn't sent on the `ws://` upgrade) and thus WebRTC.
fn request_is_https(state: &AppState, headers: &HeaderMap) -> bool {
    if state.cookie_secure {
        return true; // explicit force (e.g. an external HTTPS terminator)
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// Build the `Set-Cookie` header value for a freshly-minted session.
fn make_session_cookie(state: &AppState, secure: bool) -> String {
    let token = core::auth::make_token(&state.secret, state.session_ttl_secs, now_secs());
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
        state.session_ttl_secs
    )
}

/// Build the cookie that clears the session.
fn clear_session_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}")
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

async fn root() -> Response {
    with_security_headers(Redirect::to("/phone").into_response())
}

async fn phone(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_authed(&state, &headers) {
        return with_security_headers(Redirect::to("/login").into_response());
    }
    with_security_headers(Html(INDEX_HTML).into_response())
}

/// The login form HTML (self-contained, no external assets).
const LOGIN_HTML: &str = r#"<!doctype html><html lang="zh-CN"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>登录 · iPhone Remote</title>
<style>
:root{color-scheme:dark}
html,body{margin:0;height:100%;background:#08090c;color:#eef2ff;
  font-family:-apple-system,BlinkMacSystemFont,"PingFang SC","Segoe UI",sans-serif;
  display:flex;align-items:center;justify-content:center}
form{background:#11131a;border:1px solid #272b38;border-radius:16px;padding:28px 24px;
  width:min(86vw,320px);display:flex;flex-direction:column;gap:14px}
h1{font-size:17px;margin:0 0 4px;letter-spacing:.02em}
input{background:#08090c;color:#eef2ff;border:1px solid #272b38;border-radius:12px;
  padding:12px 14px;font-size:16px;-webkit-appearance:none}
input:focus{outline:none;border-color:#4f8cff}
button{background:#4f8cff;border:1px solid #4f8cff;color:#fff;border-radius:12px;
  padding:12px;font-size:15px;font-weight:600;cursor:pointer}
.err{color:#ff5a66;font-size:13px;min-height:1em}
</style></head><body>
<form method="POST" action="/login">
  <h1>iPhone Remote</h1>
  <div class="err">__ERR__</div>
  <input type="password" name="password" placeholder="密码" autofocus
    autocomplete="current-password" />
  <button type="submit">登录</button>
</form></body></html>"#;

async fn login_form(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Already authed → straight to the client.
    if is_authed(&state, &headers) {
        return with_security_headers(Redirect::to("/phone").into_response());
    }
    with_security_headers(Html(LOGIN_HTML.replace("__ERR__", "")).into_response())
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let expected = match &state.password {
        // Open mode: any login succeeds (no password configured).
        None => return redirect_with_cookie(&state, "/phone", &headers),
        Some(p) => p,
    };
    if core::auth::verify_password(&form.password, expected) {
        redirect_with_cookie(&state, "/phone", &headers)
    } else {
        let body = LOGIN_HTML.replace("__ERR__", "密码错误");
        let mut resp = Html(body).into_response();
        *resp.status_mut() = StatusCode::UNAUTHORIZED;
        with_security_headers(resp)
    }
}

/// 303-redirect to `to`, setting a fresh session cookie (Secure iff the request
/// arrived over HTTPS — see `request_is_https`).
fn redirect_with_cookie(state: &AppState, to: &str, headers: &HeaderMap) -> Response {
    let secure = request_is_https(state, headers);
    let mut resp = Redirect::to(to).into_response();
    if let Ok(v) = HeaderValue::from_str(&make_session_cookie(state, secure)) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    with_security_headers(resp)
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let secure = request_is_https(&state, &headers);
    let mut resp = Redirect::to("/login").into_response();
    if let Ok(v) = HeaderValue::from_str(&clear_session_cookie(secure)) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    with_security_headers(resp)
}

async fn turn_creds(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_authed(&state, &headers) {
        return with_security_headers(
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        );
    }
    let mut resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(state.ice.load().json.clone()))
        .unwrap();
    resp = with_security_headers(resp);
    resp
}

// ---------------------------------------------------------------------------
// Agent operation entry (connect-in HTTP API)
// ---------------------------------------------------------------------------
//
// An agent (Hermes, a Claude MCP client, a script) drives the phone by POSTing
// control messages to the *already-running, TCC-granted* daemon — never by
// spawning its own input process (macOS's responsible-process rule makes a
// spawned child's CGEvents untrusted). The daemon injects through the same
// validated path as the human WebRTC client, taking an `Agent` control lease.

/// Extract a `Authorization: Bearer <token>` value.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").map(|s| s.trim().to_string())
}

/// Return `true` if the request may use the agent API.
///
/// When a password is configured the agent must present it as a bearer token
/// (`Authorization: Bearer <password>`). With no password the daemon is in open
/// LAN-dev mode and the agent API is open too — same policy as [`is_authed`].
fn is_agent_authed(state: &AppState, headers: &HeaderMap) -> bool {
    match &state.password {
        None => true,
        Some(pw) => bearer_token(headers).is_some_and(|t| {
            // length-checked byte compare (avoids early-exit on first mismatch)
            let (a, b) = (t.as_bytes(), pw.as_bytes());
            a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
        }),
    }
}

/// `GET /agent/status` — auth/health probe. `{"ok":true,"phone_target":bool}`.
///
/// `phone_target` reports whether the daemon has a cua-driver window target
/// (needed for key/text/shortcut); pointer/tap/scroll work regardless.
async fn agent_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_agent_authed(&state, &headers) {
        return with_security_headers((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }
    let body = format!(
        r#"{{"ok":true,"phone_target":{}}}"#,
        state.cua_target.is_some()
    );
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap();
    with_security_headers(resp)
}

/// `POST /agent/input` — inject one control message (same JSON shape as the
/// WebRTC control channel): `{"type":"tap","x":0.5,"y":0.5}`,
/// `{"type":"text","text":"hi"}`, `{"type":"scroll","x":..,"y":..,"dx":..,"dy":..}`,
/// `{"type":"shortcut","name":"home"}`, `{"type":"key","name":"return"}`, etc.
///
/// Coordinates are normalized `[0,1]` over the phone content rect (geometry-agnostic,
/// like the web client). Acquiring an `Agent` control lease makes the injector gate
/// allow the event; this preempts a human viewer (single shared cursor, last actor
/// wins). Returns 200 on accept, 400 on an unparseable message.
async fn agent_input(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !is_agent_authed(&state, &headers) {
        return with_security_headers((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }
    let event = match crate::input_bridge::decode_control(&body) {
        Some(ev) => ev,
        None => {
            return with_security_headers(
                (StatusCode::BAD_REQUEST, "invalid control message").into_response(),
            );
        }
    };
    // Take an Agent lease so the injector gate permits this event.
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("agent")
        .to_string();
    {
        let mut control = state.control.lock().unwrap();
        let lease = control.acquire(core::control::Holder::Agent(agent_id), now_secs());
        *state.current_lease.lock().unwrap() = Some(lease);
    }
    state.injector.send(event);
    with_security_headers((StatusCode::OK, "ok").into_response())
}

async fn ws_upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !is_authed(&state, &headers) {
        return with_security_headers(
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        );
    }
    let session_id = new_session_id();
    let state = state.clone();
    ws.on_upgrade(move |socket| async move {
        crate::signaling::run_session(socket, state, session_id).await;
    })
}

// ---------------------------------------------------------------------------
// ICE servers / TURN creds
// ---------------------------------------------------------------------------

/// Build the ICE server list: Google STUN + any env-provided TURN.
///
/// `PHONE_REMOTE_TURN_URLS` (comma-separated), `PHONE_REMOTE_TURN_USERNAME`,
/// `PHONE_REMOTE_TURN_CREDENTIAL` configure an optional TURN server. STUN is
/// always included.
pub fn build_ice_servers(
    turn_urls: Option<String>,
    turn_user: Option<String>,
    turn_cred: Option<String>,
) -> Vec<RTCIceServer> {
    let mut servers = vec![RTCIceServer {
        urls: vec!["stun:stun.l.google.com:19302".to_owned()],
        ..Default::default()
    }];
    if let Some(urls) = turn_urls.filter(|s| !s.trim().is_empty()) {
        let urls: Vec<String> = urls
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if !urls.is_empty() {
            servers.push(RTCIceServer {
                urls,
                username: turn_user.unwrap_or_default(),
                credential: turn_cred.unwrap_or_default(),
            });
        }
    }
    servers
}

/// Serialize ICE servers into the `{iceServers:[...]}` JSON the client expects.
///
/// Each entry is normalized to `{urls, username?, credential?}` (username/
/// credential omitted when empty).
pub fn ice_servers_json(servers: &[RTCIceServer]) -> String {
    let arr: Vec<serde_json::Value> = servers
        .iter()
        .map(|s| {
            let mut obj = serde_json::Map::new();
            obj.insert("urls".to_string(), serde_json::json!(s.urls));
            if !s.username.is_empty() {
                obj.insert("username".to_string(), serde_json::json!(s.username));
            }
            if !s.credential.is_empty() {
                obj.insert("credential".to_string(), serde_json::json!(s.credential));
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    serde_json::json!({ "iceServers": arr }).to_string()
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A best-effort unique session id (time + a counter). Not security-sensitive —
/// it only labels the control lease holder.
fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("viewer-{}-{}", now_secs(), n)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ice_servers_stun_only_by_default() {
        let servers = build_ice_servers(None, None, None);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls[0], "stun:stun.l.google.com:19302");
    }

    #[test]
    fn ice_servers_with_env_turn() {
        let servers = build_ice_servers(
            Some("turn:turn.example.com:3478,turns:turn.example.com:5349".to_string()),
            Some("user".to_string()),
            Some("pass".to_string()),
        );
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].urls.len(), 2);
        assert_eq!(servers[1].username, "user");
        assert_eq!(servers[1].credential, "pass");
    }

    #[test]
    fn ice_servers_empty_turn_urls_ignored() {
        let servers = build_ice_servers(Some("   ".to_string()), None, None);
        assert_eq!(servers.len(), 1);
    }

    #[test]
    fn ice_json_normalizes_to_array_stun() {
        let servers = build_ice_servers(None, None, None);
        let json = ice_servers_json(&servers);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["iceServers"].is_array());
        assert_eq!(v["iceServers"][0]["urls"][0], "stun:stun.l.google.com:19302");
        // No username/credential on a bare STUN entry.
        assert!(v["iceServers"][0].get("username").is_none());
    }

    #[test]
    fn ice_json_includes_turn_creds() {
        let servers = build_ice_servers(
            Some("turn:t.example:3478".to_string()),
            Some("u".to_string()),
            Some("c".to_string()),
        );
        let json = ice_servers_json(&servers);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["iceServers"][1]["username"], "u");
        assert_eq!(v["iceServers"][1]["credential"], "c");
    }

    #[test]
    fn session_cookie_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("foo=bar; phone_session=abc.def.ghi; baz=qux"),
        );
        assert_eq!(session_cookie(&headers), Some("abc.def.ghi".to_string()));
    }

    #[test]
    fn session_cookie_absent() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("foo=bar"));
        assert_eq!(session_cookie(&headers), None);
        assert_eq!(session_cookie(&HeaderMap::new()), None);
    }

    #[test]
    fn embedded_index_html_is_the_client() {
        // include_str! must pick up web/index.html (the WebRTC client).
        assert!(INDEX_HTML.contains("iPhone Remote"));
        assert!(INDEX_HTML.contains("/ws"));
        assert!(INDEX_HTML.contains("turn-creds"));
    }
}
