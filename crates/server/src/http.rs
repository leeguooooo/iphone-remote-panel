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
    /// WDA's on-device MJPEG stream URL (e.g. `http://127.0.0.1:9100`), if WDA
    /// is configured. The `/agent/mjpeg` endpoint proxies it so agent mode gets
    /// LIVE video without iPhone Mirroring — the MJPEG server runs inside the
    /// same XCUITest session as control, so the two coexist (Mirroring can't).
    /// Defaults to `127.0.0.1:9100` (the relay target), override via
    /// `PHONE_REMOTE_WDA_MJPEG_URL`.
    pub mjpeg_url: Option<String>,
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
        .route("/agent/mjpeg", get(agent_mjpeg))
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
    // L2 health — action-level, not just /status (which lies: it reports
    // `ready` even when every UI action fails Code=41 because the phone is
    // locked or the test session was severed). `wda` stays "runner reachable"
    // for back-compat; `wda_actionable` is the honest "can it act right now".
    let health = match &state.wda {
        Some(w) => w.lock().await.probe_health().await,
        None => crate::wda::WdaHealth::down(),
    };
    let wda = health.up;
    let wda_actionable = health.actionable;
    let wda_locked = match health.locked {
        Some(true) => "true",
        Some(false) => "false",
        None => "null",
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
        // WDA injects on-device regardless of the mirror window — but only if
        // it can actually act. A "zombie ready" runner (locked / severed) is up
        // yet undrivable, so gate drivable on the action-level probe.
        ("active", wda_actionable)
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
    let hint = if wda && !wda_actionable {
        // "Zombie ready": runner answers /status but UI actions fail Code=41.
        // Almost always the phone is locked/asleep; otherwise the test session
        // was severed (sleep / WARP toggle / CoreDevice tunnel).
        if wda_locked == "true" {
            "WDA is up but the phone is LOCKED — XCUITest cannot act on a locked screen (every action fails Code=41). Unlock the phone and keep it awake (set Auto-Lock to Never for long agent sessions)."
        } else {
            "WDA answers /status but cannot perform UI actions (Code=41) — the test session was severed (phone sleep / WARP toggle / CoreDevice tunnel), a 'zombie ready' runner. Restart WDA via POST /agent/mode mode=agent (with the phone unlocked and awake)."
        }
    } else if !drivable {
        match mirror_state {
            "paused" => "Mirroring needs reconnecting (paused / interrupted / timed out) — tap the Resume/Connect/Try Again button (x=0.5, y=0.64), once, then wait 45s+; do NOT loop",
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
    // Setup progress: `setup-wda.sh` writes ~/.iphone-use/wda-setup-status.json
    // ({phase, blocked_on, message, ts}) as it runs. Surface `setup_blocked_on`
    // so a caller (or POST /agent/mode) knows WHY a WDA bring-up is stuck —
    // "warp" | "usb" | "trust" | "ddi" | "" — instead of polling wda:false
    // blind. Only honored while fresh (< 5 min) so a stale file isn't reported.
    let setup_blocked_on = read_setup_blocked_on();
    let body = format!(
        r#"{{"ok":true,"phone_target":{phone_target},"wda":{wda},"wda_actionable":{wda_actionable},"wda_locked":{wda_locked},"drivable":{drivable},"human_active":{human_active},"mode":"{mode}","mirror_state":"{mirror_state}","hint":"{hint}","setup_blocked_on":"{setup_blocked_on}","viewer_count":{viewer_count},"version":"{version}","latest":{latest_json},"update_available":{update_available}}}"#
    );
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    with_security_headers(resp)
}

/// Read `blocked_on` from `setup-wda.sh`'s status file, but only if it was
/// written in the last 5 minutes (a stale file from a finished run shouldn't be
/// reported as a live blocker). Returns "" when absent/stale/unparseable —
/// best-effort, never errors.
fn read_setup_blocked_on() -> String {
    let path = match std::env::var("HOME") {
        Ok(h) => format!("{h}/.iphone-use/wda-setup-status.json"),
        Err(_) => return String::new(),
    };
    let txt = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return String::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
    if now_secs().saturating_sub(ts) > 300 {
        return String::new();
    }
    v.get("blocked_on")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string()
}

/// launchd label for the dedicated, self-healing WDA job.
const WDA_AGENT_LABEL: &str = "com.leeguoo.iphone-use.wda";

/// Current GUI launchd domain (`gui/<uid>`), via `id -u`.
fn gui_domain() -> String {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    format!("gui/{uid}")
}

/// Write the WDA LaunchAgent plist and (re)bootstrap it. Running WDA as its OWN
/// launchd job — `KeepAlive=true`, in the user's GUI domain, NOT this daemon's
/// cgroup — makes it (a) survive daemon restarts and (b) auto-restart when the
/// runner dies (WARP reconnect / sleep / USB hiccup). `ThrottleInterval` caps
/// the rebuild rate so a persistent killer thrashes harmlessly. Returns whether
/// the bootstrap succeeded.
fn write_and_bootstrap_wda_agent(home: &str, setup_sh: &str, log: &str, udid: &str) -> bool {
    let plist_path = format!("{home}/Library/LaunchAgents/{WDA_AGENT_LABEL}.plist");
    let udid_kv = if udid.is_empty() {
        String::new()
    } else {
        format!("        <key>WDA_UDID</key><string>{udid}</string>\n")
    };
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>{WDA_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>/bin/bash</string><string>{setup_sh}</string></array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>WDA_KEEPALIVE</key><string>1</string>
        <key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
{udid_kv}    </dict>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>RunAtLoad</key><true/>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#
    );
    if std::fs::write(&plist_path, plist).is_err() {
        return false;
    }
    let domain = gui_domain();
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{WDA_AGENT_LABEL}")])
        .status();
    std::process::Command::new("launchctl")
        .args(["bootstrap", &domain, &plist_path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Boot out the WDA LaunchAgent (so its KeepAlive stops rebuilding the runner).
/// Best-effort; ignored if it isn't loaded.
fn bootout_wda_agent() {
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{}/{WDA_AGENT_LABEL}", gui_domain())])
        .status();
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
    // Cookie OR bearer (same gate as screenshot/status) so the web client's
    // "Reconnect" button can drive the mode switch without the agent token.
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
    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
    let mode = parsed
        .as_ref()
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_default();
    // Optional target UDID — drive a SPECIFIC paired phone (not just the
    // Mirroring one). Passed to setup-wda.sh as WDA_UDID. Sanitized to the
    // hex/dash charset so it can't inject into the spawned shell command.
    let udid = parsed
        .as_ref()
        .and_then(|v| v.get("udid").and_then(|u| u.as_str()))
        .filter(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .map(String::from);
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
            // 1) Stop the WDA LaunchAgent FIRST (else its KeepAlive would just
            //    rebuild the runner we're about to kill), then the runner +
            //    relay via the script (single source of truth for pidfiles),
            //    falling back to pkill.
            let script = setup_sh.clone();
            let stopped = tokio::task::spawn_blocking(move || {
                bootout_wda_agent();
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
            // Run WDA under a DEDICATED LaunchAgent (com.leeguoo.iphone-use.wda)
            // with KeepAlive instead of nohup-spawning a child. Two reasons,
            // both hardware-painful bugs:
            //   1. A nohup child lives in THIS daemon's launchd cgroup, so the
            //      next daemon restart (`launchctl bootout`) reaps the runner —
            //      WDA "randomly" died on every redeploy.
            //   2. When the runner dies (WARP reconnect kills the CoreDevice
            //      tunnel, sleep, USB hiccup) nothing brought it back. KeepAlive
            //      relaunches it; setup-wda.sh's WDA_KEEPALIVE mode blocks until
            //      the runner dies so launchd sees the exit and rebuilds.
            // ThrottleInterval caps the rebuild rate so a persistent killer
            // (WARP Always-On) thrashes harmlessly instead of hot-looping.
            let log = format!("{home}/.iphone-use/wda-agent.log");
            let udid_env = udid.as_deref().unwrap_or("");
            let spawned = write_and_bootstrap_wda_agent(&home, &setup_sh, &log, udid_env);
            let body = format!(
                r#"{{"ok":{spawned},"mode":"agent","starting":true,"self_healing":true,"log":"{log}","hint":"if the phone is locked, unlock it once now — the launcher waits for it; WDA now auto-restarts if it drops"}}"#
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

/// Perform a WDA on-device scroll/swipe (issue #27). `nx`/`ny` are the
/// normalized `[0,1]` gesture anchor; `dx`/`dy` are scroll deltas whose sign
/// matches the L3 convention (positive `dy` reveals content below, positive
/// `dx` reveals content to the right). The delta is scaled into a finger travel
/// that is always a visible swipe (≥15% of the axis) yet stays on-screen (≤75%);
/// the finger moves opposite to the content reveal.
pub(crate) async fn wda_swipe(
    w: &mut crate::wda::WdaClient,
    nx: f64,
    ny: f64,
    dx: f64,
    dy: f64,
) -> anyhow::Result<()> {
    let travel = |d: f64, axis: f64| -> f64 {
        if d == 0.0 {
            0.0
        } else {
            (d.abs() * 1.5).clamp(0.15 * axis, 0.75 * axis) * d.signum()
        }
    };
    let (sw, sh) = w.window_size().await?;
    let cx = (nx * sw).clamp(1.0, sw - 1.0);
    let cy = (ny * sh).clamp(1.0, sh - 1.0);
    let tx = travel(dx, sw);
    let ty = travel(dy, sh);
    let x1 = (cx + tx / 2.0).clamp(1.0, sw - 1.0);
    let x2 = (cx - tx / 2.0).clamp(1.0, sw - 1.0);
    let y1 = (cy + ty / 2.0).clamp(1.0, sh - 1.0);
    let y2 = (cy - ty / 2.0).clamp(1.0, sh - 1.0);
    let dist = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
    let dur = (dist * 1.2).clamp(120.0, 600.0) as u64;
    w.swipe(x1, y1, x2, y2, dur).await
}

/// Resolve the CoreDevice identifier of the currently-connected iPhone by
/// parsing `devicectl list devices` (the `connected` row). Needed because
/// `devicectl` requires an explicit `--device` and the daemon doesn't otherwise
/// track the UDID.
#[cfg(target_os = "macos")]
fn detect_connected_device() -> Option<String> {
    let out = std::process::Command::new("xcrun")
        .args(["devicectl", "list", "devices"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // States seen: "connected", "available (paired)", "unavailable".
        // Only the live one contains the bare word "connected".
        if line.contains("connected") {
            for tok in line.split_whitespace() {
                // CoreDevice identifier is a 36-char UUID (8-4-4-4-12).
                if tok.len() == 36 && tok.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                    return Some(tok.to_string());
                }
            }
        }
    }
    None
}

/// Uninstall an app (and its data container) from a paired device via
/// CoreDevice. This is the reliable "Delete App" primitive: WDA cannot remove
/// apps, and UI-driven deletion (Settings → Storage, or a home-screen
/// long-press) is flaky to automate. `udid` defaults to the connected device.
#[cfg(target_os = "macos")]
fn devicectl_uninstall(udid: Option<&str>, bundle: &str) -> Result<(), String> {
    let device = match udid {
        Some(u) => u.to_string(),
        None => detect_connected_device().ok_or_else(|| "no connected device".to_string())?,
    };
    let out = std::process::Command::new("xcrun")
        .args(["devicectl", "device", "uninstall", "app", "--device", &device, bundle])
        .output()
        .map_err(|e| format!("spawn devicectl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Route a web-client control message (the WebRTC `control` data-channel JSON)
/// to WDA when it's up, so the BROWSER drives the phone on-device in agent mode
/// — like `/agent/input`, but for the live data channel. Returns true if WDA
/// handled it; false → the caller falls back to the L3 (mirror) injector, which
/// drives whatever the Mac mirrors and steals Mac focus. Covers the common
/// interactions (tap/scroll/text/home); rarer events (drag down/up,
/// spotlight/switcher, key) fall through to L3.
pub(crate) async fn wda_control_from_json(
    wda: &Arc<tokio::sync::Mutex<crate::wda::WdaClient>>,
    v: &serde_json::Value,
) -> bool {
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let mut w = wda.lock().await;
    let r: anyhow::Result<()> = match typ {
        "tap" | "longpress" => {
            match (
                v.get("x").and_then(|x| x.as_f64()),
                v.get("y").and_then(|y| y.as_f64()),
            ) {
                (Some(x), Some(y)) => {
                    async {
                        let (sw, sh) = w.window_size().await?;
                        w.tap_point(x * sw, y * sh).await
                    }
                    .await
                }
                _ => return false,
            }
        }
        "scroll" => {
            let nx = v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.5);
            let ny = v.get("y").and_then(|y| y.as_f64()).unwrap_or(0.5);
            let dx = v.get("dx").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let dy = v.get("dy").and_then(|y| y.as_f64()).unwrap_or(0.0);
            if dx == 0.0 && dy == 0.0 {
                return false;
            }
            wda_swipe(&mut w, nx, ny, dx, dy).await
        }
        "text" => match v.get("text").and_then(|t| t.as_str()) {
            Some(t) => w.keys(t).await,
            None => return false,
        },
        "shortcut" if v.get("name").and_then(|n| n.as_str()) == Some("home") => {
            w.press_home().await
        }
        _ => return false,
    };
    match r {
        Ok(()) => true,
        Err(e) => {
            w.invalidate_session();
            tracing::warn!("wda data-channel control ({typ}): {e:#}");
            false
        }
    }
}

/// `POST /agent/input` — inject one control message (same JSON shape as the
/// WebRTC control channel): `{"type":"tap","x":0.5,"y":0.5}`,
/// `{"type":"text","text":"hi"}`, `{"type":"scroll","x":..,"y":..,"dx":..,"dy":..}`,
/// `{"type":"shortcut","name":"home"}`, `{"type":"key","name":"return"}`,
/// `{"type":"uninstall","bundle":"com.example.app"}` (via devicectl), etc.
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
    // App uninstall via CoreDevice (`devicectl`) — WDA can't remove apps and
    // UI-driven deletion is unreliable to automate, so this is the dependable
    // "Delete App (with data)" primitive (e.g. resetting a wedged app to its
    // login state). `{"type":"uninstall","bundle":"com.example.app"}`; optional
    // `"udid"` targets a specific paired phone, else the connected one is used.
    // Destructive — gated behind agent auth like every action here.
    #[cfg(target_os = "macos")]
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
        if v.get("type").and_then(|t| t.as_str()) == Some("uninstall") {
            let bundle = v.get("bundle").and_then(|b| b.as_str()).unwrap_or("");
            // Bundle ids are reverse-DNS — letters/digits/dot/hyphen only.
            // Reject anything else so it can't inject into the spawned command.
            let bundle_ok = !bundle.is_empty()
                && bundle.len() <= 200
                && bundle
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
            if !bundle_ok {
                return with_security_headers(
                    (
                        StatusCode::BAD_REQUEST,
                        "uninstall needs a valid \"bundle\" (reverse-DNS id)",
                    )
                        .into_response(),
                );
            }
            let udid = v
                .get("udid")
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
                .map(String::from);
            let bundle = bundle.to_string();
            let r = tokio::task::spawn_blocking(move || {
                devicectl_uninstall(udid.as_deref(), &bundle)
            })
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
            return match r {
                Ok(()) => {
                    with_security_headers((StatusCode::OK, "ok (uninstalled)").into_response())
                }
                Err(e) => {
                    tracing::warn!("devicectl uninstall failed: {e}");
                    with_security_headers(
                        (StatusCode::BAD_GATEWAY, format!("uninstall failed: {e}"))
                            .into_response(),
                    )
                }
            };
        }
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
                // Open an app by bundle id (issue #18-A): Home-Screen icons report
                // rect [0,0,0,0] and don't navigate on tap, so launching by bundle
                // is the only reliable way to open a system app. Accepts an
                // explicit `{"bundle":"com.apple.Preferences"}` or a `{"name":"设置"}`
                // mapped from the common-app table below.
                ("launch_app", _) => {
                    let bundle = v
                        .get("bundle")
                        .and_then(|b| b.as_str())
                        .map(str::to_string)
                        .or_else(|| {
                            v.get("name")
                                .and_then(|n| n.as_str())
                                .and_then(system_app_bundle)
                                .map(str::to_string)
                        });
                    return match bundle {
                        Some(b) => match wda.lock().await.launch_app(&b).await {
                            Ok(()) => with_security_headers(
                                (StatusCode::OK, "ok (wda launch)").into_response(),
                            ),
                            Err(e) => {
                                tracing::warn!("wda launch_app '{b}' failed: {e:#}");
                                with_security_headers(
                                    (StatusCode::BAD_GATEWAY, "wda: launch failed")
                                        .into_response(),
                                )
                            }
                        },
                        None => with_security_headers(
                            (
                                StatusCode::BAD_REQUEST,
                                "launch_app needs \"bundle\":\"<id>\" (or a known \"name\")",
                            )
                                .into_response(),
                        ),
                    };
                }
                // Tap the Nth element from /agent/elements by its rect center
                // (#24.1) — for when labels are non-unique/opaque. WDA-only,
                // on-device. `{"type":"tap","element":N}`.
                ("tap", None) if v.get("element").and_then(|e| e.as_u64()).is_some() => {
                    let idx = v.get("element").and_then(|e| e.as_u64()).unwrap() as usize;
                    let mut w = wda.lock().await;
                    let r = async {
                        let rows = w.elements().await?;
                        let row = rows.get(idx).ok_or_else(|| {
                            anyhow::anyhow!("element {idx} out of range ({} on screen)", rows.len())
                        })?;
                        let (cx, cy) = (row.rect[0] + row.rect[2] / 2.0, row.rect[1] + row.rect[3] / 2.0);
                        w.tap_point(cx, cy).await
                    }
                    .await;
                    return match r {
                        Ok(()) => with_security_headers(
                            (StatusCode::OK, "ok (wda element tap)").into_response(),
                        ),
                        Err(e) => {
                            w.invalidate_session();
                            tracing::warn!("wda element tap [{idx}] failed: {e:#}");
                            with_security_headers(
                                (StatusCode::BAD_GATEWAY, format!("wda element tap: {e}"))
                                    .into_response(),
                            )
                        }
                    };
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
                // Go to the Home screen on-device (`{"type":"home"}`). WDA-only
                // so it works in agent mode; the L3 `shortcut` path needs the
                // mirror frontmost.
                ("home", _) => {
                    let mut w = wda.lock().await;
                    if w.press_home().await.is_ok() {
                        return with_security_headers(
                            (StatusCode::OK, "ok (wda home)").into_response(),
                        );
                    }
                    // One stale-session retry (the session can expire / be
                    // reclaimed between calls).
                    w.invalidate_session();
                    return match w.press_home().await {
                        Ok(()) => with_security_headers(
                            (StatusCode::OK, "ok (wda home)").into_response(),
                        ),
                        Err(e) => {
                            w.invalidate_session();
                            tracing::warn!("wda home failed: {e:#}");
                            with_security_headers(
                                (StatusCode::BAD_GATEWAY, format!("wda home: {e}")).into_response(),
                            )
                        }
                    };
                }
                // Navigate back via the universal iOS left-edge swipe
                // (`{"type":"back"}`). Reliable across apps, unlike a nav-bar
                // button whose label/position varies.
                ("back", _) => {
                    let mut w = wda.lock().await;
                    if w.back().await.is_ok() {
                        return with_security_headers(
                            (StatusCode::OK, "ok (wda back)").into_response(),
                        );
                    }
                    w.invalidate_session();
                    return match w.back().await {
                        Ok(()) => with_security_headers(
                            (StatusCode::OK, "ok (wda back)").into_response(),
                        ),
                        Err(e) => {
                            w.invalidate_session();
                            tracing::warn!("wda back failed: {e:#}");
                            with_security_headers(
                                (StatusCode::BAD_GATEWAY, format!("wda back: {e}")).into_response(),
                            )
                        }
                    };
                }
                ("text", None) => {
                    if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                        let mut w = wda.lock().await;
                        // `{"type":"text","clear":true}` empties the focused
                        // field first so the text REPLACES rather than appends
                        // (the "ClaudeClaude" search-box bug). Best-effort — a
                        // failed clear still lets the type proceed.
                        if v.get("clear").and_then(|c| c.as_bool()).unwrap_or(false) {
                            if let Err(e) = w.clear_active().await {
                                tracing::warn!("wda clear_active before text: {e:#}");
                            }
                        }
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
                // Set a date/option PickerWheel — WDA-only (a scroll gesture
                // can't move it; issue #23). `{"type":"picker","column":N,
                // "value":"March"}`. No L3 fallback (the mirror can't express
                // adjustToPickerWheelValue).
                ("picker", _) => {
                    let column = v.get("column").and_then(|c| c.as_u64()).unwrap_or(0) as usize;
                    let value = v.get("value").and_then(|x| x.as_str());
                    return match value {
                        Some(value) => {
                            let mut w = wda.lock().await;
                            match w.set_picker(column, value).await {
                                Ok(()) => with_security_headers(
                                    (StatusCode::OK, "ok (wda picker)").into_response(),
                                ),
                                Err(e) => {
                                    w.invalidate_session();
                                    tracing::warn!("wda set_picker col={column} '{value}': {e:#}");
                                    with_security_headers(
                                        (StatusCode::BAD_GATEWAY, format!("wda picker: {e}"))
                                            .into_response(),
                                    )
                                }
                            }
                        }
                        None => with_security_headers(
                            (
                                StatusCode::BAD_REQUEST,
                                "picker needs \"value\":\"<target>\" (and optional \"column\":N, 0-based)",
                            )
                                .into_response(),
                        ),
                    };
                }
                // Scroll/swipe on-device via WDA (issue #27) — like tap/text,
                // independent of whether the Mirroring window is frontmost.
                // `{"type":"scroll","x":0.5,"y":0.5,"dy":300}` (x/y optional,
                // default screen center). Sign matches the L3 convention:
                // positive `dy` reveals content further down (finger swipes up),
                // positive `dx` reveals content to the right. On WDA error we
                // reset the session, retry once, then fall through to L3.
                ("scroll", _) | ("swipe", _) => {
                    let nx = v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.5);
                    let ny = v.get("y").and_then(|y| y.as_f64()).unwrap_or(0.5);
                    let dx = v.get("dx").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let dy = v.get("dy").and_then(|y| y.as_f64()).unwrap_or(0.0);
                    if dx != 0.0 || dy != 0.0 {
                        let mut w = wda.lock().await;
                        match wda_swipe(&mut w, nx, ny, dx, dy).await {
                            Ok(()) => {
                                return with_security_headers(
                                    (StatusCode::OK, "ok (wda swipe)").into_response(),
                                );
                            }
                            Err(e) => {
                                // One stale-session retry, then fall through to L3.
                                w.invalidate_session();
                                if wda_swipe(&mut w, nx, ny, dx, dy).await.is_ok() {
                                    return with_security_headers(
                                        (StatusCode::OK, "ok (wda swipe)").into_response(),
                                    );
                                }
                                tracing::warn!("wda swipe failed, falling back to L3: {e:#}");
                            }
                        }
                    }
                }
                _ => {} // key/shortcut/down/up → L3 (mirroring) below
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
    // Deliverability check (issue #25): an L3 event only lands if iPhone
    // Mirroring can be brought frontmost. When a human is on the Mac, macOS
    // refuses to let a background LaunchAgent steal focus, so the event is
    // silently dropped — and returning "ok" makes an agent loop blindly. Bring
    // it frontmost up front; if that fails, report the drop instead of lying.
    #[cfg(target_os = "macos")]
    {
        let delivered = tokio::task::spawn_blocking(|| {
            crate::macos::ensure_mirroring_frontmost(std::time::Duration::from_millis(1200))
        })
        .await
        .unwrap_or(false);
        if !delivered {
            return with_security_headers(
                Response::builder()
                    .status(StatusCode::CONFLICT)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"ok":false,"dropped":true,"reason":"iPhone Mirroring could not be brought frontmost (a human is using the Mac, or it is paused/in-use) — poll /agent/status until human_active is false and drivable is true, or switch to agent mode (POST /agent/mode mode=agent)"}"#,
                    ))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            );
        }
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
    // Always answer with parseable JSON so a client's `r.json()["elements"]`
    // never throws — even when WDA is absent or mid-transition (issue #18-B:
    // a non-JSON 502/503 body crashed agents' decoders, same class as #15-C).
    let json_body = |body: String| {
        with_security_headers(
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        )
    };
    let Some(wda) = &state.wda else {
        return json_body(
            r#"{"elements":[],"error":"wda not configured (PHONE_REMOTE_WDA_URL)"}"#.to_string(),
        );
    };
    let mut w = wda.lock().await;
    let rows = match w.elements().await {
        Ok(rows) => rows,
        Err(_) => {
            // One stale-session retry, then degrade gracefully: an empty set with
            // `transitioning:true` (WDA is mid-screen-change or briefly down) lets
            // the agent retry instead of crashing on an unparseable body.
            w.invalidate_session();
            match w.elements().await {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!("wda elements failed: {e:#}");
                    return json_body(r#"{"elements":[],"transitioning":true}"#.to_string());
                }
            }
        }
    };
    // Screen size so the agent can normalize the point-space rects to [0,1]
    // instead of guessing the device dimensions from the largest rect (#24.2).
    // The array index of each row is its stable handle for `{"type":"tap",
    // "element":<i>}` (#24.1) — useful when labels are non-unique/opaque.
    let screen = w.window_size().await.ok();
    json_body(
        serde_json::to_string(&serde_json::json!({
            "screen": screen.map(|(width, height)| serde_json::json!({"width": width, "height": height})),
            "elements": rows,
        }))
        .unwrap_or_else(|_| r#"{"elements":[]}"#.to_string()),
    )
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
    // Prefer the WDA on-device capture whenever WDA is configured. In agent mode
    // the Mirroring window only shows the "iPhone in Use" interstitial (WDA
    // monopolizes the device), and when WDA targets a *second* phone the mirror
    // is a different device entirely — so the L3 mirror bytes would be the WRONG
    // screen. WDA bytes are always the actual target phone. Falls through to the
    // L3 mirror capture when WDA is absent or its runner is down (the failed
    // call doubles as the liveness check, so no extra /status round-trip).
    if let Some(wda) = &state.wda {
        if let Ok(bytes) = wda.lock().await.screenshot_png().await {
            if is_valid_png(&bytes) {
                let resp = Response::builder()
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                return with_security_headers(resp);
            }
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

/// `GET /agent/mjpeg` — LIVE video in agent mode by proxying WDA's on-device
/// MJPEG stream (`multipart/x-mixed-replace`). The MJPEG server runs inside the
/// same XCUITest session as control, so video and driving coexist — unlike
/// iPhone Mirroring, which is mutually exclusive with WDA. A browser renders
/// this directly in an `<img src="/agent/mjpeg">`. ~28 fps at the tuned
/// settings applied here (framerate/scaling/quality), regardless of USB vs Wi-Fi
/// (the cap is WDA's screenshot rate, not the transport).
async fn agent_mjpeg(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Same cookie-or-bearer rule as `agent_screenshot`.
    let cookie_ok = state.password.is_some() && is_authed(&state, &headers);
    if !cookie_ok {
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
    }
    let Some(url) = state.mjpeg_url.clone() else {
        return with_security_headers(
            (StatusCode::SERVICE_UNAVAILABLE, "no WDA MJPEG configured").into_response(),
        );
    };
    // Best-effort: tune the stream for a smooth feed (idempotent). A failure
    // here just leaves WDA's defaults (~9 fps) — still usable.
    if let Some(wda) = &state.wda {
        let _ = wda.lock().await.set_mjpeg_settings(30, 50, 60).await;
    }
    // Proxy the upstream MJPEG stream straight through. A fresh client with no
    // timeout — the stream is intentionally long-lived (one frame after another
    // forever), so a request timeout would cut it off.
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match client.get(&url).send().await {
        Ok(up) if up.status().is_success() => {
            let content_type = up
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("multipart/x-mixed-replace; boundary=--BoundaryString")
                .to_string();
            let body = Body::from_stream(up.bytes_stream());
            Response::builder()
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CACHE_CONTROL, "no-store")
                .body(body)
                .map(with_security_headers)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(up) => with_security_headers(
            (
                StatusCode::BAD_GATEWAY,
                format!("WDA MJPEG upstream {}", up.status()),
            )
                .into_response(),
        ),
        Err(e) => {
            tracing::warn!("mjpeg proxy to {url} failed: {e}");
            with_security_headers(
                (
                    StatusCode::BAD_GATEWAY,
                    "WDA MJPEG unreachable — is the :9100 relay up? (restart WDA via mode=agent)",
                )
                    .into_response(),
            )
        }
    }
}

/// True when `bytes` is a plausibly-decodable PNG: the 8-byte signature plus
/// enough length to carry an IHDR. Guards the agent's decoder against the
/// runt/garbage frames the Mirroring capture can emit mid-transition (issue #14).
fn is_valid_png(bytes: &[u8]) -> bool {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    // 8 sig + 4 len + 4 "IHDR" + 13 IHDR data + 4 CRC = 33 minimum.
    bytes.len() >= 33 && bytes.starts_with(&PNG_SIG)
}

/// Map a friendly app name (zh or en) to its iOS bundle id, for
/// `{"type":"launch_app","name":…}` (issue #18-A). Unknown names return `None`
/// — the caller can always pass an explicit `bundle`. Covers the stock apps an
/// agent most often needs to reach.
fn system_app_bundle(name: &str) -> Option<&'static str> {
    Some(match name.trim() {
        "设置" | "设定" | "Settings" | "settings" => "com.apple.Preferences",
        "照片" | "Photos" | "photos" => "com.apple.mobileslideshow",
        "相机" | "Camera" | "camera" => "com.apple.camera",
        "时钟" | "Clock" | "clock" => "com.apple.mobiletimer",
        "备忘录" | "Notes" | "notes" => "com.apple.mobilenotes",
        "提醒事项" | "Reminders" | "reminders" => "com.apple.reminders",
        "日历" | "Calendar" | "calendar" => "com.apple.mobilecal",
        "Safari" | "safari" | "浏览器" => "com.apple.mobilesafari",
        "信息" | "Messages" | "messages" => "com.apple.MobileSMS",
        "电话" | "Phone" | "phone" => "com.apple.mobilephone",
        "邮件" | "Mail" | "mail" => "com.apple.mobilemail",
        "地图" | "Maps" | "maps" => "com.apple.Maps",
        "App Store" | "app store" | "appstore" | "应用商店" => "com.apple.AppStore",
        "钱包" | "Wallet" | "wallet" => "com.apple.Passbook",
        "健康" | "Health" | "health" => "com.apple.Health",
        "文件" | "Files" | "files" => "com.apple.DocumentsApp",
        "快捷指令" | "Shortcuts" | "shortcuts" => "com.apple.shortcuts",
        "音乐" | "Music" | "music" => "com.apple.Music",
        "App资源库" | "Find My" | "查找" => "com.apple.findmy",
        _ => return None,
    })
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
