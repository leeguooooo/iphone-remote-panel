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
use std::time::Instant;

/// Recover a poisoned mutex guard instead of panicking. A panic inside a lock
/// holder poisons the mutex; for the control-lease state that would permanently
/// disable the lease subsystem. The data stays consistent, so unwrapping the
/// poison error is safe here.
#[inline]
fn recover<T>(r: std::sync::LockResult<T>) -> T {
    r.unwrap_or_else(std::sync::PoisonError::into_inner)
}

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

// ---------------------------------------------------------------------------
// Rate limiter (login + agent bearer auth failures)
// ---------------------------------------------------------------------------

/// In-memory failure tracking for `/login` (wrong password) and failed agent
/// bearer auth.  After [`AUTH_MAX_FAILURES`] consecutive failures the limiter
/// locks out all auth attempts for [`AUTH_LOCKOUT_SECS`] seconds.
///
/// **Design notes / tradeoffs:**
/// - Global (not per-IP) to keep the implementation simple and testable.
///   This daemon fronts a single household: the realistic threat is a brute-
///   force bot, not a multi-origin attack, and per-IP requires `ConnectInfo`
///   which is unavailable in axum oneshot tests.
/// - A success (correct password) resets the failure counter and lifts an
///   active lockout immediately — legitimate users are never permanently
///   locked out by their own typos.
/// - The lockout window is 30 seconds (sliding, reset by a success).
pub struct AuthLimiter {
    pub(crate) failures: u32,
    pub(crate) locked_until: Option<Instant>,
}

/// Number of consecutive failures that trigger a lockout.
const AUTH_MAX_FAILURES: u32 = 5;

/// Lockout duration in seconds after hitting [`AUTH_MAX_FAILURES`].
const AUTH_LOCKOUT_SECS: u64 = 30;

impl AuthLimiter {
    pub fn new() -> Self {
        AuthLimiter { failures: 0, locked_until: None }
    }

    /// Returns `true` if requests should be rejected right now.
    pub fn is_locked(&self) -> bool {
        match self.locked_until {
            Some(until) => Instant::now() < until,
            None => false,
        }
    }

    /// Record an auth failure.  Starts or extends the lockout window once
    /// the failure count reaches [`AUTH_MAX_FAILURES`].
    pub fn record_failure(&mut self) {
        self.failures += 1;
        if self.failures >= AUTH_MAX_FAILURES {
            self.locked_until = Some(
                Instant::now() + std::time::Duration::from_secs(AUTH_LOCKOUT_SECS),
            );
        }
    }

    /// Record a successful auth.  Resets the failure counter and lifts any
    /// active lockout.
    pub fn record_success(&mut self) {
        self.failures = 0;
        self.locked_until = None;
    }
}

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
    /// Rate limiter for login and agent bearer auth failures.
    /// After 5 consecutive failures requests are rejected with 429 for 30 s.
    pub auth_limiter: Arc<Mutex<AuthLimiter>>,
    /// Optional dedicated bearer token for agent/API access.
    ///
    /// When `Some`, the `Authorization: Bearer` credential on the agent paths must
    /// match this token; the human login password is **not** accepted as a bearer
    /// (clean separation of human and machine secrets).
    ///
    /// When `None`, the existing behavior applies: the password (if set) is used as
    /// the bearer check, and open mode (no password) passes everything through.
    pub agent_token: Option<String>,
    /// Inbox: structured results POSTed back BY the phone (e.g. an iOS Shortcut's
    /// "Get Contents of URL" action returning Health / battery / location JSON),
    /// for an agent to GET. This is the return path of the Shortcuts RPC bridge —
    /// the daemon triggers a shortcut by name, the shortcut runs a native iOS
    /// action and POSTs the result here. Bounded ring buffer; oldest dropped.
    pub inbox: Arc<Mutex<std::collections::VecDeque<InboxItem>>>,
    /// Optional L2 element-tree control via WebDriverAgent on the phone
    /// (`PHONE_REMOTE_WDA_URL`, e.g. `http://<phone-ip>:8100`). When present,
    /// agent input auto-routes through it (see [`agent_input`]): text goes in
    /// as Unicode (CJK lands cleanly), taps are synthesized on-device (no host
    /// cursor), with the L3 pixel path as fallback on any WDA error.
    /// `tokio::sync::Mutex` because the client mutates its cached session and
    /// handlers hold the lock across awaits.
    pub wda: Option<Arc<tokio::sync::Mutex<crate::wda::WdaClient>>>,
    /// Latest released tag on GitHub (e.g. `"v0.3.0"`), refreshed by a
    /// background task every 24h (`main::spawn_update_check`). `None` until
    /// the first successful fetch (or when offline). Read by `agent_status`
    /// to surface `update_available` to agents and the web client.
    pub latest_release: Arc<Mutex<Option<String>>>,
    /// Single-active-viewer arbitration for `/ws` (issue #8: queue + notify).
    /// One viewer streams at a time; others wait in line and are promoted when
    /// the active one disconnects. Read by `/agent/status` as `viewer_count`.
    pub viewers: Arc<Mutex<crate::signaling::ViewerRegistry>>,
    /// Memoized Mirroring window classification (issue #14/#3): `(checked_at,
    /// state)`. Detection runs `screencapture`, so `/agent/status` reuses a
    /// recent result instead of re-capturing on every poll.
    pub mirror_paused_cache: Arc<Mutex<Option<(Instant, core::capture::MirrorState)>>>,
}

/// One message in the [`AppState::inbox`] — arbitrary JSON the phone POSTed back,
/// plus when the daemon received it.
#[derive(Clone, serde::Serialize)]
pub struct InboxItem {
    /// Unix seconds the daemon received this item.
    pub received_at: u64,
    /// The JSON body the phone (shortcut) sent.
    pub body: serde_json::Value,
}

/// Max inbox items retained (oldest dropped past this).
const INBOX_CAP: usize = 64;

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
        .route("/agent/mode", post(agent_mode))
        .route("/agent/input", post(agent_input))
        .route("/agent/screenshot", get(agent_screenshot))
        .route("/agent/elements", get(agent_elements))
        // Shortcuts RPC return path: the phone POSTs structured results here;
        // an agent GETs (and drains) them. See `AppState::inbox`.
        .route("/agent/inbox", get(agent_inbox_get).post(agent_inbox_post))
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
<title>登录 · iphone-use</title>
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
  <h1>iphone-use</h1>
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
        // Open mode: any login succeeds (no password configured); no limiting.
        None => return redirect_with_cookie(&state, "/phone", &headers),
        Some(p) => p.clone(),
    };
    // Check the limiter BEFORE verifying the password (prevents timing oracle).
    {
        let limiter = state.auth_limiter.lock().unwrap();
        if limiter.is_locked() {
            return with_security_headers(
                (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
            );
        }
    }
    if core::auth::verify_password(&form.password, &expected) {
        state.auth_limiter.lock().unwrap().record_success();
        redirect_with_cookie(&state, "/phone", &headers)
    } else {
        state.auth_limiter.lock().unwrap().record_failure();
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
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(state.ice.load().json.clone()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    with_security_headers(resp)
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

/// Extract the bytes after `Authorization: Bearer `.
///
/// Works on the raw header bytes, NOT `to_str()`: a non-ASCII password — e.g. a
/// Chinese one — makes `HeaderValue::to_str()` fail, which 401'd every agent
/// request (caught on hardware). Reading bytes + trimming ASCII whitespace
/// handles the UTF-8 token a client (curl) sends verbatim.
fn bearer_credential(headers: &HeaderMap) -> Option<&[u8]> {
    let v = headers.get(header::AUTHORIZATION)?;
    Some(v.as_bytes().strip_prefix(b"Bearer ")?.trim_ascii())
}

/// Constant-time byte-level equality check (length-guarded, UTF-8 safe).
///
/// Returns `true` iff `a` and `b` are byte-for-byte identical.  Uses a
/// fold over XOR so the compiler cannot short-circuit, preventing timing
/// oracles regardless of value length.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Return `true` if the bearer credential matches the effective agent secret.
///
/// **Selection logic** (in order):
/// 1. `agent_token` is configured → the bearer must match it; the password is
///    **not** a valid bearer credential (clean separation).
/// 2. `agent_token` is absent and `password` is configured → fall back to the
///    original behavior (password doubles as the bearer secret).
/// 3. Neither is configured (open mode) → always returns `true`.
///
/// Does NOT touch the rate limiter — callers must check / record against the
/// shared `auth_limiter` themselves so the limiter covers both login and agent
/// paths with a unified counter.
fn check_bearer(state: &AppState, headers: &HeaderMap) -> bool {
    // Determine which secret governs bearer auth.
    let expected: &str = match (&state.agent_token, &state.password) {
        // Dedicated agent token takes precedence; password is NOT accepted.
        (Some(tok), _) => tok,
        // No dedicated token → fall back to the password (original behavior).
        (None, Some(pw)) => pw,
        // Open mode (neither configured) → always authed.
        (None, None) => return true,
    };
    bearer_credential(headers)
        .is_some_and(|token| ct_eq(token, expected.as_bytes()))
}

/// Outcome of an agent auth check (combines lockout + credential verify).
enum AgentAuth {
    /// Request may proceed.
    Ok,
    /// Auth limiter triggered — respond 429.
    Locked,
    /// Credential missing or wrong — respond 401.
    Denied,
}

/// Check the agent bearer token and advance the rate limiter.
///
/// * Checks the limiter BEFORE credential verification.
/// * Records a failure (wrong or missing bearer) or success (correct) in the
///   shared [`AuthLimiter`].
/// * Open-mode (neither `agent_token` nor `password` configured): always returns
///   `Ok` without touching the limiter so open-mode integration tests stay clean.
fn agent_auth(state: &AppState, headers: &HeaderMap) -> AgentAuth {
    // Open mode: no credential of any kind is configured.
    if state.agent_token.is_none() && state.password.is_none() {
        return AgentAuth::Ok;
    }
    {
        let limiter = state.auth_limiter.lock().unwrap();
        if limiter.is_locked() {
            return AgentAuth::Locked;
        }
    }
    if check_bearer(state, headers) {
        state.auth_limiter.lock().unwrap().record_success();
        AgentAuth::Ok
    } else {
        state.auth_limiter.lock().unwrap().record_failure();
        AgentAuth::Denied
    }
}

/// What the Mirroring window is showing (active / paused / in_use). Memoized for
/// [`MIRROR_STATE_CACHE_TTL`] so `/agent/status` polling doesn't run a
/// `screencapture` on every request. Detection is blocking (spawns
/// `screencapture` + decodes), so it runs on a blocking thread.
async fn mirror_state_cached(state: &Arc<AppState>) -> core::capture::MirrorState {
    const MIRROR_STATE_CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(1000);
    if let Some((at, s)) = *recover(state.mirror_paused_cache.lock()) {
        if at.elapsed() < MIRROR_STATE_CACHE_TTL {
            return s;
        }
    }
    let s = tokio::task::spawn_blocking(|| {
        core::capture::mirroring_state().unwrap_or(core::capture::MirrorState::Active)
    })
    .await
    .unwrap_or(core::capture::MirrorState::Active);
    *recover(state.mirror_paused_cache.lock()) = Some((Instant::now(), s));
    s
}

/// `GET /agent/status` — auth/health probe. `{"ok":true,"phone_target":bool}`.
///
/// `phone_target` is `true` when an iPhone Mirroring window is currently
/// findable on-screen (cheap `find_mirroring_geometry` probe at request time;
/// macOS only — non-macOS always returns `false`).  This replaces the old
/// cua-driver window-target check: input is now fully native (CGEvent), so no
/// external binary is needed for key/text/shortcut — `phone_target` simply
/// tells the agent whether the Mirroring window is up right now.
async fn agent_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Same cookie-or-bearer rule as `agent_screenshot`: a logged-in browser
    // viewer may read the health/version probe (the web client uses it for
    // the update banner). Cookie first so polling never trips the limiter;
    // only honored when a password is configured (see agent_screenshot).
    let cookie_ok = state.password.is_some() && is_authed(&state, &headers);
    if !cookie_ok {
        match agent_auth(&state, &headers) {
            AgentAuth::Locked => return with_security_headers(
                (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
            ),
            AgentAuth::Denied => return with_security_headers(
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            ),
            AgentAuth::Ok => {}
        }
    }
    // Cheap probe: returns Ok if ScreenCaptureKit can see the window.
    #[cfg(target_os = "macos")]
    let phone_target = core::capture::find_mirroring_geometry().is_ok();
    #[cfg(not(target_os = "macos"))]
    let phone_target = false;
    // L2 liveness: configured AND answering /status right now.
    let wda = match &state.wda {
        Some(w) => w.lock().await.is_up().await,
        None => false,
    };
    // Derived mode (see `agent_mode`): WDA up wins — while the on-phone
    // XCUITest runner is alive, Mirroring CANNOT connect (hardware-verified
    // mutual exclusion), so phone_target at best shows the Interrupted screen.
    let mode = if wda {
        "agent"
    } else if phone_target {
        "mirror"
    } else {
        "offline"
    };
    // mirror_state + drivable (issue #14 §1): `phone_target` only says the
    // Mirroring *window* exists — it stays true on the "Connection Paused" /
    // "in use" interstitial, where L3 taps land in the void. `drivable` is the
    // honest "can an agent act right now" signal: WDA always can (on-device);
    // the mirror path can only when the window isn't paused.
    let (mirror_state, drivable) = if wda {
        ("active", true) // WDA injects on-device regardless of the mirror window
    } else if phone_target {
        let s = mirror_state_cached(&state).await;
        (s.as_str(), s.drivable())
    } else {
        ("offline", false)
    };
    // Human-presence signal (issue #16): in mirror mode the agent and the human
    // share ONE Mac cursor — an L3 tap first yanks iPhone Mirroring frontmost,
    // stealing focus from whatever the human is doing. If Mirroring isn't
    // frontmost right now, a human/another app holds the Mac, so the next tap
    // WILL interrupt them. (Agent/WDA mode injects on-device → no contention,
    // so this is always false there.) Passive NSWorkspace read — no focus steal.
    #[cfg(target_os = "macos")]
    let human_active = !wda && drivable && !crate::macos::mirroring_is_frontmost();
    #[cfg(not(target_os = "macos"))]
    let human_active = false;
    // Version + update hint. `latest_release` is fetched by a background
    // task (24h cadence); compare as plain tags — any mismatch with the
    // running version means a release the binary doesn't match.
    let version = env!("CARGO_PKG_VERSION");
    let latest = recover(state.latest_release.lock()).clone();
    let (latest_json, update_available) = match &latest {
        Some(tag) => (
            format!(r#""{tag}""#),
            tag.trim_start_matches('v') != version,
        ),
        None => ("null".to_string(), false),
    };
    // Connected `/ws` viewers (active + queued) — issue #8.
    let viewer_count = recover(state.viewers.lock()).count();
    // When not drivable, tell the caller HOW to recover (the recovery differs by
    // state, and auto-recovery is blocked by macOS while the phone is in use).
    // Plain text only — kept free of quotes/braces so it drops into the JSON.
    let hint = if !drivable {
        match mirror_state {
            "paused" => "Mirroring paused — tap the Resume button to reconnect (a tap at x=0.5, y=0.64 hits it)",
            "in_use" => "iPhone in use — LOCK the phone to reconnect; the on-screen Connect button will not reconnect while it is in use",
            "offline" => "no iPhone Mirroring window — open iPhone Mirroring on the Mac, or start WebDriverAgent for on-device control",
            _ => "",
        }
    } else if human_active {
        // Issue #16: a human is on the Mac — yield instead of stealing focus.
        "a human is using the Mac (iPhone Mirroring is not frontmost) — an L3 tap will steal their focus; pause until they are idle, or switch to agent mode (on-device, no focus steal) via POST /agent/mode mode=agent"
    } else if !wda && state.wda.is_some() {
        // WDA was configured but the probe is down — the on-phone XCUITest runner
        // was almost certainly reaped by iOS (issue #14 §4). Spell out the
        // recovery, not just "no WDA".
        "WDA configured but unreachable (the on-phone runner was likely reaped) — taps/scroll still work but text typing is unreliable; restart WDA via POST /agent/mode mode=agent, then poll status for wda:true"
    } else if !wda {
        // No WDA configured at all. Taps/scroll land; text/key injection through
        // the mirror is unreliable (Mirroring does not forward synthetic
        // keystrokes — issue #15). Point the agent at the reliable path.
        "no WDA: taps/scroll work but text typing is unreliable through the mirror — for reliable typing start WDA via POST /agent/mode mode=agent (needs the phone unlocked once)"
    } else {
        ""
    };
    let body = format!(
        r#"{{"ok":true,"phone_target":{phone_target},"wda":{wda},"drivable":{drivable},"human_active":{human_active},"mode":"{mode}","mirror_state":"{mirror_state}","hint":"{hint}","viewer_count":{viewer_count},"version":"{version}","latest":{latest_json},"update_available":{update_available}}}"#
    );
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    with_security_headers(resp)
}

/// `POST /agent/mode` — switch between the two (mutually exclusive) control
/// modes. Body: `{"mode":"mirror"}` or `{"mode":"agent"}`.
///
/// The on-phone XCUITest runner (WDA, the L2 layer) monopolizes the device's
/// remote session: while it runs, iPhone Mirroring shows "Connection
/// Interrupted" and can never reconnect — even with the phone locked
/// (hardware A/B-verified, see docs/wda-setup.html pitfall ⑨). So L2 and
/// L3-video are switch MODES, not stacked layers, and this endpoint
/// orchestrates the switch:
///
/// * `mirror` — stop the WDA runner + relay (via `~/.iphone-use/setup-wda.sh
///   stop`, falling back to pkill), bring Mirroring frontmost, and tap its
///   "Try Again" button through the L3 injector. Returns once dispatched;
///   callers poll `/agent/status` for `"mode":"mirror"` and verify pixels.
/// * `agent` — spawn `~/.iphone-use/setup-wda.sh` detached (the script
///   self-installs there). WDA takes ~30-90s; poll `/agent/status` for
///   `"wda":true`. Mirroring will drop — expected.
async fn agent_mode(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    match agent_auth(&state, &headers) {
        AgentAuth::Locked => return with_security_headers(
            (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
        ),
        AgentAuth::Denied => return with_security_headers(
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        ),
        AgentAuth::Ok => {}
    }
    let mode = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let setup_sh = format!("{home}/.iphone-use/setup-wda.sh");
    match mode.as_str() {
        "mirror" => {
            // 0) Lock the phone via WDA while it's still alive — Mirroring
            //    can only connect to a LOCKED phone, so this makes the
            //    reconnect deterministic instead of depending on whatever
            //    state the agent left the phone in. Best-effort.
            if let Some(wda) = &state.wda {
                if let Err(e) = wda.lock().await.lock().await {
                    tracing::warn!("wda lock before mirror switch failed (continuing): {e:#}");
                }
            }
            // 1) Stop the runner + relay. Prefer the script (single source of
            //    truth for pidfiles); fall back to pkill patterns.
            let script = setup_sh.clone();
            let stopped = tokio::task::spawn_blocking(move || {
                let via_script = std::path::Path::new(&script).exists()
                    && std::process::Command::new("bash")
                        .arg(&script)
                        .arg("stop")
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                if !via_script {
                    for pat in ["xcodebuild.*WebDriverAgentRunner", "socat.*8100", "iproxy 8100"] {
                        let _ = std::process::Command::new("pkill").args(["-f", pat]).status();
                    }
                }
                via_script
            })
            .await
            .unwrap_or(false);
            // 2) Drop any cached WDA session — it's dead now.
            if let Some(wda) = &state.wda {
                wda.lock().await.invalidate_session();
            }
            // 3) Give the phone a moment to release the session, then bring
            //    Mirroring frontmost and tap "Try Again" (the button sits at
            //    ~(0.5, 0.65) of the Interrupted screen — hardware-verified;
            //    a stray tap there is harmless if already connected).
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open")
                    .args(["-a", "iPhone Mirroring"])
                    .status();
                tokio::task::spawn_blocking(|| {
                    crate::macos::ensure_mirroring_frontmost(std::time::Duration::from_secs(4))
                })
                .await
                .ok();
            }
            if let Some(ev) =
                crate::input_bridge::decode_control(r#"{"type":"tap","x":0.5,"y":0.65}"#)
            {
                {
                    let mut control = recover(state.control.lock());
                    let lease = control
                        .acquire(core::control::Holder::Agent("mode-switch".into()), now_secs());
                    *recover(state.current_lease.lock()) = Some(lease);
                }
                state.injector.send(ev);
            }
            let body = format!(
                r#"{{"ok":true,"mode":"mirror","switching":true,"stopped_via_script":{stopped}}}"#
            );
            with_security_headers(
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            )
        }
        "agent" => {
            if !std::path::Path::new(&setup_sh).exists() {
                return with_security_headers(
                    (
                        StatusCode::CONFLICT,
                        "setup-wda.sh not installed — run scripts/setup-wda.sh once manually \
                         (it self-installs to ~/.iphone-use/) before using mode=agent",
                    )
                        .into_response(),
                );
            }
            // Detached: the runner takes ~30-90s (xcodebuild). Log to a file
            // the operator can tail.
            let log = format!("{home}/.iphone-use/wda-mode-switch.log");
            let spawned = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("nohup bash '{setup_sh}' > '{log}' 2>&1 &"))
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            // The one thing automation cannot do: xcodebuild refuses to
            // launch the runner on a LOCKED phone, and Face ID can't be
            // faked. The spawned script waits patiently, so a single human
            // unlock completes the switch.
            let body = format!(
                r#"{{"ok":{spawned},"mode":"agent","starting":true,"log":"{log}","hint":"if the phone is locked, unlock it once now — the launcher waits for it"}}"#
            );
            with_security_headers(
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            )
        }
        _ => with_security_headers(
            (
                StatusCode::BAD_REQUEST,
                r#"body must be {"mode":"mirror"} or {"mode":"agent"}"#,
            )
                .into_response(),
        ),
    }
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
    match agent_auth(&state, &headers) {
        AgentAuth::Locked => return with_security_headers(
            (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
        ),
        AgentAuth::Denied => return with_security_headers(
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        ),
        AgentAuth::Ok => {}
    }
    // ── L2 auto-routing (WebDriverAgent) ──────────────────────────────────
    // When WDA is configured, prefer it for the actions where it is strictly
    // better: text goes in as Unicode (CJK lands cleanly instead of being
    // eaten by the on-phone IME), label-taps address elements directly, and
    // coordinate taps are synthesized on-device (no host-cursor contention,
    // no frontmost requirement). Any WDA failure falls back to the L3 pixel
    // path below — except label-taps, which L3 cannot express.
    if let Some(wda) = &state.wda {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let label = v.get("label").and_then(|l| l.as_str());
            match (typ, label) {
                // Dismiss the on-screen keyboard so it stops covering a page's
                // own submit/next buttons. `{"type":"keyboard"}` or
                // `{"type":"key","name":"dismiss"|"hide"}`. WDA-only — there is
                // no L3 equivalent, and a missing keyboard is a no-op, so this
                // always reports ok.
                ("keyboard", _) | ("key", _)
                    if typ == "keyboard"
                        || v.get("name")
                            .and_then(|n| n.as_str())
                            .is_some_and(|n| matches!(n, "dismiss" | "hide")) =>
                {
                    let _ = wda.lock().await.dismiss_keyboard().await;
                    return with_security_headers(
                        (StatusCode::OK, "ok (wda keyboard dismiss)").into_response(),
                    );
                }
                ("tap", Some(label)) => {
                    let r = wda.lock().await.click_label(label).await;
                    return match r {
                        Ok(()) => with_security_headers(
                            (StatusCode::OK, "ok (wda element)").into_response(),
                        ),
                        Err(e) => {
                            tracing::warn!("wda label tap '{label}' failed: {e:#}");
                            with_security_headers(
                                (StatusCode::BAD_GATEWAY, "wda: element not found")
                                    .into_response(),
                            )
                        }
                    };
                }
                ("text", None) => {
                    if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                        let mut w = wda.lock().await;
                        match w.keys(text).await {
                            Ok(()) => {
                                return with_security_headers(
                                    (StatusCode::OK, "ok (wda keys)").into_response(),
                                );
                            }
                            Err(e) => {
                                // One stale-session retry, then fall through to L3.
                                w.invalidate_session();
                                if w.keys(text).await.is_ok() {
                                    return with_security_headers(
                                        (StatusCode::OK, "ok (wda keys)").into_response(),
                                    );
                                }
                                tracing::warn!("wda keys failed, falling back to L3: {e:#}");
                            }
                        }
                    }
                }
                ("tap", None) => {
                    let (x, y) = (
                        v.get("x").and_then(|x| x.as_f64()),
                        v.get("y").and_then(|y| y.as_f64()),
                    );
                    if let (Some(x), Some(y)) = (x, y) {
                        let mut w = wda.lock().await;
                        let r = async {
                            let (sw, sh) = w.window_size().await?;
                            w.tap_point(x * sw, y * sh).await
                        }
                        .await;
                        match r {
                            Ok(()) => {
                                return with_security_headers(
                                    (StatusCode::OK, "ok (wda tap)").into_response(),
                                );
                            }
                            Err(e) => {
                                w.invalidate_session();
                                tracing::warn!("wda tap failed, falling back to L3: {e:#}");
                            }
                        }
                    }
                }
                _ => {} // scroll/key/shortcut/down/up → L3 (mirroring) below
            }
        }
    }

    let event = match crate::input_bridge::decode_control(&body) {
        Some(ev) => ev,
        None => {
            return with_security_headers(
                (StatusCode::BAD_REQUEST, "invalid control message").into_response(),
            );
        }
    };
    // Cooperative yield (issue #16): an agent that doesn't want to interrupt a
    // human sets `X-Yield-To-Human: 1`. This L3 path would yank iPhone Mirroring
    // frontmost and steal the Mac's focus — so if a human/another app currently
    // holds the foreground, refuse with 409 instead of barging in. Opt-in, so
    // default behavior is unchanged. (WDA-handled events returned earlier; their
    // on-device injection never contends, so only the L3 path is gated.)
    let yield_to_human = headers
        .get("x-yield-to-human")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.is_empty() && v != "0" && v != "false");
    #[cfg(target_os = "macos")]
    let mac_held_by_human = yield_to_human && !crate::macos::mirroring_is_frontmost();
    #[cfg(not(target_os = "macos"))]
    let mac_held_by_human = {
        let _ = yield_to_human;
        false
    };
    if mac_held_by_human {
        return with_security_headers(
            (
                StatusCode::CONFLICT,
                "yielded to human: iPhone Mirroring is not frontmost — retry when status human_active is false, or switch to agent mode",
            )
                .into_response(),
        );
    }
    // Take an Agent lease so the injector gate permits this event.
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("agent")
        .to_string();
    {
        let mut control = recover(state.control.lock());
        let lease = control.acquire(core::control::Holder::Agent(agent_id), now_secs());
        *recover(state.current_lease.lock()) = Some(lease);
    }
    state.injector.send(event);
    with_security_headers((StatusCode::OK, "ok").into_response())
}

/// `GET /agent/elements` — the phone's element tree, flattened to
/// agent-friendly rows `{kind, label, rect:[x,y,w,h], depth}` (L2 / WDA).
///
/// An agent reasons over this the way it reasons over a screenshot, but it's
/// text — an order of magnitude cheaper — and the labels feed straight back
/// into `POST /agent/input {"type":"tap","label":"…"}`. 503 when WDA is not
/// configured; 502 when it's configured but unreachable.
async fn agent_elements(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match agent_auth(&state, &headers) {
        AgentAuth::Locked => return with_security_headers(
            (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
        ),
        AgentAuth::Denied => return with_security_headers(
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        ),
        AgentAuth::Ok => {}
    }
    let Some(wda) = &state.wda else {
        return with_security_headers(
            (StatusCode::SERVICE_UNAVAILABLE, "wda not configured (PHONE_REMOTE_WDA_URL)")
                .into_response(),
        );
    };
    let mut w = wda.lock().await;
    let rows = match w.elements().await {
        Ok(rows) => rows,
        Err(_) => {
            // One stale-session retry.
            w.invalidate_session();
            match w.elements().await {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!("wda elements failed: {e:#}");
                    return with_security_headers(
                        (StatusCode::BAD_GATEWAY, "wda unreachable").into_response(),
                    );
                }
            }
        }
    };
    let body = serde_json::to_string(&serde_json::json!({ "elements": rows }))
        .unwrap_or_else(|_| r#"{"elements":[]}"#.to_string());
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    with_security_headers(resp)
}

/// `GET /agent/screenshot` — current phone screen as a PNG.
///
/// Captures the Mirroring window via [`core::capture::screenshot_mirroring_png`]
/// (uses `screencapture -l <window_id>` internally — no external cua-driver
/// dependency).  503 if the Mirroring window is not currently found.
///
/// Auth: agent bearer **or** a valid session cookie. The cookie path exists for
/// the web client's stills-fallback (when Mirroring dies the page polls this
/// endpoint) — a logged-in viewer already sees these pixels as video, so the
/// privilege is identical. The cookie is checked FIRST so browser polling never
/// touches the bearer auth-limiter (5 misses there lock the agent API for 30s).
async fn agent_screenshot(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Cookie counts only when a password is actually configured — in a
    // token-only deployment `is_authed` would otherwise wave everyone through
    // (it treats password=None as open mode).
    let cookie_ok = state.password.is_some() && is_authed(&state, &headers);
    if !cookie_ok {
        match agent_auth(&state, &headers) {
            AgentAuth::Locked => return with_security_headers(
                (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
            ),
            AgentAuth::Denied => return with_security_headers(
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            ),
            AgentAuth::Ok => {}
        }
    }
    let png = tokio::task::spawn_blocking(core::capture::screenshot_mirroring_png).await;
    // A valid L3 capture short-circuits. Otherwise — capture error, empty, OR a
    // runt/garbage frame (issue #14: ~26-byte non-PNG bodies came back during the
    // post-Resume screen transition and crashed the agent's decoder) — fall
    // through to the WDA on-device screenshot, then to a clear 503. Never hand
    // the agent half a frame with an image/png content-type.
    match &png {
        Ok(Ok(bytes)) if is_valid_png(bytes) => {
            let resp = Response::builder()
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(bytes.clone()))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            return with_security_headers(resp);
        }
        Ok(Ok(bytes)) => {
            // Non-empty but not a decodable PNG, or empty.
            tracing::warn!(
                "agent screenshot: L3 capture returned {} bytes, not a valid PNG — trying WDA",
                bytes.len()
            );
        }
        Ok(Err(e)) => {
            tracing::warn!("agent screenshot: no Mirroring window: {e:#} — trying WDA");
        }
        Err(e) => {
            tracing::warn!("agent screenshot: capture task panicked: {e:?} — trying WDA");
        }
    }
    // On-device fallback: works with no Mac-side window at all, and its bytes
    // come straight from WDA so they're a complete frame.
    if let Some(wda) = &state.wda {
        match wda.lock().await.screenshot_png().await {
            Ok(bytes) if is_valid_png(&bytes) => {
                let resp = Response::builder()
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                return with_security_headers(resp);
            }
            Ok(bytes) => tracing::warn!(
                "agent screenshot: WDA returned {} bytes, not a valid PNG",
                bytes.len()
            ),
            Err(e) => tracing::warn!("agent screenshot: WDA screenshot failed: {e:#}"),
        }
    }
    with_security_headers(
        (StatusCode::SERVICE_UNAVAILABLE, "no valid screenshot frame available").into_response(),
    )
}

/// True when `bytes` is a plausibly-decodable PNG: the 8-byte signature plus
/// enough length to carry an IHDR. Guards the agent's decoder against the
/// runt/garbage frames the Mirroring capture can emit mid-transition (issue #14).
fn is_valid_png(bytes: &[u8]) -> bool {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    // 8 sig + 4 len + 4 "IHDR" + 13 IHDR data + 4 CRC = 33 minimum.
    bytes.len() >= 33 && bytes.starts_with(&PNG_SIG)
}

/// `POST /agent/inbox` — the phone (an iOS Shortcut) delivers a structured result.
///
/// Body is arbitrary JSON; it's stored with a receive timestamp for an agent to
/// GET. Bearer-auth'd (the shortcut carries the token), so only the trusted phone
/// can write. Returns 200 `accepted`. This is the return half of the Shortcuts
/// RPC bridge — the daemon triggers a shortcut by name (Spotlight), the shortcut
/// runs a native iOS action and POSTs its result here.
async fn agent_inbox_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    match agent_auth(&state, &headers) {
        AgentAuth::Locked => {
            return with_security_headers(
                (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
            )
        }
        AgentAuth::Denied => {
            return with_security_headers(
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            )
        }
        AgentAuth::Ok => {}
    }
    // Accept any JSON; if the shortcut sent a bare string / non-JSON, wrap it.
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| serde_json::Value::String(body.clone()));
    {
        let mut inbox = state.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.push_back(InboxItem {
            received_at: now_secs(),
            body: value,
        });
        while inbox.len() > INBOX_CAP {
            inbox.pop_front();
        }
    }
    with_security_headers((StatusCode::OK, "accepted").into_response())
}

/// `GET /agent/inbox` — an agent retrieves and DRAINS pending phone results.
///
/// Returns `{"items":[{"received_at":..,"body":..}, ...]}` and empties the inbox
/// (use `?peek=1` to read without draining). Bearer-auth'd.
async fn agent_inbox_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    match agent_auth(&state, &headers) {
        AgentAuth::Locked => {
            return with_security_headers(
                (StatusCode::TOO_MANY_REQUESTS, "too many attempts").into_response(),
            )
        }
        AgentAuth::Denied => {
            return with_security_headers(
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            )
        }
        AgentAuth::Ok => {}
    }
    let peek = query.as_deref().is_some_and(|q| q.contains("peek=1"));
    let items: Vec<InboxItem> = {
        let mut inbox = state.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if peek {
            inbox.iter().cloned().collect()
        } else {
            inbox.drain(..).collect()
        }
    };
    let json = serde_json::json!({ "items": items }).to_string();
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    with_security_headers(resp)
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

    // ── AuthLimiter unit tests ────────────────────────────────────────────────

    #[test]
    fn auth_limiter_not_locked_initially() {
        let limiter = AuthLimiter::new();
        assert!(!limiter.is_locked());
        assert_eq!(limiter.failures, 0);
    }

    #[test]
    fn auth_limiter_locks_after_max_failures() {
        let mut limiter = AuthLimiter::new();
        for _ in 0..AUTH_MAX_FAILURES {
            assert!(!limiter.is_locked(), "should not lock before reaching max");
            limiter.record_failure();
        }
        assert!(limiter.is_locked(), "should be locked after max failures");
    }

    #[test]
    fn auth_limiter_four_failures_not_locked() {
        let mut limiter = AuthLimiter::new();
        for _ in 0..(AUTH_MAX_FAILURES - 1) {
            limiter.record_failure();
        }
        assert!(!limiter.is_locked(), "4 failures should not trigger lockout (max=5)");
    }

    #[test]
    fn auth_limiter_success_resets_counter_and_lifts_lockout() {
        let mut limiter = AuthLimiter::new();
        for _ in 0..AUTH_MAX_FAILURES {
            limiter.record_failure();
        }
        assert!(limiter.is_locked());
        limiter.record_success();
        assert!(!limiter.is_locked(), "success should lift active lockout");
        assert_eq!(limiter.failures, 0, "success should reset failure counter");
    }

    #[test]
    fn auth_limiter_lockout_expires_after_duration() {
        let mut limiter = AuthLimiter::new();
        // Manually set a lockout that already expired.
        limiter.failures = AUTH_MAX_FAILURES;
        limiter.locked_until = Some(Instant::now() - std::time::Duration::from_secs(1));
        assert!(!limiter.is_locked(), "expired lockout should not block requests");
    }

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
        assert!(INDEX_HTML.contains("iphone-use"));
        assert!(INDEX_HTML.contains("/ws"));
        assert!(INDEX_HTML.contains("turn-creds"));
    }

    #[test]
    fn png_validation_rejects_runt_and_garbage_frames() {
        let sig = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        // The issue-#14 failure mode: a short non-PNG body.
        assert!(!is_valid_png(&[0u8; 26]));
        assert!(!is_valid_png(&[]));
        // Right signature but too short to hold an IHDR.
        assert!(!is_valid_png(&sig));
        // Right length but wrong magic (e.g. a JPEG or HTML error page).
        assert!(!is_valid_png(&[0xffu8; 64]));
        // A minimal well-formed-enough PNG header passes.
        let mut ok = sig.to_vec();
        ok.extend_from_slice(&[0u8; 25]); // pad past the 33-byte floor
        assert!(is_valid_png(&ok));
    }
}
