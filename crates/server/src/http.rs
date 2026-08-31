//! axum HTTP app: auth-gated routes for the WebRTC web client.
//!
//! Routes (contract from `web/index.html`):
//!   * `GET  /phone`       — auth-gated; serves the embedded web client.
//!   * `GET  /setup`       — auth-gated; serves the live iPhone connection guide.
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
    extract::{ws::WebSocketUpgrade, Query, State},
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

/// The embedded first-connect and recovery guide served at `/setup`.
const SETUP_HTML: &str = include_str!("../../../web/setup.html");

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

/// Control arbitration and its currently-authorized injector lease.
///
/// These values used to live behind two independent mutexes.  Some call sites
/// locked `control → current_lease` while the injector gate locked them in the
/// opposite order, which could deadlock the daemon permanently.  One mutex also
/// prevents observers from seeing a newly-acquired control holder paired with a
/// stale lease.
pub struct LeaseState {
    control: Control,
    current: Option<Lease>,
}

impl LeaseState {
    pub fn new() -> Self {
        Self {
            control: Control::new(),
            current: None,
        }
    }

    pub fn acquire(&mut self, holder: core::control::Holder, now: u64) -> Lease {
        let lease = self.control.acquire(holder, now);
        self.current = Some(lease.clone());
        lease
    }

    pub fn allows_injection(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|lease| self.control.is_current(lease))
    }

    pub fn release_if_current(&mut self, lease: &Lease) {
        if self.control.is_current(lease) {
            self.control.release(lease);
            self.current = None;
        }
    }
}

impl Default for LeaseState {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of consecutive failures that trigger a lockout.
const AUTH_MAX_FAILURES: u32 = 5;

/// Lockout duration in seconds after hitting [`AUTH_MAX_FAILURES`].
const AUTH_LOCKOUT_SECS: u64 = 30;

impl AuthLimiter {
    pub fn new() -> Self {
        AuthLimiter {
            failures: 0,
            locked_until: None,
        }
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
            self.locked_until =
                Some(Instant::now() + std::time::Duration::from_secs(AUTH_LOCKOUT_SECS));
        }
    }

    /// Record a successful auth.  Resets the failure counter and lifts any
    /// active lockout.
    pub fn record_success(&mut self) {
        self.failures = 0;
        self.locked_until = None;
    }
}

impl Default for AuthLimiter {
    fn default() -> Self {
        Self::new()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum WdaLifecycleTransition {
    Active = 0,
    Releasing = 1,
    Reconnecting = 2,
}

/// Single-owner arbitration for managed WDA start/stop transitions.
///
/// Release and reconnect are mutually exclusive device lifecycle operations.
/// Keeping them in one atomic state means they cannot both win after stale
/// prechecks and concurrently stop/bootstrap the same launchd supervisor.
#[derive(Debug)]
pub struct WdaLifecycle {
    transition: std::sync::atomic::AtomicU8,
}

impl WdaLifecycle {
    pub fn new() -> Self {
        Self {
            transition: std::sync::atomic::AtomicU8::new(WdaLifecycleTransition::Active as u8),
        }
    }

    fn current(&self) -> WdaLifecycleTransition {
        match self.transition.load(std::sync::atomic::Ordering::Acquire) {
            value if value == WdaLifecycleTransition::Active as u8 => {
                WdaLifecycleTransition::Active
            }
            value if value == WdaLifecycleTransition::Releasing as u8 => {
                WdaLifecycleTransition::Releasing
            }
            value if value == WdaLifecycleTransition::Reconnecting as u8 => {
                WdaLifecycleTransition::Reconnecting
            }
            _ => unreachable!("WDA lifecycle state is private and always valid"),
        }
    }

    fn try_begin(&self, transition: WdaLifecycleTransition) -> bool {
        debug_assert_ne!(transition, WdaLifecycleTransition::Active);
        self.transition
            .compare_exchange(
                WdaLifecycleTransition::Active as u8,
                transition as u8,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish(&self, transition: WdaLifecycleTransition) {
        debug_assert_ne!(transition, WdaLifecycleTransition::Active);
        let result = self.transition.compare_exchange(
            transition as u8,
            WdaLifecycleTransition::Active as u8,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
        debug_assert!(
            result.is_ok(),
            "only the current WDA lifecycle owner may finish its transition"
        );
    }

    fn try_begin_releasing(&self) -> bool {
        self.try_begin(WdaLifecycleTransition::Releasing)
    }

    fn finish_releasing(&self) {
        self.finish(WdaLifecycleTransition::Releasing);
    }

    fn try_begin_reconnecting(&self) -> bool {
        self.try_begin(WdaLifecycleTransition::Reconnecting)
    }

    fn finish_reconnecting(&self) {
        self.finish(WdaLifecycleTransition::Reconnecting);
    }

    pub fn is_releasing(&self) -> bool {
        self.current() == WdaLifecycleTransition::Releasing
    }

    pub fn is_reconnecting(&self) -> bool {
        self.current() == WdaLifecycleTransition::Reconnecting
    }

    fn is_transitioning(&self) -> bool {
        self.current() != WdaLifecycleTransition::Active
    }
}

impl Default for WdaLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared application state for all handlers.
pub struct AppState {
    /// Selected device transport. Direct mode never probes, captures, or injects
    /// through iPhone Mirroring; mirror mode keeps the original Mac-side path.
    pub backend: crate::config::DeviceBackend,
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
    /// Control arbitration and current injector authorization under one lock.
    pub lease_state: Arc<Mutex<LeaseState>>,
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
    /// Persisted target iPhone UDID used by every WDA start/restart.
    pub device_udid: Option<String>,
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
    /// cursor). Direct mode fails closed on WDA errors; only the explicit mirror
    /// backend may use the L3 compatibility path.
    /// `tokio::sync::Mutex` because the client mutates its cached session and
    /// handlers hold the lock across awaits.
    pub wda: Option<Arc<tokio::sync::Mutex<crate::wda::WdaClient>>>,
    /// Whether this daemon owns the local WDA supervisor and relay lifecycle.
    ///
    /// Only a direct backend pointed at a loopback WDA URL is managed. A remote
    /// `PHONE_REMOTE_WDA_URL` is externally owned: this process may use it, but
    /// must never stop or bootstrap local launchd jobs on its behalf.
    pub managed_wda: bool,
    /// A local Direct backend whose managed ownership is waiting for a
    /// canonical target UDID. Pending setup is neither daemon-managed nor
    /// external: lifecycle actions stay disabled until setup persists a target
    /// and the daemon restarts.
    pub managed_wda_pending: bool,
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
    /// Last-known "WDA can act on-device" flag, updated by Direct health probes
    /// and control events. Direct handlers use it as readiness evidence but
    /// always fail closed when WDA cannot act; they never fall through to the
    /// Mac injector. The explicit Mirror backend ignores WDA and retains its
    /// legacy host-capture/input path.
    pub wda_actionable: Arc<std::sync::atomic::AtomicBool>,
    /// Last completed WDA health probe. Status polling uses this cache whenever
    /// the control client is busy, so a slow health check never queues behind or
    /// blocks a time-sensitive browser gesture indefinitely.
    pub wda_health: Arc<Mutex<crate::wda::WdaHealth>>,
    /// Why WDA last stopped being drivable, captured at the transition (#26 §2).
    /// Without this, a mid-session `wda:true -> false` is indistinguishable from
    /// a human picking the phone up, and agents blame the wrong thing.
    pub wda_death: Arc<Mutex<WdaDeath>>,
    /// Single in-flight background health probe. Status requests return the
    /// cache immediately; a control request aborts this task before taking the
    /// WDA mutex so a cold/slow probe cannot delay an input action.
    pub wda_health_probe: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Number of Direct control requests currently waiting for or using WDA.
    /// Health polling re-checks this while holding `wda_health_probe`'s mutex,
    /// closing the race where a poll could start a new probe after input asked
    /// the previous probe to stop.
    pub wda_control_pending: Arc<std::sync::atomic::AtomicUsize>,
    /// Monotonic timestamp of the last remote-driving activity — any `/agent`
    /// control or live-view request, refreshed by [`AppState::touch_activity`].
    /// The idle-release watchdog ([`spawn_idle_release_watchdog`]) frees the
    /// phone (stops WDA, boots out its KeepAlive LaunchAgent) once this goes
    /// stale and nobody is watching, so the owner gets their device back when
    /// no one is driving it remotely.
    pub last_activity: Arc<Mutex<Instant>>,
    /// True while WDA has been auto-released for idle (runner stopped + its
    /// LaunchAgent booted out). The next `/agent/input` re-bootstraps it.
    pub released: Arc<std::sync::atomic::AtomicBool>,
    /// Mutually-exclusive managed WDA stop/bootstrap transition. New
    /// control/view requests fail fast while it is owned. `released` stays true
    /// until bootstrap succeeds, so a failed recovery is never reported active.
    pub wda_lifecycle: Arc<WdaLifecycle>,
    /// Open `/agent/mjpeg` live-view streams. A connected viewer counts as
    /// activity for as long as it watches, so passive viewing doesn't get
    /// released out from under the user.
    pub live_streams: Arc<std::sync::atomic::AtomicUsize>,
    /// Per-browser MJPEG byte activity keyed by a short, client-generated
    /// stream id. The web client asks for its own stream age on `/agent/status`
    /// so another viewer cannot make a frozen local image look fresh.
    pub mjpeg_stream_activity: Arc<Mutex<std::collections::HashMap<String, (u64, Instant)>>>,
    /// Recently served element trees keyed by their `snapshot` token, so
    /// `GET /agent/elements?since=<snapshot>` and `POST /agent/input?return=delta`
    /// can answer with a diff instead of the full tree. Bounded ring — an agent
    /// only ever diffs against its own last read or two, and a miss degrades
    /// gracefully to the full tree.
    pub element_snapshots: Arc<Mutex<ElementSnapshotCache>>,
}

/// The bounded ring behind [`AppState::element_snapshots`]: recently served
/// element trees keyed by their snapshot token, oldest first.
pub type ElementSnapshotCache =
    std::collections::VecDeque<(String, Arc<Vec<crate::wda::ElementRow>>)>;

impl AppState {
    /// Stamp "remote driving happened just now" for the idle-release watchdog.
    /// Called by every `/agent` action and live-view request (NOT `/agent/status`,
    /// which the web client polls constantly — counting it would pin the phone
    /// forever).
    pub fn touch_activity(&self) {
        *recover(self.last_activity.lock()) = Instant::now();
    }

    /// A viewer is actively watching — an MJPEG stream is open or a `/ws`
    /// WebRTC viewer is connected. The watchdog never releases out from under one.
    fn viewer_busy(&self) -> bool {
        self.live_streams.load(std::sync::atomic::Ordering::Relaxed) > 0
            || recover(self.viewers.lock()).count() > 0
    }

    /// How long since the last remote-driving activity.
    fn idle_for(&self) -> std::time::Duration {
        recover(self.last_activity.lock()).elapsed()
    }

    /// Give a Direct control operation priority over background health work.
    fn begin_wda_control(&self) -> WdaControlPriorityGuard {
        self.wda_control_pending
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Some(probe) = recover(self.wda_health_probe.lock()).take() {
            probe.abort();
        }
        WdaControlPriorityGuard(self.wda_control_pending.clone())
    }
}

/// RAII marker that prevents status polling from starting a competing WDA
/// health probe while a time-sensitive Direct action is pending.
struct WdaControlPriorityGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for WdaControlPriorityGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// RAII counter for in-flight `/agent/mjpeg` live-view streams. Increments
/// [`AppState::live_streams`] on creation and decrements on drop — so when the
/// viewer's connection ends (browser tab closed, network drop) the count falls
/// and the phone becomes eligible for idle release again.
struct StreamGuard(Arc<std::sync::atomic::AtomicUsize>);
impl StreamGuard {
    fn try_reserve(c: Arc<std::sync::atomic::AtomicUsize>, maximum: usize) -> Option<Self> {
        c.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| (current < maximum).then_some(current + 1),
        )
        .ok()
        .map(|_| StreamGuard(c))
    }
}
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

const MJPEG_INACTIVITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
static NEXT_MJPEG_ACTIVITY_TOKEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

struct MjpegActivityGuard {
    activity: Arc<Mutex<std::collections::HashMap<String, (u64, Instant)>>>,
    stream_id: String,
    token: u64,
}

impl MjpegActivityGuard {
    fn register(
        activity: Arc<Mutex<std::collections::HashMap<String, (u64, Instant)>>>,
        stream_id: String,
    ) -> Self {
        let token = NEXT_MJPEG_ACTIVITY_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        recover(activity.lock()).insert(stream_id.clone(), (token, Instant::now()));
        Self {
            activity,
            stream_id,
            token,
        }
    }

    fn touch(&self) {
        if let Some((token, last_chunk)) = recover(self.activity.lock()).get_mut(&self.stream_id) {
            if *token == self.token {
                *last_chunk = Instant::now();
            }
        }
    }
}

impl Drop for MjpegActivityGuard {
    fn drop(&mut self) {
        let mut activity = recover(self.activity.lock());
        if activity
            .get(&self.stream_id)
            .is_some_and(|(token, _)| *token == self.token)
        {
            activity.remove(&self.stream_id);
        }
    }
}

fn valid_mjpeg_stream_id(stream_id: &str) -> bool {
    (8..=64).contains(&stream_id.len())
        && stream_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[derive(Default, Deserialize)]
struct MjpegStreamQuery {
    stream_id: Option<String>,
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
        .route("/setup", get(setup))
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", get(logout))
        .route("/turn-creds", get(turn_creds))
        .route("/ws", get(ws_upgrade))
        // Browser control in the direct backend is deliberately independent of
        // WebRTC.  MJPEG viewers can still control the device when ICE/H.264 is
        // unavailable, and every request receives an explicit HTTP ACK.
        .route("/control", post(direct_control))
        // Agent operation entry (connect-in; reuses the validated injector +
        // control lease). Bearer-token auth; see `agent_input` / `agent_status`.
        .route("/agent/status", get(agent_status))
        .route("/agent/mode", post(agent_mode))
        .route("/agent/input", post(agent_input))
        .route("/agent/actions", post(agent_actions))
        .route("/agent/screenshot", get(agent_screenshot))
        .route("/agent/mjpeg", get(agent_mjpeg))
        .route("/agent/elements", get(agent_elements))
        // Shortcuts RPC return path: the phone POSTs structured results here.
        // Safe GET only peeks; destructive consumption has an explicit,
        // CSRF-protected POST endpoint.
        .route("/agent/inbox", get(agent_inbox_get).post(agent_inbox_post))
        .route("/agent/inbox/drain", post(agent_inbox_drain))
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
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
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

async fn setup(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_authed(&state, &headers) {
        return with_security_headers(Redirect::to("/login?next=%2Fsetup").into_response());
    }
    with_security_headers(Html(SETUP_HTML).into_response())
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
  width:min(86vw,320px);display:flex;flex-direction:column;gap:12px}
h1{font-size:17px;margin:0 0 4px;letter-spacing:.02em}
label{font-size:13px;font-weight:600;color:#dbe6ff}
.hint{margin:-4px 0 2px;color:#8b93a7;font-size:12px;line-height:1.45}
input{background:#08090c;color:#eef2ff;border:1px solid #272b38;border-radius:12px;
  padding:12px 14px;font-size:16px;-webkit-appearance:none}
input:focus{outline:none;border-color:#4f8cff}
input[aria-invalid="true"]{border-color:#ff5a66}
input:focus-visible,button:focus-visible{outline:3px solid rgba(79,140,255,.35);
  outline-offset:2px}
input[aria-invalid="true"]:focus-visible{outline-color:rgba(255,90,102,.32)}
button{background:#4f8cff;border:1px solid #4f8cff;color:#fff;border-radius:12px;
  padding:12px;font-size:15px;font-weight:600;cursor:pointer}
.err{color:#ff5a66;font-size:13px;line-height:1.4}
.err:empty{display:none}
</style></head><body>
<form method="POST" action="/login" novalidate>
  <h1>iphone-use</h1>
  <p class="hint" id="passwordHint">请输入这台 Mac 安装 iphone-use 时生成的控制密码。忘记后，请回到 Mac 重新运行安装程序查看或重设。</p>
  __NEXT_INPUT__
  <label for="password">控制密码</label>
  <input id="password" type="password" name="password" autofocus required
    autocomplete="current-password" autocapitalize="off" spellcheck="false"
    aria-describedby="passwordHint loginError" aria-invalid="__INVALID__" />
  <div class="err" id="loginError" role="alert" aria-live="assertive">__ERR__</div>
  <button type="submit">登录</button>
</form></body></html>"#;

fn login_destination(next: Option<&str>) -> &'static str {
    match next {
        Some("/setup") => "/setup",
        _ => "/phone",
    }
}

fn render_login(error: &str, next: Option<&str>) -> String {
    let next_input = match login_destination(next) {
        "/setup" => r#"<input type="hidden" name="next" value="/setup">"#,
        _ => "",
    };
    LOGIN_HTML
        .replace("__NEXT_INPUT__", next_input)
        .replace("__ERR__", error)
        .replace(
            "__INVALID__",
            if error.is_empty() { "false" } else { "true" },
        )
}

#[derive(Default, Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

async fn login_form(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let destination = login_destination(query.next.as_deref());
    // Already authed → return to the allow-listed route the user originally
    // requested instead of silently dropping them on the control page.
    if is_authed(&state, &headers) {
        return with_security_headers(Redirect::to(destination).into_response());
    }
    with_security_headers(Html(render_login("", Some(destination))).into_response())
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
    #[serde(default)]
    next: Option<String>,
}

async fn login_submit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let destination = login_destination(form.next.as_deref());
    let expected = match &state.password {
        // Open mode: any login succeeds (no password configured); no limiting.
        None => return redirect_with_cookie(&state, destination, &headers),
        Some(p) => p.clone(),
    };
    // The form deliberately uses `novalidate` so feedback is consistent across
    // browsers and remains available to assistive technology. Do not count a
    // missing value as an authentication failure: the user has not attempted a
    // credential yet.
    if form.password.is_empty() {
        let mut resp = Html(render_login("请输入控制密码", Some(destination))).into_response();
        *resp.status_mut() = StatusCode::BAD_REQUEST;
        return with_security_headers(resp);
    }
    // Check the limiter BEFORE verifying the password (prevents timing oracle).
    {
        let limiter = state.auth_limiter.lock().unwrap();
        if limiter.is_locked() {
            let mut resp = Html(render_login(
                "尝试次数过多。为保护手机，请 30 秒后再试",
                Some(destination),
            ))
            .into_response();
            *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
            return with_security_headers(resp);
        }
    }
    if core::auth::verify_password(&form.password, &expected) {
        state.auth_limiter.lock().unwrap().record_success();
        redirect_with_cookie(&state, destination, &headers)
    } else {
        state.auth_limiter.lock().unwrap().record_failure();
        let body = render_login("密码错误，请检查安装时保存的控制密码", Some(destination));
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
        return with_security_headers((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
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
// An agent (Hermes, an MCP client, or a script) drives the selected backend by
// POSTing to this already-running daemon. Direct dispatches only to on-device
// WDA. The explicit Mirror compatibility backend uses the legacy Mac injector.

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
    bearer_credential(headers).is_some_and(|token| ct_eq(token, expected.as_bytes()))
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

/// Authorize a route shared by the browser UI and bearer-authenticated agents.
///
/// A missing/expired browser cookie is not a bearer brute-force attempt. Only
/// requests that actually present `Authorization` are allowed to advance the
/// shared bearer limiter; otherwise a stale page polling in the background could
/// repeatedly lock out a legitimate MCP client.
fn browser_or_agent_auth(state: &AppState, headers: &HeaderMap) -> AgentAuth {
    if is_authed(state, headers) {
        AgentAuth::Ok
    } else if headers.contains_key(header::AUTHORIZATION) {
        agent_auth(state, headers)
    } else {
        AgentAuth::Denied
    }
}

/// Mutation endpoints require a non-simple custom header in addition to auth.
///
/// Cross-origin HTML forms and `text/plain` fetches cannot attach this header
/// without a CORS preflight, and this daemon exposes no CORS policy. This keeps
/// open-mode LAN deployments and cookie-authenticated browsers from becoming
/// drive-by CSRF targets.
fn has_phone_control_header(headers: &HeaderMap) -> bool {
    headers
        .get("x-phone-control")
        .and_then(|value| value.to_str().ok())
        == Some("1")
}

fn missing_phone_control_header_response() -> Response {
    with_security_headers(
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"ok":false,"error":"missing_control_header","required_header":"X-Phone-Control: 1","hint":"retry the same state-changing request with X-Phone-Control: 1"}"#,
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn target_not_configured_response() -> Response {
    with_security_headers(
        Response::builder()
            .status(StatusCode::CONFLICT)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"ok":false,"error":"target_not_configured","hint":"run setup-wda.sh to select and persist the canonical iPhone before using Direct control"}"#,
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
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

/// Return cached WDA health and, when idle, start one background refresh.
///
/// A cold WDA session can legitimately take longer than the old 1.5-second
/// status budget. Cancelling it on every poll meant the session was never
/// cached and `drivable` stayed false forever. The refresh now gets a realistic
/// deadline and survives the HTTP status request that started it. Direct input
/// has priority: [`AppState::begin_wda_control`] aborts the probe before waiting
/// for the shared WDA client, and the pending counter prevents a replacement
/// probe from racing in behind it.
async fn cached_wda_health(state: &AppState) -> crate::wda::WdaHealth {
    let cached = *recover(state.wda_health.lock());
    let Some(wda) = &state.wda else {
        return crate::wda::WdaHealth::down();
    };
    let mut probe_slot = recover(state.wda_health_probe.lock());
    if probe_slot
        .as_ref()
        .is_some_and(|probe| !probe.is_finished())
    {
        return cached;
    }
    *probe_slot = None;
    if state
        .wda_control_pending
        .load(std::sync::atomic::Ordering::Acquire)
        != 0
    {
        return cached;
    }

    let wda = wda.clone();
    let health_cache = state.wda_health.clone();
    let actionable = state.wda_actionable.clone();
    let released = state.released.clone();
    let death = state.wda_death.clone();
    let releasing = state.wda_lifecycle.is_releasing();
    *probe_slot = Some(tokio::spawn(async move {
        let Ok(mut client) = wda.try_lock() else {
            return;
        };
        match tokio::time::timeout(std::time::Duration::from_secs(15), client.probe_health()).await
        {
            Ok(health) => {
                apply_wda_health_probe_tracked(
                    &health_cache,
                    &actionable,
                    &released,
                    releasing,
                    Some(&death),
                    health,
                );
            }
            Err(_) => {
                // Preserve the last completed observation. A timeout is not an
                // authoritative "down", and the next status poll may retry.
                tracing::warn!("WDA health probe timed out; retaining cached health");
            }
        }
    }));
    cached
}

// ---------------------------------------------------------------------------
// wda_died_reason — who actually killed it (issue #26 §2)
// ---------------------------------------------------------------------------

/// Why WDA last stopped being drivable, and when.
///
/// `reason` is empty when WDA has never gone down in this daemon's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WdaDeath {
    pub reason: &'static str,
    /// Unix seconds of the transition; 0 when there hasn't been one.
    pub at: u64,
}

/// Classify a WDA up→down transition from the two observations around it.
///
/// Deliberately reasons from *signatures we can observe* rather than probing
/// the network stack. `warp-cli` is frequently absent even on machines running
/// WARP (the GUI install ships no CLI), so a warp-cli check would report
/// "no WARP" on exactly the machines this issue is about. What is always
/// observable is the shape of the death:
///
/// * `up && !actionable` — the runner still answers `/status` but every action
///   fails Code=41. That is the severed-testmanagerd-session signature: a WARP
///   reconnect, a sleep, or a lock tore down the session under a live runner.
/// * `!up` — nothing answers at all: the runner exited, the relay died, or the
///   phone's Wi-Fi DHCP lease moved and the 8100 relay is pointing at a stale
///   address.
///
/// Neither is a human picking up the phone, which is what the old status
/// implied by staying silent. Returns `None` when this is not a death (no
/// previous drivable state, or still drivable).
fn classify_wda_death(
    prev: crate::wda::WdaHealth,
    new: crate::wda::WdaHealth,
    released: bool,
    releasing: bool,
) -> Option<&'static str> {
    // Only a fall from "was working" counts. A probe that was already down and
    // stays down is not a new death, and must not overwrite the real cause.
    if !prev.actionable || new.actionable {
        return None;
    }
    // The daemon stopped it on purpose — this is the one case that is nobody's
    // fault, and conflating it with a crash sends agents into pointless repair.
    if released || releasing {
        return Some("idle_release");
    }
    if new.up {
        if new.locked == Some(true) {
            return Some("device_locked");
        }
        return Some("session_severed");
    }
    Some("unreachable")
}

/// Recovery guidance per death reason. Empty when there is nothing to say.
fn wda_death_hint(reason: &str) -> &'static str {
    match reason {
        "idle_release" => {
            "WDA was released on purpose after idle — the next control request re-bootstraps it; nothing is broken"
        }
        "device_locked" => {
            "the iPhone locked while WDA was driving it — unlock it and keep it awake"
        }
        // Named in likelihood order from what has actually caused this in the
        // field; the daemon cannot see which of them fired.
        "session_severed" => {
            "WDA still answers but its test session was torn down — a WARP/VPN reconnect, Mac sleep, or a phone lock does this; restart the direct device service, and if WARP is on, exclude the CoreDevice tunnel"
        }
        "unreachable" => {
            "WDA stopped answering entirely — the runner exited, the 8100/9100 relay died, or the phone's Wi-Fi address changed; re-run setup-wda.sh and check the relay"
        }
        _ => "",
    }
}

/// Commit one completed WDA health observation to every readiness cache.
///
/// `released` tracks whether the managed runner has relinquished the device.
/// An authoritative `up` probe clears it immediately, including when launchd
/// self-healed the runner outside an explicit `/agent/mode` readiness wait.
fn apply_wda_health_probe(
    health_slot: &Mutex<crate::wda::WdaHealth>,
    actionable: &std::sync::atomic::AtomicBool,
    released: &std::sync::atomic::AtomicBool,
    health: crate::wda::WdaHealth,
) -> bool {
    apply_wda_health_probe_tracked(health_slot, actionable, released, false, None, health)
}

/// [`apply_wda_health_probe`] plus death attribution.
///
/// Split so the plain call sites stay unchanged while the probe path can
/// record *why* WDA stopped being drivable. This is the single choke point
/// every completed observation passes through, so it is the only place that
/// sees both sides of a transition.
fn apply_wda_health_probe_tracked(
    health_slot: &Mutex<crate::wda::WdaHealth>,
    actionable: &std::sync::atomic::AtomicBool,
    released: &std::sync::atomic::AtomicBool,
    releasing: bool,
    death_slot: Option<&Mutex<WdaDeath>>,
    health: crate::wda::WdaHealth,
) -> bool {
    use std::sync::atomic::Ordering;

    let prev = *recover(health_slot.lock());
    let was_released = released.load(Ordering::Acquire);
    if let Some(slot) = death_slot {
        if let Some(reason) = classify_wda_death(prev, health, was_released, releasing) {
            tracing::warn!(
                reason,
                "WDA stopped being drivable: {}",
                wda_death_hint(reason)
            );
            *recover(slot.lock()) = WdaDeath {
                reason,
                at: now_secs(),
            };
        } else if health.actionable {
            // Recovered — clear the epitaph so a stale cause can't be read as
            // the current state.
            *recover(slot.lock()) = WdaDeath::default();
        }
    }

    *recover(health_slot.lock()) = health;
    actionable.store(health.actionable, Ordering::Release);
    if health.up {
        released.store(false, Ordering::Release);
    }
    health.actionable
}

fn finish_wda_readiness_wait(lifecycle: &WdaLifecycle) {
    lifecycle.finish_reconnecting();
}

const WDA_READINESS_TIMEOUT_SECS: u64 = 420;

fn spawn_wda_readiness_wait(state: Arc<AppState>) {
    tokio::spawn(async move {
        // setup-wda.sh allows up to six minutes for xcodebuild to report the
        // on-device server URL, and first startup after an Xcode update can use
        // most of that budget. Ending the lifecycle after two minutes exposed
        // `released` while launchd was still building, which invited a second
        // reconnect against the same in-flight supervisor. Keep a small margin
        // for prerequisite checks and relay verification.
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(WDA_READINESS_TIMEOUT_SECS);
        let mut ready = false;
        let mut seen_up = false;
        let mut setup_blocker = String::new();
        while tokio::time::Instant::now() < deadline && !state.wda_lifecycle.is_releasing() {
            // A concrete prerequisite failure is authoritative. Keeping the
            // transition at `reconnecting` for the full two-minute WDA budget
            // made an unplugged phone look like a slow-but-healthy startup and
            // hid the actionable USB/trust/DDI message from clients.
            // Lifecycle transitions must only trust the current helper's
            // structured status. `read_setup_blocked_on` also has a narrow
            // old-helper log fallback for display, but a stale log inference
            // must never end an active reconnect and expose `released` while
            // launchd is still rebuilding WDA.
            setup_blocker = read_structured_setup_blocked_on();
            if !setup_blocker.is_empty() {
                break;
            }
            if let Some(wda) = &state.wda {
                let _priority = state.begin_wda_control();
                let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                    wda.lock().await.probe_health().await
                })
                .await;
                if let Ok(health) = result {
                    seen_up |= health.up;
                    if apply_wda_health_probe(
                        &state.wda_health,
                        &state.wda_actionable,
                        &state.released,
                        health,
                    ) {
                        ready = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if ready {
            state.touch_activity();
        } else if !setup_blocker.is_empty() {
            tracing::warn!(
                blocked_on = %setup_blocker,
                "managed WDA reconnect stopped on a setup prerequisite"
            );
        } else if seen_up {
            let health = *recover(state.wda_health.lock());
            tracing::warn!(
                locked = ?health.locked,
                "managed WDA is running but did not become actionable before reconnect deadline"
            );
        } else {
            tracing::warn!("managed WDA did not become actionable before reconnect deadline");
        }
        finish_wda_readiness_wait(&state.wda_lifecycle);
    });
}

/// `GET /agent/status` — authenticated backend/readiness/lifecycle probe.
///
/// Direct callers gate on `drivable`; its legacy `phone_target` field is always
/// false and no Mirroring API is touched. In explicit Mirror compatibility
/// mode, `phone_target` reports whether a Mirroring window is currently
/// findable on macOS.
async fn agent_status(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MjpegStreamQuery>,
    headers: HeaderMap,
) -> Response {
    // Same cookie-or-bearer rule as `agent_screenshot`: a logged-in browser
    // viewer may read the health/version probe (the web client uses it for
    // the update banner). Cookie first so polling never trips the limiter;
    // only honored when a password is configured (see agent_screenshot).
    // Browser access follows the same contract as `/phone`: no configured
    // password means an intentionally open browser UI, even when a separate
    // agent bearer token protects machine callers.
    match browser_or_agent_auth(&state, &headers) {
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
    if query
        .stream_id
        .as_deref()
        .is_some_and(|stream_id| !valid_mjpeg_stream_id(stream_id))
    {
        return with_security_headers(
            (StatusCode::BAD_REQUEST, "invalid MJPEG stream id").into_response(),
        );
    }
    let direct = state.backend == crate::config::DeviceBackend::Direct;
    let lifecycle = state.wda_lifecycle.current();
    let releasing = lifecycle == WdaLifecycleTransition::Releasing;
    let reconnecting = lifecycle == WdaLifecycleTransition::Reconnecting;
    let released = state.released.load(std::sync::atomic::Ordering::Relaxed);
    // Direct mode must not touch any iPhone Mirroring API. The legacy backend
    // keeps the cheap geometry probe for compatibility status.
    #[cfg(target_os = "macos")]
    let phone_target = !direct && core::capture::find_mirroring_geometry().is_ok();
    #[cfg(not(target_os = "macos"))]
    let phone_target = false;
    // L2 health — action-level, not just /status (which lies: it reports
    // `ready` even when every UI action fails Code=41 because the phone is
    // locked or the test session was severed). `wda` stays "runner reachable"
    // for back-compat; `wda_actionable` is the honest "can it act right now".
    let health = if !direct || state.managed_wda_pending {
        crate::wda::WdaHealth::down()
    } else if reconnecting {
        *recover(state.wda_health.lock())
    } else {
        cached_wda_health(&state).await
    };
    let wda = health.up;
    let wda_actionable = health.actionable;
    let wda_locked = match health.locked {
        Some(true) => "true",
        Some(false) => "false",
        None => "null",
    };
    // `backend` is configuration and never changes because a health probe
    // flickered. `mode` remains for old clients, but in direct mode it can only
    // be agent/offline — never an implicit switch back to Mirroring.
    let mode = if direct {
        if wda {
            "agent"
        } else {
            "offline"
        }
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
    let (mirror_state, drivable) = if direct {
        (
            "disabled",
            wda_actionable && !releasing && !reconnecting && !released,
        )
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
    let human_active = !direct && drivable && !crate::macos::mirroring_is_frontmost();
    #[cfg(not(target_os = "macos"))]
    let human_active = false;
    // Version + update hint. `latest_release` is fetched by a background
    // task (24h cadence); compare as plain tags — any mismatch with the
    // running version means a release the binary doesn't match.
    let version = env!("CARGO_PKG_VERSION");
    let latest = recover(state.latest_release.lock()).clone();
    let (latest_json, update_available) = match &latest {
        Some(tag) => (
            serde_json::to_string(tag).unwrap_or_else(|_| "null".to_string()),
            tag.trim_start_matches('v') != version,
        ),
        None => ("null".to_string(), false),
    };
    // Connected `/ws` viewers (active + queued) — issue #8.
    let ws_viewer_count = recover(state.viewers.lock()).count();
    let mjpeg_viewer_count = state
        .live_streams
        .load(std::sync::atomic::Ordering::Relaxed);
    let viewer_count = ws_viewer_count.saturating_add(mjpeg_viewer_count);
    let mjpeg_stream_age_ms = query.stream_id.as_deref().and_then(|stream_id| {
        recover(state.mjpeg_stream_activity.lock())
            .get(stream_id)
            .map(|(_, last_chunk)| {
                u64::try_from(last_chunk.elapsed().as_millis()).unwrap_or(u64::MAX)
            })
    });
    let mjpeg_stream_fresh = mjpeg_stream_age_ms.is_some_and(|age_ms| {
        age_ms <= u64::try_from(MJPEG_INACTIVITY_TIMEOUT.as_millis()).unwrap_or(u64::MAX)
    });
    let mjpeg_stream_age_json = mjpeg_stream_age_ms
        .map(|age_ms| age_ms.to_string())
        .unwrap_or_else(|| "null".to_string());
    let recovery_owner = if direct {
        if state.managed_wda {
            "daemon"
        } else if state.managed_wda_pending {
            "unconfigured"
        } else {
            "external"
        }
    } else {
        "mirror"
    };
    // Setup progress: `setup-wda.sh` writes ~/.iphone-use/wda-setup-status.json
    // ({phase, blocked_on, message, ts}) as it runs. Read it before selecting
    // the recovery hint: a concrete prerequisite failure must take precedence
    // over the generic "keep waiting" text while reconnecting.
    let setup_status = if direct && state.managed_wda {
        read_structured_setup_status()
    } else {
        None
    };
    let setup_blocked_on = if direct && state.managed_wda {
        read_setup_blocked_on()
    } else {
        String::new()
    };
    let setup_phase_json = serde_json::to_string(
        setup_status
            .as_ref()
            .map(|status| status.phase.as_str())
            .unwrap_or(""),
    )
    .unwrap_or_else(|_| "\"\"".to_string());
    let setup_message_json = serde_json::to_string(
        setup_status
            .as_ref()
            .map(|status| status.message.as_str())
            .unwrap_or(""),
    )
    .unwrap_or_else(|_| "\"\"".to_string());
    // Death attribution (#26 §2). Only meaningful while WDA is actually down;
    // a stale cause next to a healthy runner would read as a live problem.
    let death = *recover(state.wda_death.lock());
    let (wda_died_reason, wda_died_at) = if wda_actionable || death.reason.is_empty() {
        ("", 0)
    } else {
        (death.reason, death.at)
    };
    // Build progress (#26 §1). Read raw — unlike `setup_status`, a *stale*
    // status is meaningful here: it means the helper died mid-build rather
    // than that the blocker went away.
    let wda_build = if direct && state.managed_wda {
        derive_wda_build(
            read_raw_setup_status().as_ref(),
            now_secs(),
            read_runner_log_tail,
        )
    } else {
        WdaBuild::unknown()
    }
    .to_json();
    // When not drivable, tell the caller HOW to recover (the recovery differs by
    // state, and auto-recovery is blocked by macOS while the phone is in use).
    // Plain text only — kept free of quotes/braces so it drops into the JSON.
    let hint = if direct && releasing {
        "direct device service is being released after inactivity — wait for confirmation before reconnecting"
    } else if direct && !wda {
        if state.managed_wda_pending {
            "no canonical iPhone target is configured — run setup-wda.sh to persist PHONE_REMOTE_UDID; until then the daemon will not stop or bootstrap local WDA"
        } else if let Some(blocker_hint) = setup_blocker_hint(&setup_blocked_on) {
            blocker_hint
        } else if reconnecting {
            "the daemon is restarting its managed direct device service — wait for reconnecting=false before retrying"
        } else if released && !state.managed_wda {
            "the remote WDA endpoint is externally managed — restart it on the owning host; this daemon will not stop or bootstrap local services"
        } else if released {
            "direct device service was released after inactivity — reconnect to restart WDA, then keep the phone unlocked and awake"
        } else if !state.managed_wda {
            "the configured remote WDA endpoint is unreachable and externally managed — recover it on the owning host; this daemon will not run local setup or launchctl commands"
        } else {
            "direct device service is unreachable — start or repair WDA and the 8100/9100 relays; iPhone Mirroring is not used"
        }
    } else if direct && reconnecting {
        "the daemon is restarting its managed direct device service — wait for reconnecting=false before retrying"
    } else if direct && wda && !wda_actionable {
        if wda_locked == "true" {
            "WDA is reachable but the iPhone is locked — unlock it and keep it awake; direct control never falls back to iPhone Mirroring"
        } else if !wda_died_reason.is_empty() {
            // We watched it die; say what took it down instead of the generic
            // "cannot act" that made a severed session look like interference.
            wda_death_hint(wda_died_reason)
        } else {
            "WDA is reachable but cannot act — restart the direct device service; direct control is fail-closed and will not inject into the Mac"
        }
    } else if !drivable {
        match mirror_state {
            "paused" => "Mirroring needs reconnecting (paused / interrupted / timed out) — tap the Resume/Connect/Try Again button (x=0.5, y=0.64), once, then wait 45s+; do NOT loop",
            "in_use" => "iPhone in use — LOCK the phone to reconnect; the on-screen Connect button will not reconnect while it is in use",
            "offline" => "no iPhone Mirroring window — open it on the Mac; to use on-device control, persist PHONE_REMOTE_BACKEND=direct and restart the daemon",
            _ => "",
        }
    } else if human_active {
        // Issue #16: a human is on the Mac — yield instead of stealing focus.
        "a human is using the Mac (iPhone Mirroring is not frontmost) — an L3 tap will steal their focus; pause until they are idle, or persist PHONE_REMOTE_BACKEND=direct and restart for on-device control"
    } else {
        ""
    };
    let device_state = if releasing {
        "releasing"
    } else if direct && !wda && !setup_blocked_on.is_empty() {
        "blocked"
    } else if reconnecting {
        "reconnecting"
    } else if released {
        "released"
    } else if wda_actionable {
        "ready"
    } else if wda_locked == "true" {
        "locked"
    } else if wda {
        "blocked"
    } else {
        "offline"
    };
    let screen_state = if direct && wda && mjpeg_stream_fresh {
        "live"
    } else if direct && wda {
        "waiting"
    } else if direct {
        "offline"
    } else if phone_target {
        "ready"
    } else {
        "offline"
    };
    let body = format!(
        r#"{{"ok":true,"backend":"{}","target_configured":{},"managed_wda":{},"managed_wda_pending":{},"recovery_owner":"{recovery_owner}","phone_target":{phone_target},"wda":{wda},"wda_actionable":{wda_actionable},"wda_locked":{wda_locked},"drivable":{drivable},"human_active":{human_active},"mode":"{mode}","device_state":"{device_state}","screen_state":"{screen_state}","mirror_state":"{mirror_state}","releasing":{releasing},"reconnecting":{reconnecting},"released":{released},"hint":"{hint}","setup_blocked_on":"{setup_blocked_on}","setup_phase":{setup_phase_json},"setup_message":{setup_message_json},"wda_build":{wda_build},"wda_died_reason":"{wda_died_reason}","wda_died_at":{wda_died_at},"viewer_count":{viewer_count},"mjpeg_viewer_count":{mjpeg_viewer_count},"mjpeg_stream_fresh":{mjpeg_stream_fresh},"mjpeg_stream_age_ms":{mjpeg_stream_age_json},"version":"{version}","latest":{latest_json},"update_available":{update_available}}}"#,
        state.backend.as_str(),
        state.device_udid.is_some(),
        state.managed_wda,
        state.managed_wda_pending,
    );
    let resp = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    with_security_headers(resp)
}

/// Read `blocked_on` from `setup-wda.sh`'s status file, but only if it was
/// written in the last 5 minutes (a stale file from a finished run shouldn't be
/// reported as a live blocker).
///
/// Older installed helper copies predate the structured USB status write. For
/// that one compatibility case, inspect only the latest, fresh setup-log
/// attempt and recognize its exact USB failure text. This keeps a source-built
/// daemon paired with an older installed helper from polling `reconnecting`
/// blindly for two minutes. The fallback is deliberately narrow: it does not
/// infer blockers from arbitrary log prose.
fn read_setup_blocked_on() -> String {
    let blocker = read_structured_setup_blocked_on();
    if !blocker.is_empty() {
        return blocker;
    }
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return String::new(),
    };
    read_recent_setup_log_blocker(&format!("{home}/.iphone-use/wda-agent.log"))
}

/// Read only the current helper's timestamped structured prerequisite state.
///
/// Unlike [`read_setup_blocked_on`], this has no compatibility inference from
/// historical log text and is therefore safe to drive reconnect lifecycle.
fn read_structured_setup_blocked_on() -> String {
    read_structured_setup_status()
        .map(|status| status.blocked_on)
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct WdaSetupStatus {
    #[serde(default)]
    phase: String,
    #[serde(default)]
    blocked_on: String,
    #[serde(default)]
    message: String,
    ts: u64,
}

fn read_structured_setup_status() -> Option<WdaSetupStatus> {
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return None,
    };
    let status_path = format!("{home}/.iphone-use/wda-setup-status.json");
    std::fs::read_to_string(status_path)
        .ok()
        .and_then(|txt| parse_setup_status(&txt, now_secs()))
}

fn read_recent_setup_log_blocker(path: &str) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};

    const MAX_LOG_TAIL_BYTES: u64 = 64 * 1024;
    const MAX_LOG_AGE_SECS: u64 = 300;

    let Ok(metadata) = std::fs::metadata(path) else {
        return String::new();
    };
    let fresh = metadata
        .modified()
        .ok()
        .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age.as_secs() <= MAX_LOG_AGE_SECS);
    if !fresh {
        return String::new();
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let start = metadata.len().saturating_sub(MAX_LOG_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len().saturating_sub(start)).unwrap_or(0));
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    parse_setup_log_blocked_on(&String::from_utf8_lossy(&bytes))
}

fn parse_setup_log_blocked_on(txt: &str) -> String {
    let latest_attempt = txt
        .rsplit_once("== Checking prerequisites")
        .map_or(txt, |(_, latest)| latest);
    if latest_attempt.contains("WARP is ON and will block WDA")
        || latest_attempt.contains("Split Tunnel exclusions do not cover the CoreDevice")
    {
        "warp".to_string()
    } else if latest_attempt.contains("not currently connected over USB")
        || latest_attempt.contains("no USB iPhone was found")
        || latest_attempt.contains("no USB iPhone is connected")
    {
        "usb".to_string()
    } else {
        String::new()
    }
}

fn setup_blocker_hint(blocked_on: &str) -> Option<&'static str> {
    match blocked_on {
        "warp" => Some(
            "WARP is capturing the CoreDevice device tunnel — for selected destinations, ask the Zero Trust administrator for Traffic only mode with Split Tunnels Include limited to those destination IPs/CIDRs; otherwise exclude fe80::/10 and fd00::/8 in full-tunnel mode (or temporarily run warp-cli disconnect), then wait for policy propagation and poll status; do not send another reconnect request while this blocker remains",
        ),
        "proxy" => Some(
            "a system proxy is blocking CoreDevice/WDA — disable the proxy for the device tunnel, then poll status; do not send another reconnect request while this blocker remains",
        ),
        "usb" => Some(
            "the configured iPhone is not available over USB — connect that phone, unlock it, and keep it awake while the managed service retries",
        ),
        "trust" => Some(
            "the configured iPhone needs trust or developer-signing approval — unlock the phone, accept the prompt, then keep it awake while the managed service retries",
        ),
        "ddi" => Some(
            "the iPhone Developer Disk Image is unavailable — open Xcode with the phone connected, let device preparation finish, then poll status",
        ),
        "wda" => Some(
            "WebDriverAgent failed to start — inspect ~/.iphone-use/wda-agent.log and run setup-wda.sh doctor before retrying",
        ),
        _ => None,
    }
}

#[cfg(test)]
fn parse_setup_blocked_on(txt: &str, now: u64) -> String {
    parse_setup_status(txt, now)
        .map(|status| status.blocked_on)
        .unwrap_or_default()
}

fn parse_setup_status(txt: &str, now: u64) -> Option<WdaSetupStatus> {
    let mut status: WdaSetupStatus = serde_json::from_str(txt).ok()?;
    if now.saturating_sub(status.ts) > 300 {
        return None;
    }
    if !matches!(
        status.blocked_on.as_str(),
        "" | "warp" | "proxy" | "usb" | "trust" | "ddi" | "wda"
    ) {
        return None;
    }
    // Old helpers emitted only {blocked_on, ts}. Keep their actionable
    // blocker compatible, while rejecting a payload that has neither progress
    // nor a blocker and therefore communicates no state at all.
    if status.phase.is_empty() && status.blocked_on.is_empty() {
        return None;
    }
    // Keep the wire response bounded even if a locally modified helper writes
    // an unexpectedly large progress string. serde_json handles escaping.
    status.phase = status.phase.chars().take(64).collect();
    status.message = status.message.chars().take(512).collect();
    Some(status)
}

// ---------------------------------------------------------------------------
// wda_build — "is it compiling, or did it fail?" (issue #26 §1)
// ---------------------------------------------------------------------------

/// Bring-up progress, split into the one distinction `setup_blocked_on` can't
/// make: **still working** vs **gave up**.
///
/// `setup_blocked_on` answers "what prerequisite is missing" and is empty for
/// a run that is simply slow. But an `xcodebuild` that is three minutes into a
/// clean build and an `xcodebuild` that died two minutes ago both present as
/// `wda:false, setup_blocked_on:""` — so an agent polling status cannot tell
/// "wait longer" from "stop waiting and read the log". This object makes that
/// call explicit, and carries the log tail for the case where the answer is
/// "go look".
#[derive(Debug, Clone, PartialEq, Eq)]
struct WdaBuild {
    /// `ready` | `building` | `failed` | `stalled` | `unknown`.
    state: &'static str,
    /// The helper's own phase string, verbatim (`building`, `ddi-wait`, …).
    phase: String,
    /// Unix seconds of the helper's last status write; 0 when unknown.
    since: u64,
    /// Seconds since that write — how long this state has been true.
    age_secs: u64,
    /// Tail of the runner log, non-empty only when the state is worth reading
    /// a log for (`failed` / `stalled`).
    log_tail: String,
}

impl WdaBuild {
    /// No status file at all — bring-up was never attempted by this helper.
    fn unknown() -> Self {
        Self {
            state: "unknown",
            phase: String::new(),
            since: 0,
            age_secs: 0,
            log_tail: String::new(),
        }
    }

    fn to_json(&self) -> String {
        format!(
            r#"{{"state":"{}","phase":{},"since":{},"age_secs":{},"log_tail":{}}}"#,
            self.state,
            serde_json::to_string(&self.phase).unwrap_or_else(|_| "\"\"".into()),
            self.since,
            self.age_secs,
            serde_json::to_string(&self.log_tail).unwrap_or_else(|_| "\"\"".into()),
        )
    }
}

/// A helper phase this stale is not "working slowly", it is gone.
///
/// `setup-wda.sh` rewrites its status on every step, including a per-poll
/// "building (Ns elapsed)" heartbeat, so silence this long means the process
/// died without writing a `-fail` phase (killed, panicked, machine slept).
const BUILD_STALE_SECS: u64 = 300;

/// Map a helper phase + its age onto a build state.
///
/// The helper's vocabulary is regular: `ready` is terminal-success, anything
/// ending in `-fail` is terminal-failure, and everything else (`prereq`,
/// `ddi-wait`, `building`, `trust`, `serving`, `supervisor`) is in-flight.
/// Keying on the `-fail` suffix rather than an allow-list means a new failure
/// phase added to the script reports as a failure here without a code change.
fn classify_build_state(phase: &str, age_secs: u64) -> &'static str {
    if phase.is_empty() {
        return "unknown";
    }
    if phase == "ready" {
        return "ready";
    }
    if phase.ends_with("-fail") {
        return "failed";
    }
    if age_secs > BUILD_STALE_SECS {
        return "stalled";
    }
    "building"
}

/// Last `max_lines` non-empty lines of `txt`, capped at `max_bytes`.
///
/// Bounded on both axes because this rides on every `/agent/status` poll: a
/// runaway xcodebuild log must not turn a status check into a megabyte.
fn tail_lines(txt: &str, max_lines: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = txt.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut out = lines[start..].join("\n");
    if out.len() > max_bytes {
        // Cut from the front — the end of a build log is the interesting part.
        let cut = out
            .char_indices()
            .rev()
            .map(|(i, _)| i)
            .find(|i| out.len() - i <= max_bytes)
            .unwrap_or(0);
        out = out.split_off(cut);
    }
    out
}

/// Build [`WdaBuild`] from the helper's status file plus, when the state calls
/// for it, the tail of the runner log.
fn derive_wda_build(
    status: Option<&WdaSetupStatus>,
    now: u64,
    read_log: impl Fn() -> String,
) -> WdaBuild {
    let Some(status) = status else {
        return WdaBuild::unknown();
    };
    let age_secs = now.saturating_sub(status.ts);
    let state = classify_build_state(&status.phase, age_secs);
    // Only pay for the log read when the answer is "go read the log".
    let log_tail = if matches!(state, "failed" | "stalled") {
        tail_lines(&read_log(), 12, 1200)
    } else {
        String::new()
    };
    WdaBuild {
        state,
        phase: status.phase.clone(),
        since: status.ts,
        age_secs,
        log_tail,
    }
}

/// Read the helper status **without** the 5-minute freshness gate.
///
/// [`read_structured_setup_status`] drops a stale file so a finished run isn't
/// reported as a live blocker. Build state needs the opposite: a stale
/// `building` is exactly the signal that the helper died mid-build.
fn read_raw_setup_status() -> Option<WdaSetupStatus> {
    let home = std::env::var("HOME").ok()?;
    let txt = std::fs::read_to_string(format!("{home}/.iphone-use/wda-setup-status.json")).ok()?;
    let mut status: WdaSetupStatus = serde_json::from_str(&txt).ok()?;
    status.phase = status.phase.chars().take(64).collect();
    status.message = status.message.chars().take(512).collect();
    Some(status)
}

/// Tail of `~/.iphone-use/wda-runner.log` — the xcodebuild output.
fn read_runner_log_tail() -> String {
    let Ok(home) = std::env::var("HOME") else {
        return String::new();
    };
    std::fs::read_to_string(format!("{home}/.iphone-use/wda-runner.log")).unwrap_or_default()
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

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn launchd_job_loaded(domain: &str, label: &str) -> bool {
    std::process::Command::new("launchctl")
        .args(["print", &format!("{domain}/{label}")])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn wait_launchd_job_gone(domain: &str, label: &str) -> bool {
    for _ in 0..20 {
        if !launchd_job_loaded(domain, label) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    !launchd_job_loaded(domain, label)
}

fn valid_wda_udid(udid: &str) -> bool {
    !udid.is_empty()
        && udid.len() <= 128
        && udid
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-')
}

/// Write a same-directory, mode-0600 staging file without touching the live
/// destination. The caller validates and atomically renames it into place.
fn stage_file(
    destination: &std::path::Path,
    contents: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;

    static NEXT_STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("staged file has no parent"))?;
    for _ in 0..32 {
        let suffix = NEXT_STAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{}.{}.{}.tmp",
            destination
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("wda-agent"),
            std::process::id(),
            suffix
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&candidate) {
            Ok(mut file) => {
                let written = file.write_all(contents).and_then(|()| file.sync_all());
                drop(file);
                match written {
                    Ok(()) => return Ok(candidate),
                    Err(error) => {
                        let _ = std::fs::remove_file(&candidate);
                        return Err(error);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique staging file",
    ))
}

fn restore_plist(plist_path: &std::path::Path, original: Option<&[u8]>) {
    match original {
        Some(contents) => {
            if let Ok(staged) = stage_file(plist_path, contents) {
                let _ = std::fs::rename(staged, plist_path);
            }
        }
        None => {
            let _ = std::fs::remove_file(plist_path);
        }
    }
    if let Some(parent) = plist_path.parent() {
        let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
    }
}

/// Start or restart the dedicated WDA supervisor without destroying its
/// persisted signing/path/port policy.
///
/// `setup-wda.sh` owns the complete plist contract. Reconnect edits only a
/// mode-0600 staging copy, validates it, and atomically installs it so a crash
/// cannot truncate the live policy. A changed target is fully unloaded and
/// bootstrapped because launchd caches environment variables; an unchanged
/// policy may be kickstarted. A minimal plist is created only when no
/// setup-generated file exists yet.
fn write_and_bootstrap_wda_agent(home: &str, setup_sh: &str, log: &str, udid: &str) -> bool {
    if !std::path::Path::new(setup_sh).is_file() || !valid_wda_udid(udid) {
        return false;
    }
    let plist_path = std::path::PathBuf::from(format!(
        "{home}/Library/LaunchAgents/{WDA_AGENT_LABEL}.plist"
    ));
    let Some(parent) = plist_path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let original = match std::fs::read(&plist_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return false,
    };

    let candidate = if let Some(contents) = &original {
        contents.clone()
    } else {
        let mut environment = vec![
            ("WDA_KEEPALIVE", "1".to_string()),
            ("WDA_UDID", udid.to_string()),
            (
                "PATH",
                "/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/usr/bin:/bin".to_string(),
            ),
        ];
        for key in [
            "WDA_TEAM_ID",
            "WDA_BUNDLE_ID",
            "WDA_DIR",
            "WDA_REF",
            "WDA_PORT",
            "MJPEG_PORT",
            "WDA_ALLOW_LAN",
        ] {
            if let Ok(value) = std::env::var(key) {
                if !value.is_empty() {
                    environment.push((key, value));
                }
            }
        }
        let env_xml = environment
            .into_iter()
            .map(|(key, value)| {
                format!(
                    "        <key>{key}</key><string>{}</string>\n",
                    xml_escape(&value)
                )
            })
            .collect::<String>();
        let setup_sh_xml = xml_escape(setup_sh);
        let log_xml = xml_escape(log);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>{WDA_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>/bin/bash</string><string>{setup_sh_xml}</string></array>
    <key>EnvironmentVariables</key>
    <dict>
{env_xml}    </dict>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>RunAtLoad</key><true/>
    <key>StandardOutPath</key><string>{log_xml}</string>
    <key>StandardErrorPath</key><string>{log_xml}</string>
</dict></plist>
"#
        )
        .into_bytes()
    };

    let staged = match stage_file(&plist_path, &candidate) {
        Ok(path) => path,
        Err(_) => return false,
    };
    if original.is_some() {
        // Edit only the staging copy. A crash or PlistBuddy failure cannot
        // truncate or partially rewrite the live launchd configuration.
        let set_command = format!("Set :EnvironmentVariables:WDA_UDID {udid}");
        let set = std::process::Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &set_command])
            .arg(&staged)
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !set {
            let add_command = format!("Add :EnvironmentVariables:WDA_UDID string {udid}");
            let added = std::process::Command::new("/usr/libexec/PlistBuddy")
                .args(["-c", &add_command])
                .arg(&staged)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !added {
                let _ = std::fs::remove_file(&staged);
                return false;
            }
        }
    }
    if !std::process::Command::new("plutil")
        .args(["-lint"])
        .arg(&staged)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&staged);
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600)).is_err() {
            let _ = std::fs::remove_file(&staged);
            return false;
        }
    }
    let staged_contents = match std::fs::read(&staged) {
        Ok(contents) => contents,
        Err(_) => {
            let _ = std::fs::remove_file(&staged);
            return false;
        }
    };
    let plist_changed = original.as_deref() != Some(staged_contents.as_slice());
    if plist_changed {
        if std::fs::rename(&staged, &plist_path).is_err()
            || std::fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .is_err()
        {
            let _ = std::fs::remove_file(&staged);
            restore_plist(&plist_path, original.as_deref());
            return false;
        }
    } else {
        let _ = std::fs::remove_file(&staged);
    }

    let domain = gui_domain();
    let service = format!("{domain}/{WDA_AGENT_LABEL}");
    let was_loaded = launchd_job_loaded(&domain, WDA_AGENT_LABEL);
    // A persistently disabled service rejects bootstrap. Enable first and treat
    // failure as authoritative instead of continuing into a misleading start.
    if !std::process::Command::new("launchctl")
        .args(["enable", &service])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        if plist_changed {
            if was_loaded {
                let _ = std::process::Command::new("launchctl")
                    .args(["bootout", &service])
                    .status();
                let _ = wait_launchd_job_gone(&domain, WDA_AGENT_LABEL);
            }
            restore_plist(&plist_path, original.as_deref());
        }
        return false;
    }
    let activated = if was_loaded && !plist_changed {
        // The cached launchd configuration is identical, so kickstart is safe.
        std::process::Command::new("launchctl")
            .args(["kickstart", "-k", &service])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    } else {
        // Any plist change (especially WDA_UDID) requires a full unload/reload;
        // kickstart alone would reuse launchd's cached old environment.
        if was_loaded {
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &service])
                .status();
        }
        wait_launchd_job_gone(&domain, WDA_AGENT_LABEL)
            && std::process::Command::new("launchctl")
                .args(["bootstrap", &domain])
                .arg(&plist_path)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
    };
    let verified = activated && launchd_job_loaded(&domain, WDA_AGENT_LABEL);
    if !verified && plist_changed {
        // Preserve the last known-good on-disk policy while leaving the
        // mismatched service down; never restart a cached old target.
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &service])
            .status();
        let _ = wait_launchd_job_gone(&domain, WDA_AGENT_LABEL);
        restore_plist(&plist_path, original.as_deref());
    }
    verified
}

/// Boot out the WDA LaunchAgent (so its KeepAlive stops rebuilding the runner).
/// Best-effort; ignored if it isn't loaded.
fn bootout_wda_agent() {
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{}/{WDA_AGENT_LABEL}", gui_domain())])
        .status();
}

/// Stop the on-phone WDA runner + relay and boot out its KeepAlive LaunchAgent
/// (FIRST — else KeepAlive would just rebuild the runner we're about to kill).
/// Used by the idle-release watchdog. Only the dedicated launchd job and setup
/// script are in scope: global process-name matching can kill an unrelated
/// developer's xcodebuild/iproxy process (or a different phone). If the script
/// is unavailable, orphan ownership cannot be proven and the stop deliberately
/// fails closed. Blocking — call under `spawn_blocking`.
fn stop_wda_runner_blocking(setup_sh: &str) -> bool {
    bootout_wda_agent();
    let stopped_by_owner = std::path::Path::new(setup_sh).is_file()
        && std::process::Command::new("bash")
            .arg(setup_sh)
            .arg("stop")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    let domain = gui_domain();
    let supervisor_gone = wait_launchd_job_gone(&domain, WDA_AGENT_LABEL);
    supervisor_gone && stopped_by_owner
}

/// Give idle-release observation priority over a background status probe while
/// preserving foreground control priority. The second pending check while
/// holding the probe slot closes the race with [`AppState::begin_wda_control`],
/// which increments the counter before taking the same slot.
fn abort_health_probe_for_idle(
    control_pending: &std::sync::atomic::AtomicUsize,
    probe_slot: &Mutex<Option<tokio::task::JoinHandle<()>>>,
) -> bool {
    use std::sync::atomic::Ordering;

    if control_pending.load(Ordering::Acquire) != 0 {
        return false;
    }
    let mut probe = recover(probe_slot.lock());
    if control_pending.load(Ordering::Acquire) != 0 {
        return false;
    }
    if let Some(probe) = probe.take() {
        probe.abort();
    }
    true
}

fn prepare_idle_wda_probe(state: &AppState) -> bool {
    abort_health_probe_for_idle(&state.wda_control_pending, &state.wda_health_probe)
}

/// Idle auto-release — the phone belongs to its owner first. When WDA is
/// configured and nobody has driven it for `PHONE_REMOTE_IDLE_RELEASE_SECS`
/// (default 300; `0` disables) and no viewer is streaming, stop the on-phone
/// WDA runner and boot out its KeepAlive LaunchAgent so the device is free for
/// hands-on use. The next `/agent/input` re-bootstraps WDA (see [`agent_input`]).
///
/// This transition only lets go of the configured Direct target; it never
/// opens, focuses, or otherwise touches the separate Mirror compatibility
/// backend.
///
/// No-op (and silent) when WDA isn't configured: a pure L3/mirror deployment has
/// no persistent on-device session to release.
pub fn spawn_idle_release_watchdog(state: Arc<AppState>) {
    if state.backend != crate::config::DeviceBackend::Direct
        || !state.managed_wda
        || state.wda.is_none()
    {
        return; // external/remote WDA and mirror mode are never lifecycle-managed here
    }
    let idle_secs = std::env::var("PHONE_REMOTE_IDLE_RELEASE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(300);
    if idle_secs == 0 {
        tracing::info!("idle auto-release disabled (PHONE_REMOTE_IDLE_RELEASE_SECS=0)");
        return;
    }
    let window = std::time::Duration::from_secs(idle_secs);
    tracing::info!("idle auto-release enabled: free the phone after {idle_secs}s idle");
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        const POLL: std::time::Duration = std::time::Duration::from_secs(20);
        let home = std::env::var("HOME").unwrap_or_default();
        let setup_sh = format!("{home}/.iphone-use/setup-wda.sh");
        let mut was_up = false;
        loop {
            tokio::time::sleep(POLL).await;
            if state.released.load(Ordering::Relaxed) {
                continue; // already let go — reconnect is on-demand (agent_input)
            }
            if state.wda_lifecycle.is_transitioning() {
                continue;
            }
            // Status polling must not starve lifecycle work forever. Cancel its
            // bounded health probe only when no real control is pending; control
            // always wins this arbitration.
            if !prepare_idle_wda_probe(&state) {
                continue;
            }
            // Observe the service transition before consulting the idle clock.
            // The daemon may have spent hours online while WDA was down; when a
            // human starts WDA later, that first up edge begins a fresh full
            // activity window instead of immediately releasing the new runner.
            let up = match &state.wda {
                Some(wda) => match wda.try_lock() {
                    Ok(client) => {
                        if state.wda_control_pending.load(Ordering::Acquire) != 0 {
                            drop(client);
                            continue;
                        }
                        tokio::time::timeout(std::time::Duration::from_millis(1500), client.is_up())
                            .await
                            .unwrap_or(false)
                    }
                    Err(_) => continue,
                },
                None => false,
            };
            if !up {
                was_up = false;
                continue;
            }
            if !was_up {
                was_up = true;
                state.touch_activity();
                continue;
            }
            if state.viewer_busy() {
                continue; // someone is watching the live feed
            }
            if state.wda_control_pending.load(Ordering::Acquire) != 0 {
                continue; // a real control request outranks idle release
            }
            if state.idle_for() < window {
                continue; // driven recently
            }
            // The WDA probe waits behind the shared client lock. Activity may
            // have resumed while we were awaiting it, so re-check before owning
            // the release transition.
            if state.viewer_busy()
                || state.idle_for() < window
                || state.wda_control_pending.load(Ordering::Acquire) != 0
            {
                continue;
            }
            if !state.wda_lifecycle.try_begin_releasing() {
                continue;
            }
            // Close the check→CAS race. Once `releasing=true`, request handlers
            // fail fast and cannot start a new device action.
            if state.viewer_busy()
                || state.idle_for() < window
                || state.wda_control_pending.load(Ordering::Acquire) != 0
            {
                state.wda_lifecycle.finish_releasing();
                continue;
            }
            tracing::info!(
                "idle {}s with no viewer — releasing the phone (stopping WDA)",
                state.idle_for().as_secs()
            );
            let script = setup_sh.clone();
            let stopped = tokio::task::spawn_blocking(move || stop_wda_runner_blocking(&script))
                .await
                .unwrap_or(false);
            let endpoint_down = match &state.wda {
                Some(wda) => {
                    let mut wda = wda.lock().await;
                    wda.invalidate_session();
                    !wda.is_up().await
                }
                None => true,
            };
            if stopped && endpoint_down {
                state.wda_actionable.store(false, Ordering::Relaxed);
                *recover(state.wda_health.lock()) = crate::wda::WdaHealth::down();
                state.released.store(true, Ordering::Release);
                was_up = false;
                tracing::info!("idle release confirmed: supervisor/runner stopped and WDA is down");
            } else {
                tracing::warn!(
                    "idle release was not confirmed (processes_stopped={stopped}, endpoint_down={endpoint_down}); keeping device state active"
                );
            }
            state.wda_lifecycle.finish_releasing();
        }
    });
}

/// `POST /agent/mode` — recover the currently configured backend.
/// Body: `{"mode":"mirror"}` for Mirror or `{"mode":"agent"}` for Direct.
///
/// The on-phone XCUITest runner (WDA, the L2 layer) monopolizes the device's
/// remote session: while it runs, iPhone Mirroring shows "Connection
/// Interrupted" and can never reconnect — even with the phone locked
/// (hardware A/B-verified, see docs/wda-setup.html pitfall ⑨). The configured
/// backend is therefore persistent and never changes here:
///
/// * Mirror + `mirror` — bring Mirroring frontmost and tap its "Try Again"
///   button through the L3 injector. Returns once dispatched;
///   callers poll `/agent/status` for `"mode":"mirror"` and verify pixels.
/// * Direct + `agent` — recover daemon-managed WDA using its persisted
///   canonical target. Poll until `reconnecting:false` and `drivable:true`.
///
/// A cross-backend value returns 409 and instructs the operator to persist
/// `PHONE_REMOTE_BACKEND` and restart.
async fn agent_mode(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Cookie OR bearer (same gate as screenshot/status) so the web client's
    // "Reconnect" button can recover its current backend without an agent token.
    match browser_or_agent_auth(&state, &headers) {
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
    if !has_phone_control_header(&headers) {
        return missing_phone_control_header_response();
    }
    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
    let mode = parsed
        .as_ref()
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_default();
    if state.backend == crate::config::DeviceBackend::Direct && mode == "mirror" {
        return with_security_headers(
            Response::builder()
                .status(StatusCode::CONFLICT)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"ok":false,"error":"backend_is_direct","hint":"set PHONE_REMOTE_BACKEND=mirror and restart the daemon to use the legacy compatibility backend"}"#,
                ))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    if state.backend == crate::config::DeviceBackend::Mirror && mode == "agent" {
        return with_security_headers(
            Response::builder()
                .status(StatusCode::CONFLICT)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"ok":false,"error":"backend_is_mirror","hint":"set PHONE_REMOTE_BACKEND=direct and restart the daemon to use device-side WDA control"}"#,
                ))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    if mode == "agent" && state.wda_lifecycle.is_releasing() {
        return with_security_headers(
            Response::builder()
                .status(StatusCode::CONFLICT)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::RETRY_AFTER, "5")
                .body(Body::from(
                    r#"{"ok":false,"error":"device_release_in_progress"}"#,
                ))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    // Optional target UDID. Invalid values are rejected rather than silently
    // falling back to another phone. Once Direct has a persisted target, a
    // transient request may not switch it behind status/idle recovery's back;
    // change PHONE_REMOTE_UDID and restart to make a target change atomic.
    let requested_udid = parsed
        .as_ref()
        .and_then(|v| v.get("udid").and_then(|u| u.as_str()))
        .filter(|u| !u.is_empty());
    if requested_udid.is_some_and(|u| !u.chars().all(|c| c.is_ascii_hexdigit() || c == '-')) {
        return with_security_headers(
            (StatusCode::BAD_REQUEST, "invalid target UDID").into_response(),
        );
    }
    if state.backend == crate::config::DeviceBackend::Direct {
        if mode == "agent" && state.managed_wda_pending {
            return with_security_headers(
                Response::builder()
                    .status(StatusCode::CONFLICT)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"ok":false,"error":"target_not_configured","hint":"run setup-wda.sh so PHONE_REMOTE_UDID is persisted before starting managed WDA"}"#,
                    ))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            );
        }
        if let (Some(configured), Some(requested)) = (state.device_udid.as_deref(), requested_udid)
        {
            if configured != requested {
                return with_security_headers(
                    Response::builder()
                        .status(StatusCode::CONFLICT)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"ok":false,"error":"target_change_requires_restart"}"#,
                        ))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                );
            }
        }
    }
    let udid = state
        .device_udid
        .clone()
        .or_else(|| requested_udid.map(String::from));
    let home = std::env::var("HOME").unwrap_or_default();
    let setup_sh = format!("{home}/.iphone-use/setup-wda.sh");
    match mode.as_str() {
        "mirror" => {
            // Mirror recovery never starts, stops, or reuses WDA. Installation
            // owns the explicit backend transition; runtime recovery only
            // reopens the selected compatibility backend.
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open")
                    .args(["-a", "iPhone Mirroring"])
                    .status();
                tokio::task::spawn_blocking(|| {
                    crate::macos::ensure_mirroring_frontmost(crate::macos::front_deadline())
                })
                .await
                .ok();
            }
            if let Some(ev) =
                crate::input_bridge::decode_control(r#"{"type":"tap","x":0.5,"y":0.65}"#)
            {
                recover(state.lease_state.lock()).acquire(
                    core::control::Holder::Agent("mirror-recovery".into()),
                    now_secs(),
                );
                state.injector.send(ev);
            }
            // Keep `switching` temporarily for older clients, while
            // `recovering` names the actual current-backend operation.
            let body = r#"{"ok":true,"mode":"mirror","recovering":true,"switching":true,"stopped_via_script":false}"#;
            with_security_headers(
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            )
        }
        "agent" => {
            if !state.managed_wda {
                return with_security_headers(
                    Response::builder()
                        .status(StatusCode::CONFLICT)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"ok":false,"error":"wda_is_externally_managed","recovery_owner":"external","hint":"restart the configured WDA endpoint on its owning host; this daemon will not run local setup or launchctl commands"}"#,
                        ))
                        .unwrap_or_else(|_| {
                            StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }),
                );
            }
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
            if !state.wda_lifecycle.try_begin_reconnecting() {
                return with_security_headers(
                    Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::RETRY_AFTER, "5")
                        .body(Body::from(
                            r#"{"ok":false,"reconnecting":true,"error":"reconnect_in_progress"}"#,
                        ))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                );
            }
            let log = format!("{home}/.iphone-use/wda-agent.log");
            let udid_env = udid.unwrap_or_default();
            let home_for_bootstrap = home.clone();
            let setup_for_bootstrap = setup_sh.clone();
            let log_for_bootstrap = log.clone();
            let spawned = tokio::task::spawn_blocking(move || {
                write_and_bootstrap_wda_agent(
                    &home_for_bootstrap,
                    &setup_for_bootstrap,
                    &log_for_bootstrap,
                    &udid_env,
                )
            })
            .await
            .unwrap_or(false);
            if spawned {
                // launchd acceptance is not device readiness. Keep the
                // transition visible and suppress duplicate reconnects until a
                // real action-level probe succeeds (or the 120s budget ends).
                *recover(state.wda_health.lock()) = crate::wda::WdaHealth::down();
                state
                    .wda_actionable
                    .store(false, std::sync::atomic::Ordering::Release);
                spawn_wda_readiness_wait(state.clone());
            } else {
                state.wda_lifecycle.finish_reconnecting();
            }
            let body = format!(
                r#"{{"ok":{spawned},"mode":"agent","starting":{spawned},"reconnecting":{spawned},"self_healing":true,"log":"{log}","hint":"if the phone is locked, unlock it once now — startup remains reconnecting until WDA can perform actions"}}"#
            );
            with_security_headers(
                Response::builder()
                    .status(if spawned {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_GATEWAY
                    })
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
fn normalized_wda_axis(value: f64, size: f64) -> anyhow::Result<f64> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        anyhow::bail!("normalized WDA coordinate must be within 0..=1");
    }
    if !size.is_finite() || size <= 2.0 {
        anyhow::bail!("WDA screen axis must be larger than two points");
    }
    Ok((value * size).clamp(1.0, size - 1.0))
}

pub(crate) async fn wda_swipe(
    w: &mut crate::wda::WdaClient,
    nx: f64,
    ny: f64,
    dx: f64,
    dy: f64,
) -> anyhow::Result<()> {
    let (sw, sh) = w.window_size().await?;
    let cx = normalized_wda_axis(nx, sw)?;
    let cy = normalized_wda_axis(ny, sh)?;
    let tx = swipe_travel(dx, sw);
    let ty = swipe_travel(dy, sh);
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
#[derive(Debug, PartialEq, Eq)]
enum DevicectlError {
    Timeout,
    TargetRequired(usize),
    Failed(String),
}

/// Run a CoreDevice child with a server-owned deadline in addition to
/// devicectl's own `--timeout`. The outer kill is essential: Command::output
/// otherwise waits forever if CoreDevice wedges, and an HTTP timeout would only
/// detach a child that could uninstall the app much later.
#[cfg(target_os = "macos")]
fn run_child_with_deadline(
    command: &mut std::process::Command,
    deadline: std::time::Duration,
) -> Result<std::process::Output, DevicectlError> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| DevicectlError::Failed(format!("spawn devicectl: {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(DevicectlError::Timeout);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(DevicectlError::Failed(format!("wait for devicectl: {e}")));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(target_os = "macos")]
fn detect_connected_device() -> Result<String, DevicectlError> {
    let out = run_child_with_deadline(
        std::process::Command::new("xcrun").args([
            "devicectl",
            "--quiet",
            "--timeout",
            "8",
            "list",
            "devices",
        ]),
        std::time::Duration::from_secs(12),
    )?;
    if !out.status.success() {
        return Err(DevicectlError::Failed(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut connected = Vec::new();
    for line in text.lines() {
        // States seen: "connected", "available (paired)", "unavailable".
        // Match the state as a token; substring matching would misclassify a
        // future "disconnected" state as usable.
        let is_connected = line.split_whitespace().any(|field| {
            field
                .trim_matches(|c: char| !c.is_ascii_alphabetic())
                .eq_ignore_ascii_case("connected")
        });
        if is_connected {
            for tok in line.split_whitespace() {
                // CoreDevice identifier is a 36-char UUID (8-4-4-4-12).
                if tok.len() == 36 && tok.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                    let candidate = tok.to_string();
                    if !connected.contains(&candidate) {
                        connected.push(candidate);
                    }
                }
            }
        }
    }
    match connected.len() {
        1 => Ok(connected.remove(0)),
        count => Err(DevicectlError::TargetRequired(count)),
    }
}

/// Uninstall an app (and its data container) from a paired device via
/// CoreDevice. This is the reliable "Delete App" primitive: WDA cannot remove
/// apps, and UI-driven deletion (Settings → Storage, or a home-screen
/// long-press) is flaky to automate. `udid` defaults to the connected device.
#[cfg(target_os = "macos")]
fn devicectl_uninstall(udid: Option<&str>, bundle: &str) -> Result<(), DevicectlError> {
    let device = match udid {
        Some(u) => u.to_string(),
        None => detect_connected_device()?,
    };
    let out = run_child_with_deadline(
        std::process::Command::new("xcrun").args([
            "devicectl",
            "--quiet",
            "--timeout",
            "15",
            "device",
            "uninstall",
            "app",
            "--device",
            &device,
            bundle,
        ]),
        std::time::Duration::from_secs(20),
    )?;
    if out.status.success() {
        Ok(())
    } else {
        Err(DevicectlError::Failed(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WdaControlOutcome {
    Applied,
    NotSent,
    Unsupported,
    InvalidElementSnapshot,
    StaleElementSnapshot,
    ElementNotFound,
    AmbiguousElement,
    InvalidElementTarget,
    Failed,
}

fn element_snapshot_id(rows: &[crate::wda::ElementRow]) -> anyhow::Result<String> {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let encoded = serde_json::to_vec(rows)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(encoded)))
}

/// How many recent element trees the daemon retains for `?since=` diffs.
/// An agent diffs against its own previous read, so a handful is plenty; a
/// miss simply degrades to the full tree.
const ELEMENT_SNAPSHOT_CACHE_CAP: usize = 8;

/// Retain `rows` under its snapshot token so a later `?since=` can diff
/// against it. Re-serving an already-cached snapshot refreshes its position.
fn remember_element_snapshot(
    state: &AppState,
    snapshot: &str,
    rows: &Arc<Vec<crate::wda::ElementRow>>,
) {
    let mut cache = recover(state.element_snapshots.lock());
    if let Some(position) = cache.iter().position(|(id, _)| id == snapshot) {
        cache.remove(position);
    }
    cache.push_back((snapshot.to_string(), rows.clone()));
    while cache.len() > ELEMENT_SNAPSHOT_CACHE_CAP {
        cache.pop_front();
    }
}

fn lookup_element_snapshot(
    state: &AppState,
    snapshot: &str,
) -> Option<Arc<Vec<crate::wda::ElementRow>>> {
    recover(state.element_snapshots.lock())
        .iter()
        .find(|(id, _)| id == snapshot)
        .map(|(_, rows)| rows.clone())
}

/// Index-level diff between two element trees (see [`diff_element_rows`]).
#[derive(Debug, PartialEq, Eq)]
struct ElementRowsDelta {
    /// Indexes into the CURRENT tree of rows with no identity match in the
    /// baseline — directly usable as `element` with the new snapshot token.
    added: Vec<usize>,
    /// Indexes into the CURRENT tree of rows whose identity exists in the
    /// baseline but whose state or geometry differs (value, rect, flags, depth).
    changed: Vec<usize>,
    /// Indexes into the BASELINE tree of rows that are gone from the current one.
    removed: Vec<usize>,
    /// Rows identical in both trees.
    unchanged: usize,
}

/// Diff two flattened element trees for `?since=` responses.
///
/// Rows are matched by semantic identity — `(kind, label, identifier,
/// placeholder)` — pairing duplicates in document order. A matched pair whose
/// remaining fields differ is `changed`; unmatched current rows are `added` and
/// unmatched baseline rows are `removed`. Index-positional matching would
/// misreport every row after one insertion, so identity matching is what keeps
/// a small UI change a small diff.
fn diff_element_rows(
    baseline: &[crate::wda::ElementRow],
    current: &[crate::wda::ElementRow],
) -> ElementRowsDelta {
    use std::collections::HashMap;

    type IdentityKey<'a> = (&'a str, &'a str, Option<&'a str>, Option<&'a str>);
    fn identity(row: &crate::wda::ElementRow) -> IdentityKey<'_> {
        (
            row.kind.as_str(),
            row.label.as_str(),
            row.identifier.as_deref(),
            row.placeholder.as_deref(),
        )
    }

    let mut baseline_by_identity: HashMap<IdentityKey<'_>, std::collections::VecDeque<usize>> =
        HashMap::new();
    for (index, row) in baseline.iter().enumerate() {
        baseline_by_identity
            .entry(identity(row))
            .or_default()
            .push_back(index);
    }

    let mut delta = ElementRowsDelta {
        added: Vec::new(),
        changed: Vec::new(),
        removed: Vec::new(),
        unchanged: 0,
    };
    let mut matched_baseline = vec![false; baseline.len()];
    for (index, row) in current.iter().enumerate() {
        match baseline_by_identity
            .get_mut(&identity(row))
            .and_then(std::collections::VecDeque::pop_front)
        {
            Some(baseline_index) => {
                matched_baseline[baseline_index] = true;
                if baseline[baseline_index] == *row {
                    delta.unchanged += 1;
                } else {
                    delta.changed.push(index);
                }
            }
            None => delta.added.push(index),
        }
    }
    delta.removed = matched_baseline
        .iter()
        .enumerate()
        .filter(|(_, matched)| !**matched)
        .map(|(index, _)| index)
        .collect();
    delta
}

/// Serialize a computed delta for the wire: `added`/`changed` carry the full
/// current rows with their indexes (so a follow-up snapshot-bound action needs
/// no re-read), `removed` is baseline indexes only.
fn elements_delta_json(
    delta: &ElementRowsDelta,
    current: &[crate::wda::ElementRow],
) -> serde_json::Value {
    let indexed = |indexes: &[usize]| -> Vec<serde_json::Value> {
        indexes
            .iter()
            .filter_map(|&index| {
                current.get(index).map(|row| {
                    serde_json::json!({
                        "index": index,
                        "element": row,
                    })
                })
            })
            .collect()
    };
    serde_json::json!({
        "added": indexed(&delta.added),
        "changed": indexed(&delta.changed),
        "removed": delta.removed,
        "unchanged": delta.unchanged,
    })
}

#[derive(Debug)]
enum SnapshotElementTapError {
    Invalid,
    Stale,
    NotFound,
    Ambiguous,
    /// The row was resolved fresh but cannot carry this action (no semantic
    /// locator where one is required, or a degenerate rectangle).
    InvalidTarget,
    BeforeDispatch(anyhow::Error),
    AfterDispatch(anyhow::Error),
}

/// Map a finished snapshot-bound element action onto the control outcome
/// grammar shared by every dispatcher: `Err(outcome)` is a terminal outcome the
/// caller returns as-is, `Ok(result)` feeds the dispatcher's normal
/// applied/failed handling.
fn snapshot_element_outcome(
    result: Result<(), SnapshotElementTapError>,
    w: &mut crate::wda::WdaClient,
    context: &str,
) -> Result<anyhow::Result<()>, WdaControlOutcome> {
    match result {
        Ok(()) => Ok(Ok(())),
        Err(SnapshotElementTapError::Invalid) => Err(WdaControlOutcome::InvalidElementSnapshot),
        Err(SnapshotElementTapError::Stale) => Err(WdaControlOutcome::StaleElementSnapshot),
        Err(SnapshotElementTapError::NotFound) => Err(WdaControlOutcome::ElementNotFound),
        Err(SnapshotElementTapError::Ambiguous) => Err(WdaControlOutcome::AmbiguousElement),
        Err(SnapshotElementTapError::InvalidTarget) => Err(WdaControlOutcome::InvalidElementTarget),
        Err(SnapshotElementTapError::BeforeDispatch(error)) => {
            w.invalidate_session();
            tracing::warn!("wda {context} failed before dispatch: {error:#}");
            Err(WdaControlOutcome::NotSent)
        }
        Err(SnapshotElementTapError::AfterDispatch(error)) => Ok(Err(error)),
    }
}

fn element_center(row: &crate::wda::ElementRow) -> Option<(f64, f64)> {
    let [x, y, width, height] = row.rect;
    if ![x, y, width, height].into_iter().all(f64::is_finite) || width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some((x + width / 2.0, y + height / 2.0))
}

fn snapshot_row_locator(row: &crate::wda::ElementRow) -> Option<AgentElementLocator> {
    let label = (!row.label.is_empty()).then(|| row.label.clone());
    let kind = (!row.kind.is_empty()).then(|| row.kind.clone());
    let identifier = row.identifier.clone().filter(|value| !value.is_empty());

    // A label needs a type to avoid widening a snapshot-bound action into a
    // different control with the same visible text. An accessibility
    // identifier is independently usable through WDA's native lookup.
    if identifier.is_none() && (label.is_none() || kind.is_none()) {
        return None;
    }

    Some(AgentElementLocator {
        label,
        identifier,
        kind,
        value: row.value.clone(),
        focused: row.focused,
        enabled: row.enabled,
        visible: row.visible,
    })
}

/// Parse a snapshot-bound target (`{"element":N,"snapshot":"…"}`), re-read the
/// live tree, and require the snapshot token to still match before any
/// mutation. Returns the fresh rows plus the selected index.
async fn fetch_snapshot_row(
    w: &mut crate::wda::WdaClient,
    value: &serde_json::Value,
) -> Result<(Vec<crate::wda::ElementRow>, usize), SnapshotElementTapError> {
    let index = value
        .get("element")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or(SnapshotElementTapError::Invalid)?;
    let expected_snapshot = value
        .get("snapshot")
        .and_then(serde_json::Value::as_str)
        .filter(|snapshot| !snapshot.is_empty())
        .ok_or(SnapshotElementTapError::Invalid)?;

    let rows = w
        .elements()
        .await
        .map_err(SnapshotElementTapError::BeforeDispatch)?;
    let current_snapshot =
        element_snapshot_id(&rows).map_err(SnapshotElementTapError::BeforeDispatch)?;
    if current_snapshot != expected_snapshot {
        return Err(SnapshotElementTapError::Stale);
    }
    if index >= rows.len() {
        return Err(SnapshotElementTapError::Invalid);
    }
    Ok((rows, index))
}

/// Resolve a fresh snapshot row to exactly one live WDA element through its
/// semantic locator. Rows without semantics cannot be addressed this way.
async fn resolve_snapshot_row_element(
    w: &mut crate::wda::WdaClient,
    row: &crate::wda::ElementRow,
) -> Result<String, SnapshotElementTapError> {
    let locator = snapshot_row_locator(row).ok_or(SnapshotElementTapError::InvalidTarget)?;
    let (using, value) = locator_wda_query(&locator).ok_or(SnapshotElementTapError::Invalid)?;
    let element_ids = w
        .find_elements(using, &value)
        .await
        .map_err(SnapshotElementTapError::BeforeDispatch)?;
    match element_ids.as_slice() {
        [] => Err(SnapshotElementTapError::NotFound),
        [element_id] => Ok(element_id.clone()),
        _ => Err(SnapshotElementTapError::Ambiguous),
    }
}

async fn tap_snapshot_element(
    w: &mut crate::wda::WdaClient,
    value: &serde_json::Value,
) -> Result<(), SnapshotElementTapError> {
    let (rows, index) = fetch_snapshot_row(w, value).await?;
    let row = &rows[index];
    if let Some(locator) = snapshot_row_locator(row) {
        // System-owned sheets and document pickers can publish stale or offset
        // rectangles while their native XCUIElement remains clickable. The
        // snapshot proves which semantic row the caller selected; re-resolve
        // that row and require exactly one native element before dispatching.
        // Never fall back to the suspect rectangle when semantic lookup fails.
        let (using, value) = locator_wda_query(&locator).ok_or(SnapshotElementTapError::Invalid)?;
        let element_ids = w
            .find_elements(using, &value)
            .await
            .map_err(SnapshotElementTapError::BeforeDispatch)?;
        let element_id = match element_ids.as_slice() {
            [] => return Err(SnapshotElementTapError::NotFound),
            [element_id] => element_id,
            _ => return Err(SnapshotElementTapError::Ambiguous),
        };
        return w
            .click_element(element_id)
            .await
            .map_err(SnapshotElementTapError::AfterDispatch);
    }

    let (x, y) = element_center(row).ok_or(SnapshotElementTapError::Invalid)?;
    w.tap_point(x, y)
        .await
        .map_err(SnapshotElementTapError::AfterDispatch)
}

/// `{"type":"set_value","element":N,"snapshot":"…","value":"…"}` — write a text
/// field's contents directly through WDA's `element/:id/value` instead of the
/// focus-tap-then-type dance. Clears first so the value REPLACES stale text;
/// an empty string means "clear the field".
async fn set_value_snapshot_element(
    w: &mut crate::wda::WdaClient,
    value: &serde_json::Value,
) -> Result<(), SnapshotElementTapError> {
    let text = value
        .get("value")
        .and_then(serde_json::Value::as_str)
        .ok_or(SnapshotElementTapError::Invalid)?
        .to_string();
    let (rows, index) = fetch_snapshot_row(w, value).await?;
    let element_id = resolve_snapshot_row_element(w, &rows[index]).await?;
    if text.is_empty() {
        // Clearing IS the requested mutation — report its real outcome.
        return w
            .clear_element(&element_id)
            .await
            .map_err(SnapshotElementTapError::AfterDispatch);
    }
    // Clear-then-type is one intentional compound action (same contract as
    // `text` with `clear:true`): the clear is best-effort, the type is still
    // dispatched at most once.
    if let Err(error) = w.clear_element(&element_id).await {
        tracing::warn!("wda clear_element before set_value: {error:#}");
    }
    w.type_into(&element_id, &text)
        .await
        .map_err(SnapshotElementTapError::AfterDispatch)
}

/// Shared swipe-travel curve: how far a scroll gesture actually moves for a
/// requested delta `d` along an axis of the given size.
fn swipe_travel(d: f64, axis: f64) -> f64 {
    if d == 0.0 {
        0.0
    } else {
        (d.abs() * 1.5).clamp(0.15 * axis, 0.75 * axis) * d.signum()
    }
}

/// `{"type":"scroll","element":N,"snapshot":"…","dx":…,"dy":…}` — scroll INSIDE
/// a specific element's rectangle (both gesture endpoints stay within it), so a
/// list scrolls without the gesture straying into a neighboring scroll view.
async fn scroll_snapshot_element(
    w: &mut crate::wda::WdaClient,
    value: &serde_json::Value,
) -> Result<(), SnapshotElementTapError> {
    let dx = value
        .get("dx")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let dy = value
        .get("dy")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    if !dx.is_finite() || !dy.is_finite() || (dx == 0.0 && dy == 0.0) {
        return Err(SnapshotElementTapError::Invalid);
    }
    let (rows, index) = fetch_snapshot_row(w, value).await?;
    let [x, y, width, height] = rows[index].rect;
    // A meaningful in-element gesture needs room for both endpoints.
    if ![x, y, width, height].into_iter().all(f64::is_finite) || width < 8.0 || height < 8.0 {
        return Err(SnapshotElementTapError::InvalidTarget);
    }
    let cx = x + width / 2.0;
    let cy = y + height / 2.0;
    let tx = swipe_travel(dx, width);
    let ty = swipe_travel(dy, height);
    // Same direction convention as full-screen scroll: positive dy starts low
    // and ends high (content moves down). Endpoints are inset 2pt so the touch
    // cannot land on the element's border.
    let x1 = (cx + tx / 2.0).clamp(x + 2.0, x + width - 2.0);
    let x2 = (cx - tx / 2.0).clamp(x + 2.0, x + width - 2.0);
    let y1 = (cy + ty / 2.0).clamp(y + 2.0, y + height - 2.0);
    let y2 = (cy - ty / 2.0).clamp(y + 2.0, y + height - 2.0);
    let dist = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt();
    let duration = (dist * 1.2).clamp(120.0, 600.0) as u64;
    w.swipe(x1, y1, x2, y2, duration)
        .await
        .map_err(SnapshotElementTapError::AfterDispatch)
}

#[derive(Debug)]
enum UniqueLabelTapError {
    NotFound,
    Ambiguous,
    InvalidTarget,
    BeforeDispatch(anyhow::Error),
    AfterDispatch(anyhow::Error),
}

async fn tap_unique_label(
    w: &mut crate::wda::WdaClient,
    label: &str,
) -> Result<(), UniqueLabelTapError> {
    let rows = w
        .elements()
        .await
        .map_err(UniqueLabelTapError::BeforeDispatch)?;
    let mut matches = rows.iter().filter(|row| row.label == label);
    let row = matches.next().ok_or(UniqueLabelTapError::NotFound)?;
    if matches.next().is_some() {
        return Err(UniqueLabelTapError::Ambiguous);
    }
    let (x, y) = element_center(row).ok_or(UniqueLabelTapError::InvalidTarget)?;
    w.tap_point(x, y)
        .await
        .map_err(UniqueLabelTapError::AfterDispatch)
}

async fn tap_unique_locator(
    w: &mut crate::wda::WdaClient,
    locator: &AgentElementLocator,
) -> Result<(), UniqueLabelTapError> {
    let rows = w
        .elements()
        .await
        .map_err(UniqueLabelTapError::BeforeDispatch)?;
    let mut matches = rows
        .iter()
        .filter(|row| agent_locator_matches(row, locator));
    let row = matches.next().ok_or(UniqueLabelTapError::NotFound)?;
    if matches.next().is_some() {
        return Err(UniqueLabelTapError::Ambiguous);
    }
    let _ = row;

    // `/source?format=json` can report stale/wrong rectangles for elements in
    // system-owned sheets (hardware-reproduced with the iOS share sheet's
    // "Save to Files" cell). A coordinate tap at that rectangle returns a WDA
    // success envelope while landing elsewhere. Re-resolve the already-proven
    // unique locator through WDA's live element query and invoke XCUIElement's
    // click action instead. Requiring exactly one returned element preserves
    // the fail-closed uniqueness contract across the second lookup.
    let (using, value) = locator_wda_query(locator).ok_or(UniqueLabelTapError::InvalidTarget)?;
    let element_ids = w
        .find_elements(using, &value)
        .await
        .map_err(UniqueLabelTapError::BeforeDispatch)?;
    let element_id = match element_ids.as_slice() {
        [] => return Err(UniqueLabelTapError::NotFound),
        [element_id] => element_id,
        _ => return Err(UniqueLabelTapError::Ambiguous),
    };
    w.click_element(element_id)
        .await
        .map_err(UniqueLabelTapError::AfterDispatch)
}

fn wda_predicate_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Build a fresh WDA element lookup for a strict agent locator.
///
/// WDA's predicate attributes do not expose the source tree's
/// `rawIdentifier`. When an identifier is the only condition, accessibility-id
/// is the closest native lookup and the caller still requires exactly one
/// result. When other fields exist, the source-tree precheck above enforces the
/// identifier while the predicate re-resolves every WDA-queryable condition.
fn locator_wda_query(locator: &AgentElementLocator) -> Option<(&'static str, String)> {
    let mut clauses = Vec::new();
    if let Some(kind) = &locator.kind {
        clauses.push(format!(
            "type == {}",
            wda_predicate_literal(&format!("XCUIElementType{kind}"))
        ));
    }
    if let Some(label) = &locator.label {
        let label = wda_predicate_literal(label);
        clauses.push(format!("(label == {label} OR name == {label})"));
    }
    if let Some(value) = &locator.value {
        clauses.push(format!("value == {}", wda_predicate_literal(value)));
    }
    if let Some(focused) = locator.focused {
        clauses.push(format!("focused == {}", u8::from(focused)));
    }
    if let Some(enabled) = locator.enabled {
        clauses.push(format!("enabled == {}", u8::from(enabled)));
    }
    if let Some(visible) = locator.visible {
        clauses.push(format!("visible == {}", u8::from(visible)));
    }
    if clauses.is_empty() {
        locator
            .identifier
            .as_ref()
            .map(|identifier| ("accessibility id", identifier.clone()))
    } else {
        Some(("predicate string", clauses.join(" AND ")))
    }
}

async fn wda_control_with_client(
    w: &mut crate::wda::WdaClient,
    actionable: &std::sync::atomic::AtomicBool,
    v: &serde_json::Value,
) -> WdaControlOutcome {
    use std::sync::atomic::Ordering;
    let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let r: anyhow::Result<()> = match typ {
        "tap" if v.get("label").is_some() => {
            let Some(label) = v
                .get("label")
                .and_then(serde_json::Value::as_str)
                .filter(|label| !label.is_empty())
            else {
                return WdaControlOutcome::Unsupported;
            };
            match tap_unique_label(w, label).await {
                Ok(()) => Ok(()),
                Err(UniqueLabelTapError::NotFound) => {
                    return WdaControlOutcome::ElementNotFound;
                }
                Err(UniqueLabelTapError::Ambiguous) => {
                    return WdaControlOutcome::AmbiguousElement;
                }
                Err(UniqueLabelTapError::InvalidTarget) => {
                    return WdaControlOutcome::InvalidElementTarget;
                }
                Err(UniqueLabelTapError::BeforeDispatch(error)) => {
                    w.invalidate_session();
                    tracing::warn!("wda control ({typ}) failed before dispatch: {error:#}");
                    return WdaControlOutcome::NotSent;
                }
                Err(UniqueLabelTapError::AfterDispatch(error)) => Err(error),
            }
        }
        "tap" if v.get("element").is_some() => {
            let result = tap_snapshot_element(w, v).await;
            match snapshot_element_outcome(result, w, "control snapshot tap") {
                Ok(result) => result,
                Err(outcome) => return outcome,
            }
        }
        "set_value" => {
            let result = set_value_snapshot_element(w, v).await;
            match snapshot_element_outcome(result, w, "control set_value") {
                Ok(result) => result,
                Err(outcome) => return outcome,
            }
        }
        "scroll" if v.get("element").is_some() => {
            let result = scroll_snapshot_element(w, v).await;
            match snapshot_element_outcome(result, w, "control element scroll") {
                Ok(result) => result,
                Err(outcome) => return outcome,
            }
        }
        "tap" => {
            match (
                v.get("x").and_then(|x| x.as_f64()),
                v.get("y").and_then(|y| y.as_f64()),
            ) {
                (Some(x), Some(y)) if (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) => {
                    async {
                        let (sw, sh) = w.window_size().await?;
                        w.tap_point(normalized_wda_axis(x, sw)?, normalized_wda_axis(y, sh)?)
                            .await
                    }
                    .await
                }
                _ => return WdaControlOutcome::Unsupported,
            }
        }
        "longpress" => match (
            v.get("x").and_then(|x| x.as_f64()),
            v.get("y").and_then(|y| y.as_f64()),
        ) {
            (Some(x), Some(y)) if (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) => {
                async {
                    let (sw, sh) = w.window_size().await?;
                    let duration = v.get("duration_ms").and_then(|x| x.as_u64()).unwrap_or(600);
                    w.longpress_point(
                        normalized_wda_axis(x, sw)?,
                        normalized_wda_axis(y, sh)?,
                        duration,
                    )
                    .await
                }
                .await
            }
            _ => return WdaControlOutcome::Unsupported,
        },
        "scroll" => {
            let nx = v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.5);
            let ny = v.get("y").and_then(|y| y.as_f64()).unwrap_or(0.5);
            let dx = v.get("dx").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let dy = v.get("dy").and_then(|y| y.as_f64()).unwrap_or(0.0);
            if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) || (dx == 0.0 && dy == 0.0)
            {
                return WdaControlOutcome::Unsupported;
            }
            wda_swipe(w, nx, ny, dx, dy).await
        }
        "text" => match v.get("text").and_then(|t| t.as_str()) {
            Some(t) => w.keys(t).await,
            None => return WdaControlOutcome::Unsupported,
        },
        "key" => match v.get("name").and_then(|n| n.as_str()) {
            Some("dismiss" | "hide") => w.dismiss_keyboard().await,
            Some(name) => w.named_key(name).await,
            None => return WdaControlOutcome::Unsupported,
        },
        "keyboard" => w.dismiss_keyboard().await,
        "home" => w.press_home().await,
        "back" => w.back().await,
        "shortcut" => match v.get("name").and_then(|n| n.as_str()) {
            Some("home") => w.press_home().await,
            // Spotlight's Search pill acknowledges coordinate taps without
            // always opening. Resolve and click its accessibility element, then
            // verify the search field before reporting success.
            Some("spotlight") => w.open_spotlight().await,
            // App switcher: the swipe-up-from-the-home-indicator is a system
            // gesture WDA can't synthesize (hardware-verified: from Home it goes
            // Home, from an app the swipe is absorbed — the switcher never opens).
            // There is no WDA element to tap either, so it's unreachable in agent
            // mode. Report unhandled; the web client shows a hint instead of
            // sending a no-op. (Works in mirror mode via the L3 path.)
            Some("switcher") => return WdaControlOutcome::Unsupported,
            _ => return WdaControlOutcome::Unsupported,
        },
        // A whole swipe gesture as ONE on-device drag (start→end). The web client
        // sends this on pointer-up in agent mode instead of streaming per-move
        // scroll deltas (WDA has no scroll-wheel; a delta stream turned into a
        // storm of discrete swipes that kept scrolling after release — issue: the
        // screen "kept moving" after the finger stopped).
        "swipe" | "drag" => {
            let g = |k: &str| v.get(k).and_then(|x| x.as_f64());
            match (g("x1"), g("y1"), g("x2"), g("y2")) {
                (Some(x1), Some(y1), Some(x2), Some(y2))
                    if [x1, y1, x2, y2]
                        .into_iter()
                        .all(|n| (0.0..=1.0).contains(&n)) =>
                {
                    async {
                        let (sw, sh) = w.window_size().await?;
                        let (ax, ay, bx, by) = (
                            normalized_wda_axis(x1, sw)?,
                            normalized_wda_axis(y1, sh)?,
                            normalized_wda_axis(x2, sw)?,
                            normalized_wda_axis(y2, sh)?,
                        );
                        let dist = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
                        let duration = v
                            .get("duration_ms")
                            .and_then(|x| x.as_u64())
                            .unwrap_or_else(|| (dist * 0.9).clamp(120.0, 500.0) as u64);
                        if typ == "drag" {
                            let hold = v.get("hold_ms").and_then(|x| x.as_u64()).unwrap_or(500);
                            w.drag(ax, ay, bx, by, hold, duration).await
                        } else {
                            w.swipe(ax, ay, bx, by, duration).await
                        }
                    }
                    .await
                }
                _ => return WdaControlOutcome::Unsupported,
            }
        }
        // Streaming down/up/move is a Mirroring-era protocol. Direct gestures
        // arrive atomically as tap/longpress/swipe/drag.
        _ => return WdaControlOutcome::Unsupported,
    };
    match r {
        Ok(()) => {
            actionable.store(true, Ordering::Relaxed);
            WdaControlOutcome::Applied
        }
        Err(e) => {
            // A WDA call that should have worked failed. Direct callers fail
            // closed; the explicit mirror backend may choose its compatibility
            // path.
            actionable.store(false, Ordering::Relaxed);
            w.invalidate_session();
            tracing::warn!("wda control ({typ}): {e:#}");
            WdaControlOutcome::Failed
        }
    }
}

/// `POST /control` — cookie-authenticated browser control for the direct backend.
///
/// The custom request header makes cross-origin form CSRF impossible in open
/// mode (a browser must preflight it, and this server exposes no CORS policy).
/// Unlike the old data channel this endpoint acknowledges every command; the
/// client must not show success unless it receives `{"ok":true}`.
const DIRECT_CONTROL_MAX_TTL_MS: u64 = 2500;
const AGENT_INPUT_WDA_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
const AGENT_ACTIONS_MAX_BODY_BYTES: usize = 64 * 1024;
const AGENT_ACTIONS_MAX_STEPS: usize = 24;
const AGENT_ACTIONS_MAX_WAIT_MS: u64 = 10_000;
const AGENT_ACTIONS_MAX_PAUSE_MS: u64 = 3_000;
const AGENT_ACTIONS_MAX_DECLARED_WAIT_MS: u64 = 60_000;
const AGENT_ACTIONS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(75);

fn wda_deadline_response(dispatched: bool) -> Response {
    let (status, body) = if dispatched {
        (
            StatusCode::GATEWAY_TIMEOUT,
            r#"{"ok":false,"error":"outcome_unknown","outcome":"unknown","retry_safe":false}"#,
        )
    } else {
        (
            StatusCode::REQUEST_TIMEOUT,
            r#"{"ok":false,"error":"not_sent","outcome":"not_sent","retry_safe":true}"#,
        )
    };
    with_security_headers(
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn wda_failed_after_dispatch_response() -> Response {
    with_security_headers(
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"ok":false,"error":"outcome_unknown","outcome":"unknown","retry_safe":false}"#,
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn wda_failed_before_dispatch_response() -> Response {
    with_security_headers(
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"ok":false,"error":"wda_pre_dispatch_failed","outcome":"not_sent","retry_safe":true}"#,
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn invalid_element_snapshot_response() -> Response {
    with_security_headers(
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"ok":false,"error":"invalid_element_snapshot","outcome":"not_sent","retry_safe":true}"#,
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn stale_element_snapshot_response() -> Response {
    with_security_headers(
        Response::builder()
            .status(StatusCode::CONFLICT)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"ok":false,"error":"stale_element_snapshot","outcome":"not_sent","retry_safe":true,"hint":"refresh /agent/elements and choose the element again"}"#,
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn element_not_found_response() -> Response {
    element_resolution_response(
        r#"{"ok":false,"error":"element_not_found","outcome":"not_sent","retry_safe":true,"hint":"refresh /agent/elements and use an exact current label or snapshot-bound element index"}"#,
    )
}

fn ambiguous_element_response() -> Response {
    element_resolution_response(
        r#"{"ok":false,"error":"ambiguous_element_label","outcome":"not_sent","retry_safe":true,"hint":"refresh /agent/elements, disambiguate by identifier/kind/state, then send element plus snapshot"}"#,
    )
}

fn invalid_element_target_response() -> Response {
    element_resolution_response(
        r#"{"ok":false,"error":"invalid_element_target","outcome":"not_sent","retry_safe":true,"hint":"the matched element has no finite positive-size hit target; refresh /agent/elements and choose another locator"}"#,
    )
}

fn element_resolution_response(body: &'static str) -> Response {
    with_security_headers(
        Response::builder()
            .status(StatusCode::UNPROCESSABLE_ENTITY)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

/// Execute one Direct agent action exactly once.
///
/// Locator and geometry reads may precede the mutation, but once a mutating WDA
/// request has been sent this function never rebuilds the session and replays
/// it. A lost response is therefore surfaced as an uncertain outcome rather
/// than turning a tap, swipe, Home press, or text insertion into two actions.
async fn direct_agent_action(
    w: &mut crate::wda::WdaClient,
    actionable: &std::sync::atomic::AtomicBool,
    value: &serde_json::Value,
) -> WdaControlOutcome {
    use std::sync::atomic::Ordering;

    let typ = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let custom_result: Option<anyhow::Result<()>> = match typ {
        "launch_app" => {
            let bundle = value
                .get("bundle")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    value
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .and_then(system_app_bundle)
                        .map(str::to_string)
                });
            let Some(bundle) = bundle else {
                return WdaControlOutcome::Unsupported;
            };
            Some(w.launch_app(&bundle).await)
        }
        "tap" if value.get("label").is_some() => {
            let Some(label) = value
                .get("label")
                .and_then(serde_json::Value::as_str)
                .filter(|label| !label.is_empty())
            else {
                return WdaControlOutcome::Unsupported;
            };
            match tap_unique_label(w, label).await {
                Ok(()) => Some(Ok(())),
                Err(UniqueLabelTapError::NotFound) => {
                    return WdaControlOutcome::ElementNotFound;
                }
                Err(UniqueLabelTapError::Ambiguous) => {
                    return WdaControlOutcome::AmbiguousElement;
                }
                Err(UniqueLabelTapError::InvalidTarget) => {
                    return WdaControlOutcome::InvalidElementTarget;
                }
                Err(UniqueLabelTapError::BeforeDispatch(error)) => {
                    w.invalidate_session();
                    tracing::warn!("wda agent action ({typ}) failed before dispatch: {error:#}");
                    return WdaControlOutcome::NotSent;
                }
                Err(UniqueLabelTapError::AfterDispatch(error)) => Some(Err(error)),
            }
        }
        "tap" if value.get("element").is_some() => {
            let result = tap_snapshot_element(w, value).await;
            match snapshot_element_outcome(result, w, "agent snapshot tap") {
                Ok(result) => Some(result),
                Err(outcome) => return outcome,
            }
        }
        "tap_locator" => {
            let Some(locator) = value
                .get("locator")
                .cloned()
                .and_then(|locator| serde_json::from_value::<AgentElementLocator>(locator).ok())
                .filter(locator_has_condition)
            else {
                return WdaControlOutcome::Unsupported;
            };
            match tap_unique_locator(w, &locator).await {
                Ok(()) => Some(Ok(())),
                Err(UniqueLabelTapError::NotFound) => {
                    return WdaControlOutcome::ElementNotFound;
                }
                Err(UniqueLabelTapError::Ambiguous) => {
                    return WdaControlOutcome::AmbiguousElement;
                }
                Err(UniqueLabelTapError::InvalidTarget) => {
                    return WdaControlOutcome::InvalidElementTarget;
                }
                Err(UniqueLabelTapError::BeforeDispatch(error)) => {
                    w.invalidate_session();
                    tracing::warn!("wda agent action ({typ}) failed before dispatch: {error:#}");
                    return WdaControlOutcome::NotSent;
                }
                Err(UniqueLabelTapError::AfterDispatch(error)) => Some(Err(error)),
            }
        }
        "picker" => {
            let Some(target) = value.get("value").and_then(serde_json::Value::as_str) else {
                return WdaControlOutcome::Unsupported;
            };
            let column = value
                .get("column")
                .and_then(serde_json::Value::as_u64)
                .and_then(|column| usize::try_from(column).ok())
                .unwrap_or(0);
            Some(w.set_picker(column, target).await)
        }
        "text"
            if value
                .get("clear")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false) =>
        {
            let Some(text) = value.get("text").and_then(serde_json::Value::as_str) else {
                return WdaControlOutcome::Unsupported;
            };
            Some(
                async {
                    // Clearing and typing are one intentional compound action.
                    // A clear error is best-effort, but the text insertion is
                    // still dispatched at most once.
                    if let Err(error) = w.clear_active().await {
                        tracing::warn!("wda clear_active before text: {error:#}");
                    }
                    w.keys(text).await
                }
                .await,
            )
        }
        _ => None,
    };

    let Some(result) = custom_result else {
        return wda_control_with_client(w, actionable, value).await;
    };
    match result {
        Ok(()) => {
            actionable.store(true, Ordering::Release);
            WdaControlOutcome::Applied
        }
        Err(error) => {
            actionable.store(false, Ordering::Release);
            w.invalidate_session();
            tracing::warn!("wda agent action ({typ}): {error:#}");
            WdaControlOutcome::Failed
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlFreshnessError {
    Missing,
    Invalid,
}

fn direct_control_deadline(
    value: &serde_json::Value,
    monotonic_now: tokio::time::Instant,
) -> Result<tokio::time::Instant, ControlFreshnessError> {
    let ttl_ms = value
        .get("ttl_ms")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ControlFreshnessError::Missing)?;
    // `issued_at_ms` is optional audit metadata only. The browser can be on a
    // different phone/computer whose wall clock legitimately differs from the
    // Mac, so freshness is based exclusively on a server-side monotonic receipt
    // deadline.
    if value
        .get("issued_at_ms")
        .is_some_and(|issued| !issued.is_u64())
        || ttl_ms == 0
        || ttl_ms > DIRECT_CONTROL_MAX_TTL_MS
    {
        return Err(ControlFreshnessError::Invalid);
    }
    Ok(monotonic_now + std::time::Duration::from_millis(ttl_ms))
}

async fn direct_control(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !is_authed(&state, &headers) {
        return with_security_headers(
            (
                StatusCode::UNAUTHORIZED,
                r#"{"ok":false,"error":"unauthorized"}"#,
            )
                .into_response(),
        );
    }
    if !has_phone_control_header(&headers) {
        return missing_phone_control_header_response();
    }
    if state.backend != crate::config::DeviceBackend::Direct {
        return with_security_headers(
            (
                StatusCode::CONFLICT,
                r#"{"ok":false,"error":"legacy_mirror_uses_webrtc"}"#,
            )
                .into_response(),
        );
    }
    if state.managed_wda_pending {
        return target_not_configured_response();
    }
    let value: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(value) if value.is_object() => value,
        _ => {
            return with_security_headers(
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"ok":false,"error":"invalid_control_message"}"#,
                )
                    .into_response(),
            );
        }
    };
    let deadline = match direct_control_deadline(&value, tokio::time::Instant::now()) {
        Ok(deadline) => deadline,
        Err(ControlFreshnessError::Missing | ControlFreshnessError::Invalid) => {
            return with_security_headers(
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"ok":false,"error":"invalid_control_deadline"}"#,
                )
                    .into_response(),
            );
        }
    };
    let lifecycle = state.wda_lifecycle.current();
    let releasing = lifecycle == WdaLifecycleTransition::Releasing;
    let reconnecting = lifecycle == WdaLifecycleTransition::Reconnecting;
    let released = state.released.load(std::sync::atomic::Ordering::Relaxed);
    if releasing || reconnecting || released {
        let error = if releasing {
            "releasing"
        } else if reconnecting {
            "reconnecting"
        } else {
            "released"
        };
        return with_security_headers(
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::RETRY_AFTER, "5")
                .body(Body::from(format!(
                    r#"{{"ok":false,"error":"{error}","reconnecting":{reconnecting}}}"#
                )))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    let Some(wda) = &state.wda else {
        return with_security_headers(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"ok":false,"error":"wda_not_configured"}"#,
            )
                .into_response(),
        );
    };
    if tokio::time::Instant::now() >= deadline {
        return wda_deadline_response(false);
    }
    let _priority = state.begin_wda_control();
    let dispatched = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dispatch_marker = dispatched.clone();
    // One deadline covers BOTH mutex acquisition and the WDA action. Re-check
    // lifecycle after acquiring the mutex: a request that sat behind a status
    // probe or previous gesture must never execute after its browser was already
    // told it timed out or after release began.
    let outcome = tokio::time::timeout_at(deadline, async {
        let mut client = wda.lock().await;
        if tokio::time::Instant::now() >= deadline
            || state.wda_lifecycle.is_transitioning()
            || state.released.load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        dispatch_marker.store(true, std::sync::atomic::Ordering::Release);
        Some(wda_control_with_client(&mut client, &state.wda_actionable, &value).await)
    })
    .await;
    let outcome = match outcome {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return wda_deadline_response(false),
        Err(_) => {
            return wda_deadline_response(dispatched.load(std::sync::atomic::Ordering::Acquire));
        }
    };
    if outcome == WdaControlOutcome::Applied {
        state.touch_activity();
        let locked = recover(state.wda_health.lock()).locked;
        *recover(state.wda_health.lock()) = crate::wda::WdaHealth {
            up: true,
            actionable: true,
            locked,
        };
        return with_security_headers(
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ok":true}"#))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }

    if outcome == WdaControlOutcome::Failed {
        let locked = recover(state.wda_health.lock()).locked;
        *recover(state.wda_health.lock()) = crate::wda::WdaHealth {
            up: true,
            actionable: false,
            locked,
        };
        return wda_failed_after_dispatch_response();
    }
    if outcome == WdaControlOutcome::NotSent {
        mark_wda_read_path_unactionable(&state);
        return wda_failed_before_dispatch_response();
    }
    if outcome == WdaControlOutcome::InvalidElementSnapshot {
        return invalid_element_snapshot_response();
    }
    if outcome == WdaControlOutcome::StaleElementSnapshot {
        return stale_element_snapshot_response();
    }
    if outcome == WdaControlOutcome::ElementNotFound {
        return element_not_found_response();
    }
    if outcome == WdaControlOutcome::AmbiguousElement {
        return ambiguous_element_response();
    }
    if outcome == WdaControlOutcome::InvalidElementTarget {
        return invalid_element_target_response();
    }
    with_security_headers(
        Response::builder()
            .status(StatusCode::UNPROCESSABLE_ENTITY)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"ok":false,"error":"unsupported_control","outcome":"not_sent","retry_safe":false}"#,
            ))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentActionsRequest {
    steps: Vec<AgentActionStep>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AgentActionStep {
    /// Execute one existing `/agent/input` action. `after_ms` is only a short
    /// animation settle; use a following `wait_for` step for correctness.
    Action {
        action: serde_json::Value,
        #[serde(default)]
        after_ms: u64,
    },
    /// Poll the WDA element tree until every positive locator and the optional
    /// application match, while every negative locator remains absent.
    WaitFor {
        expect: AgentUiExpectation,
        #[serde(default = "default_agent_actions_wait_ms")]
        timeout_ms: u64,
        #[serde(default = "default_agent_actions_poll_ms")]
        poll_ms: u64,
    },
    /// A bounded animation pause. This is deliberately small and should not be
    /// used instead of a semantic `wait_for` gate.
    Pause { ms: u64 },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentUiExpectation {
    #[serde(default)]
    application: Option<String>,
    #[serde(default)]
    present: Vec<AgentElementLocator>,
    #[serde(default)]
    absent: Vec<AgentElementLocator>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentElementLocator {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    focused: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    visible: Option<bool>,
}

fn default_agent_actions_wait_ms() -> u64 {
    5_000
}

fn default_agent_actions_poll_ms() -> u64 {
    250
}

fn agent_actions_json(status: StatusCode, value: serde_json::Value) -> Response {
    with_security_headers(
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn agent_actions_invalid(detail: impl Into<String>) -> Response {
    agent_actions_json(
        StatusCode::BAD_REQUEST,
        serde_json::json!({
            "ok": false,
            "error": "invalid_actions_request",
            "detail": detail.into(),
            "outcome": "not_sent",
            "retry_safe": true
        }),
    )
}

fn locator_has_condition(locator: &AgentElementLocator) -> bool {
    locator.label.is_some()
        || locator.identifier.is_some()
        || locator.kind.is_some()
        || locator.value.is_some()
        || locator.focused.is_some()
        || locator.enabled.is_some()
        || locator.visible.is_some()
}

fn finite_unit(value: Option<f64>) -> bool {
    value.is_some_and(|value| value.is_finite() && (0.0..=1.0).contains(&value))
}

fn validate_agent_action_value(
    action: &serde_json::Map<String, serde_json::Value>,
    index: usize,
) -> Result<(), String> {
    let Some(typ) = action
        .get("type")
        .and_then(serde_json::Value::as_str)
        .filter(|typ| !typ.is_empty())
    else {
        return Err(format!(
            "steps[{index}].action.type must be a non-empty string"
        ));
    };
    if typ == "uninstall" {
        return Err(format!(
            "steps[{index}] cannot batch destructive uninstall actions"
        ));
    }

    let invalid = |detail: &str| Err(format!("steps[{index}].action {detail}"));
    match typ {
        "tap" => {
            let modes = usize::from(action.contains_key("label"))
                + usize::from(action.contains_key("element"))
                + usize::from(action.contains_key("x") || action.contains_key("y"));
            if modes != 1 {
                return invalid(
                    "tap must use exactly one target mode: label, element+snapshot, or x+y",
                );
            }
            if action.contains_key("label") {
                if !action
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|label| !label.is_empty() && label.chars().count() <= 500)
                {
                    return invalid("tap label must contain 1 to 500 characters");
                }
            } else if action.contains_key("element") {
                if action
                    .get("element")
                    .and_then(serde_json::Value::as_u64)
                    .is_none()
                    || !action
                        .get("snapshot")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|snapshot| {
                            !snapshot.is_empty() && snapshot.chars().count() <= 200
                        })
                {
                    return invalid("indexed tap needs a non-negative element and snapshot");
                }
            } else if !finite_unit(action.get("x").and_then(serde_json::Value::as_f64))
                || !finite_unit(action.get("y").and_then(serde_json::Value::as_f64))
            {
                return invalid("tap coordinates must be finite values from 0 to 1");
            }
        }
        "tap_locator" => {
            let locator = action
                .get("locator")
                .cloned()
                .and_then(|locator| serde_json::from_value::<AgentElementLocator>(locator).ok());
            if !locator.as_ref().is_some_and(locator_has_condition) {
                return invalid(
                    "tap_locator needs one non-empty strict locator with supported fields",
                );
            }
        }
        "longpress" => {
            if !finite_unit(action.get("x").and_then(serde_json::Value::as_f64))
                || !finite_unit(action.get("y").and_then(serde_json::Value::as_f64))
                || action
                    .get("duration_ms")
                    .is_some_and(|duration| duration.as_u64().is_none_or(|value| value > 10_000))
            {
                return invalid("longpress needs x/y from 0 to 1 and duration_ms at most 10000");
            }
        }
        "scroll" => {
            let dx = action
                .get("dx")
                .map_or(Some(0.0), serde_json::Value::as_f64);
            let dy = action
                .get("dy")
                .map_or(Some(0.0), serde_json::Value::as_f64);
            let valid_deltas = dx.is_some_and(|value| value.is_finite() && value.abs() <= 1_000.0)
                && dy.is_some_and(|value| value.is_finite() && value.abs() <= 1_000.0)
                && !(dx == Some(0.0) && dy == Some(0.0));
            if action.contains_key("element") {
                // Element-relative scroll: the gesture stays inside that
                // element's rectangle, so x/y have no meaning here.
                if action.contains_key("x") || action.contains_key("y") {
                    return invalid("element scroll does not take x/y coordinates");
                }
                if action
                    .get("element")
                    .and_then(serde_json::Value::as_u64)
                    .is_none()
                    || !action
                        .get("snapshot")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|snapshot| {
                            !snapshot.is_empty() && snapshot.chars().count() <= 200
                        })
                {
                    return invalid("element scroll needs a non-negative element and snapshot");
                }
                if !valid_deltas {
                    return invalid("scroll geometry is invalid");
                }
            } else {
                let x = action.get("x").map_or(Some(0.5), serde_json::Value::as_f64);
                let y = action.get("y").map_or(Some(0.5), serde_json::Value::as_f64);
                if !finite_unit(x) || !finite_unit(y) || !valid_deltas {
                    return invalid("scroll geometry is invalid");
                }
            }
        }
        "set_value" => {
            if action
                .get("element")
                .and_then(serde_json::Value::as_u64)
                .is_none()
                || !action
                    .get("snapshot")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|snapshot| !snapshot.is_empty() && snapshot.chars().count() <= 200)
                || action
                    .get("value")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|value| value.chars().count() > 1_000)
            {
                return invalid(
                    "set_value needs element, snapshot, and a string value up to 1000 characters",
                );
            }
        }
        "text" => {
            if action
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|text| text.chars().count() > 1_000)
                || action
                    .get("clear")
                    .is_some_and(|clear| clear.as_bool().is_none())
            {
                return invalid(
                    "text needs a string up to 1000 characters and optional bool clear",
                );
            }
        }
        "key" => {
            if !action
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| {
                    matches!(
                        name,
                        "return"
                            | "enter"
                            | "escape"
                            | "space"
                            | "tab"
                            | "delete"
                            | "backspace"
                            | "up"
                            | "down"
                            | "left"
                            | "right"
                            | "dismiss"
                            | "hide"
                    )
                })
            {
                return invalid("key name is unsupported");
            }
        }
        "keyboard" | "home" | "back" => {}
        "shortcut" => {
            if !action
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| matches!(name, "home" | "spotlight"))
            {
                return invalid("shortcut must be home or spotlight");
            }
        }
        "swipe" | "drag" => {
            if !["x1", "y1", "x2", "y2"]
                .into_iter()
                .all(|key| finite_unit(action.get(key).and_then(serde_json::Value::as_f64)))
                || action
                    .get("duration_ms")
                    .is_some_and(|value| value.as_u64().is_none_or(|value| value > 10_000))
                || action
                    .get("hold_ms")
                    .is_some_and(|value| value.as_u64().is_none_or(|value| value > 10_000))
            {
                return invalid("swipe/drag geometry or timing is invalid");
            }
        }
        "launch_app" => {
            let has_bundle = action.contains_key("bundle");
            let has_name = action.contains_key("name");
            let valid_bundle = action
                .get("bundle")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|bundle| {
                    !bundle.is_empty()
                        && bundle.len() <= 200
                        && bundle
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                });
            let valid_name = action
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| system_app_bundle(name).is_some());
            if has_bundle == has_name || (has_bundle && !valid_bundle) || (has_name && !valid_name)
            {
                return invalid(
                    "launch_app needs exactly one valid bundle or supported system-app name",
                );
            }
        }
        "picker" => {
            if !action
                .get("value")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty() && value.chars().count() <= 500)
                || action
                    .get("column")
                    .is_some_and(|column| column.as_u64().is_none_or(|value| value > 20))
            {
                return invalid("picker needs a value up to 500 characters and column 0 to 20");
            }
        }
        _ => return invalid(&format!("has unsupported type {typ:?}")),
    }
    Ok(())
}

fn validate_agent_actions(request: &AgentActionsRequest) -> Result<(), String> {
    if request.steps.is_empty() {
        return Err("steps must contain at least one step".to_string());
    }
    if request.steps.len() > AGENT_ACTIONS_MAX_STEPS {
        return Err(format!(
            "steps exceeds the maximum of {AGENT_ACTIONS_MAX_STEPS}"
        ));
    }

    let mut declared_wait_ms = 0_u64;
    for (index, step) in request.steps.iter().enumerate() {
        match step {
            AgentActionStep::Action { action, after_ms } => {
                let Some(action) = action.as_object() else {
                    return Err(format!("steps[{index}].action must be an object"));
                };
                validate_agent_action_value(action, index)?;
                if *after_ms > AGENT_ACTIONS_MAX_PAUSE_MS {
                    return Err(format!(
                        "steps[{index}].after_ms exceeds {AGENT_ACTIONS_MAX_PAUSE_MS}"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(*after_ms);
            }
            AgentActionStep::WaitFor {
                expect,
                timeout_ms,
                poll_ms,
            } => {
                if expect.application.is_none()
                    && expect.present.is_empty()
                    && expect.absent.is_empty()
                {
                    return Err(format!(
                        "steps[{index}].expect must include application, present, or absent"
                    ));
                }
                if expect
                    .application
                    .as_ref()
                    .is_some_and(|application| application.is_empty())
                {
                    return Err(format!(
                        "steps[{index}].expect.application must not be empty"
                    ));
                }
                if expect
                    .present
                    .iter()
                    .chain(expect.absent.iter())
                    .any(|locator| !locator_has_condition(locator))
                {
                    return Err(format!("steps[{index}] contains an empty element locator"));
                }
                if *timeout_ms == 0 || *timeout_ms > AGENT_ACTIONS_MAX_WAIT_MS {
                    return Err(format!(
                        "steps[{index}].timeout_ms must be between 1 and {AGENT_ACTIONS_MAX_WAIT_MS}"
                    ));
                }
                if !(50..=1_000).contains(poll_ms) {
                    return Err(format!(
                        "steps[{index}].poll_ms must be between 50 and 1000"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(*timeout_ms);
            }
            AgentActionStep::Pause { ms } => {
                if *ms == 0 || *ms > AGENT_ACTIONS_MAX_PAUSE_MS {
                    return Err(format!(
                        "steps[{index}].ms must be between 1 and {AGENT_ACTIONS_MAX_PAUSE_MS}"
                    ));
                }
                declared_wait_ms = declared_wait_ms.saturating_add(*ms);
            }
        }
    }
    if declared_wait_ms > AGENT_ACTIONS_MAX_DECLARED_WAIT_MS {
        return Err(format!(
            "declared waits exceed the batch maximum of {AGENT_ACTIONS_MAX_DECLARED_WAIT_MS}ms"
        ));
    }
    Ok(())
}

fn agent_locator_matches(row: &crate::wda::ElementRow, locator: &AgentElementLocator) -> bool {
    locator
        .label
        .as_ref()
        .is_none_or(|value| &row.label == value)
        && locator
            .identifier
            .as_ref()
            .is_none_or(|value| row.identifier.as_ref() == Some(value))
        && locator.kind.as_ref().is_none_or(|value| &row.kind == value)
        && locator
            .value
            .as_ref()
            .is_none_or(|value| row.value.as_ref() == Some(value))
        && locator
            .focused
            .is_none_or(|value| row.focused.unwrap_or(false) == value)
        && locator
            .enabled
            .is_none_or(|value| row.enabled.unwrap_or(true) == value)
        && locator
            .visible
            .is_none_or(|value| row.visible.unwrap_or(true) == value)
}

fn agent_expectation_observation(
    rows: &[crate::wda::ElementRow],
    expect: &AgentUiExpectation,
) -> (bool, serde_json::Value) {
    let application = rows
        .iter()
        .find(|row| row.kind == "Application")
        .map(|row| row.label.clone());
    let application_matches = expect
        .application
        .as_ref()
        .is_none_or(|expected| application.as_ref() == Some(expected));
    let missing_present: Vec<usize> = expect
        .present
        .iter()
        .enumerate()
        .filter_map(|(index, locator)| {
            (!rows.iter().any(|row| agent_locator_matches(row, locator))).then_some(index)
        })
        .collect();
    let violated_absent: Vec<usize> = expect
        .absent
        .iter()
        .enumerate()
        .filter_map(|(index, locator)| {
            rows.iter()
                .any(|row| agent_locator_matches(row, locator))
                .then_some(index)
        })
        .collect();
    let matches = application_matches && missing_present.is_empty() && violated_absent.is_empty();
    (
        matches,
        serde_json::json!({
            "application": application,
            "application_matches": application_matches,
            "missing_present": missing_present,
            "violated_absent": violated_absent
        }),
    )
}

#[derive(Debug)]
enum AgentWaitReadError {
    Failed(anyhow::Error),
    TimedOut,
}

async fn agent_wait_elements(
    w: &mut crate::wda::WdaClient,
    deadline: tokio::time::Instant,
) -> Result<Vec<crate::wda::ElementRow>, AgentWaitReadError> {
    match tokio::time::timeout_at(deadline, w.elements()).await {
        Ok(Ok(rows)) => Ok(rows),
        Ok(Err(first_error)) => {
            // Source reads are idempotent. A WebView transition can invalidate
            // one WDA session, so rebuild once within the same wait deadline.
            w.invalidate_session();
            match tokio::time::timeout_at(deadline, w.elements()).await {
                Ok(Ok(rows)) => Ok(rows),
                Ok(Err(second_error)) => Err(AgentWaitReadError::Failed(anyhow::anyhow!(
                    "source retry failed: {second_error:#}; first attempt: {first_error:#}"
                ))),
                Err(_) => Err(AgentWaitReadError::TimedOut),
            }
        }
        Err(_) => Err(AgentWaitReadError::TimedOut),
    }
}

// Keeping all failure evidence in one builder prevents individual early-return
// branches from silently omitting the at-most-once fields callers rely on.
#[allow(clippy::too_many_arguments)]
fn agent_actions_failure(
    status: StatusCode,
    failed_step: usize,
    completed: usize,
    applied_actions: usize,
    error: &str,
    outcome: &str,
    retry_safe: bool,
    steps: &[serde_json::Value],
    observation: Option<serde_json::Value>,
) -> Response {
    let mut body = serde_json::json!({
        "ok": false,
        "error": error,
        "failed_step": failed_step,
        "completed": completed,
        "applied_actions": applied_actions,
        "outcome": outcome,
        "retry_safe": retry_safe,
        "steps": steps
    });
    if let (Some(object), Some(observation)) = (body.as_object_mut(), observation) {
        object.insert("observation".to_string(), observation);
    }
    agent_actions_json(status, body)
}

/// `POST /agent/actions` — execute a bounded, fail-closed Direct/WDA sequence.
///
/// The whole request is validated before any action is sent. It supports three
/// step kinds: one existing input `action`, a short `pause`, and a semantic
/// `wait_for` over the current application and element locators. The WDA lock is
/// held for the sequence so another daemon client cannot interleave gestures.
/// Any failed action, expectation, read, lifecycle transition, or deadline stops
/// the sequence immediately; later actions are never attempted.
async fn agent_actions(
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
    if !has_phone_control_header(&headers) {
        return missing_phone_control_header_response();
    }
    if body.len() > AGENT_ACTIONS_MAX_BODY_BYTES {
        return agent_actions_invalid(format!(
            "request body exceeds {AGENT_ACTIONS_MAX_BODY_BYTES} bytes"
        ));
    }
    let request: AgentActionsRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => return agent_actions_invalid(format!("invalid JSON shape: {error}")),
    };
    if let Err(error) = validate_agent_actions(&request) {
        return agent_actions_invalid(error);
    }
    if state.backend != crate::config::DeviceBackend::Direct {
        return agent_actions_json(
            StatusCode::CONFLICT,
            serde_json::json!({
                "ok": false,
                "error": "batch_requires_direct_wda",
                "outcome": "not_sent",
                "retry_safe": true
            }),
        );
    }
    if state.managed_wda_pending {
        return target_not_configured_response();
    }
    if state.wda_lifecycle.is_transitioning()
        || state.released.load(std::sync::atomic::Ordering::Acquire)
    {
        return agent_actions_json(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({
                "ok": false,
                "error": "device_not_drivable",
                "outcome": "not_sent",
                "retry_safe": true,
                "hint": "check /agent/status, reconnect the canonical Direct target if instructed, then retry only after drivable=true"
            }),
        );
    }
    let Some(wda) = &state.wda else {
        return agent_actions_json(
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({
                "ok": false,
                "error": "wda_not_configured",
                "outcome": "not_sent",
                "retry_safe": true
            }),
        );
    };

    state.touch_activity();
    let _priority = state.begin_wda_control();
    let batch_deadline = tokio::time::Instant::now() + AGENT_ACTIONS_DEADLINE;
    let mut w = match tokio::time::timeout_at(batch_deadline, wda.lock()).await {
        Ok(client) => client,
        Err(_) => {
            return agent_actions_failure(
                StatusCode::REQUEST_TIMEOUT,
                0,
                0,
                0,
                "batch_deadline",
                "not_sent",
                true,
                &[],
                None,
            )
        }
    };

    let mut completed = 0_usize;
    let mut applied_actions = 0_usize;
    let mut step_results = Vec::with_capacity(request.steps.len());
    for (index, step) in request.steps.iter().enumerate() {
        if state.wda_lifecycle.is_transitioning()
            || state.released.load(std::sync::atomic::Ordering::Acquire)
        {
            return agent_actions_failure(
                StatusCode::SERVICE_UNAVAILABLE,
                index,
                completed,
                applied_actions,
                "device_transition_in_progress",
                "not_sent",
                applied_actions == 0,
                &step_results,
                None,
            );
        }
        if tokio::time::Instant::now() >= batch_deadline {
            return agent_actions_failure(
                StatusCode::GATEWAY_TIMEOUT,
                index,
                completed,
                applied_actions,
                "batch_deadline",
                "not_sent",
                applied_actions == 0,
                &step_results,
                None,
            );
        }

        match step {
            AgentActionStep::Action { action, after_ms } => {
                // Dispatch exactly once. If the batch deadline wins after this
                // point, the action outcome is unknown and the whole batch must
                // not be replayed automatically.
                let outcome = match tokio::time::timeout_at(
                    batch_deadline,
                    direct_agent_action(&mut w, &state.wda_actionable, action),
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        mark_wda_read_path_unactionable(&state);
                        return agent_actions_failure(
                            StatusCode::GATEWAY_TIMEOUT,
                            index,
                            completed,
                            applied_actions,
                            "outcome_unknown",
                            "unknown",
                            false,
                            &step_results,
                            None,
                        );
                    }
                };
                if outcome != WdaControlOutcome::Applied {
                    let (status, error, outcome_name, current_retry_safe) = match outcome {
                        WdaControlOutcome::NotSent => (
                            StatusCode::BAD_GATEWAY,
                            "wda_pre_dispatch_failed",
                            "not_sent",
                            true,
                        ),
                        WdaControlOutcome::Unsupported => (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "unsupported_control",
                            "not_sent",
                            true,
                        ),
                        WdaControlOutcome::InvalidElementSnapshot => (
                            StatusCode::BAD_REQUEST,
                            "invalid_element_snapshot",
                            "not_sent",
                            true,
                        ),
                        WdaControlOutcome::StaleElementSnapshot => (
                            StatusCode::CONFLICT,
                            "stale_element_snapshot",
                            "not_sent",
                            true,
                        ),
                        WdaControlOutcome::ElementNotFound => (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "element_not_found",
                            "not_sent",
                            true,
                        ),
                        WdaControlOutcome::AmbiguousElement => (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "ambiguous_element_label",
                            "not_sent",
                            true,
                        ),
                        WdaControlOutcome::InvalidElementTarget => (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "invalid_element_target",
                            "not_sent",
                            true,
                        ),
                        WdaControlOutcome::Failed => {
                            (StatusCode::BAD_GATEWAY, "outcome_unknown", "unknown", false)
                        }
                        WdaControlOutcome::Applied => unreachable!(),
                    };
                    if matches!(
                        outcome,
                        WdaControlOutcome::Failed | WdaControlOutcome::NotSent
                    ) {
                        mark_wda_read_path_unactionable(&state);
                    }
                    return agent_actions_failure(
                        status,
                        index,
                        completed,
                        applied_actions,
                        error,
                        outcome_name,
                        current_retry_safe && applied_actions == 0,
                        &step_results,
                        None,
                    );
                }
                applied_actions += 1;
                if *after_ms > 0
                    && tokio::time::timeout_at(
                        batch_deadline,
                        tokio::time::sleep(std::time::Duration::from_millis(*after_ms)),
                    )
                    .await
                    .is_err()
                {
                    return agent_actions_failure(
                        StatusCode::GATEWAY_TIMEOUT,
                        index,
                        completed,
                        applied_actions,
                        "batch_deadline_after_action",
                        "applied",
                        false,
                        &step_results,
                        None,
                    );
                }
                step_results.push(serde_json::json!({
                    "index": index,
                    "kind": "action",
                    "ok": true
                }));
            }
            AgentActionStep::Pause { ms } => {
                if tokio::time::timeout_at(
                    batch_deadline,
                    tokio::time::sleep(std::time::Duration::from_millis(*ms)),
                )
                .await
                .is_err()
                {
                    return agent_actions_failure(
                        StatusCode::GATEWAY_TIMEOUT,
                        index,
                        completed,
                        applied_actions,
                        "batch_deadline",
                        "not_sent",
                        applied_actions == 0,
                        &step_results,
                        None,
                    );
                }
                step_results.push(serde_json::json!({
                    "index": index,
                    "kind": "pause",
                    "ok": true
                }));
            }
            AgentActionStep::WaitFor {
                expect,
                timeout_ms,
                poll_ms,
            } => {
                let wait_deadline = std::cmp::min(
                    batch_deadline,
                    tokio::time::Instant::now() + std::time::Duration::from_millis(*timeout_ms),
                );
                let mut attempts = 0_u64;
                let mut last_observation = serde_json::Value::Null;
                let mut last_read_error = None;
                loop {
                    attempts += 1;
                    let rows = match agent_wait_elements(&mut w, wait_deadline).await {
                        Ok(rows) => rows,
                        Err(AgentWaitReadError::Failed(error)) => {
                            last_read_error = Some(format!("{error:#}"));
                            // System sheets can briefly restart the WDA relay or
                            // invalidate the app-scoped session after an applied
                            // action. A `wait_for` owns a bounded polling window,
                            // so keep rebuilding the read-only session inside that
                            // window instead of failing on the first two quick
                            // connection refusals. No mutation is replayed.
                            if tokio::time::Instant::now() >= wait_deadline {
                                tracing::warn!(
                                    "wda batch wait_for source never recovered: {error:#}"
                                );
                                mark_wda_read_path_unactionable(&state);
                                return agent_actions_failure(
                                    StatusCode::BAD_GATEWAY,
                                    index,
                                    completed,
                                    applied_actions,
                                    "wda_source_failed",
                                    "not_sent",
                                    applied_actions == 0,
                                    &step_results,
                                    None,
                                );
                            }
                            let remaining = wait_deadline
                                .saturating_duration_since(tokio::time::Instant::now());
                            tokio::time::sleep(std::cmp::min(
                                std::time::Duration::from_millis(*poll_ms),
                                remaining,
                            ))
                            .await;
                            continue;
                        }
                        Err(AgentWaitReadError::TimedOut) => {
                            if let Some(error) = last_read_error {
                                tracing::warn!(
                                    "wda batch wait_for source timed out after retries: {error}"
                                );
                            }
                            w.invalidate_session();
                            return agent_actions_failure(
                                StatusCode::CONFLICT,
                                index,
                                completed,
                                applied_actions,
                                "expectation_timeout",
                                "not_sent",
                                applied_actions == 0,
                                &step_results,
                                Some(last_observation),
                            );
                        }
                    };
                    let (matches, observation) = agent_expectation_observation(&rows, expect);
                    last_observation = observation;
                    if matches {
                        step_results.push(serde_json::json!({
                            "index": index,
                            "kind": "wait_for",
                            "ok": true,
                            "attempts": attempts,
                            "observation": last_observation
                        }));
                        break;
                    }
                    if tokio::time::Instant::now() >= wait_deadline {
                        return agent_actions_failure(
                            StatusCode::CONFLICT,
                            index,
                            completed,
                            applied_actions,
                            "expectation_timeout",
                            "not_sent",
                            applied_actions == 0,
                            &step_results,
                            Some(last_observation),
                        );
                    }
                    let remaining =
                        wait_deadline.saturating_duration_since(tokio::time::Instant::now());
                    tokio::time::sleep(std::cmp::min(
                        std::time::Duration::from_millis(*poll_ms),
                        remaining,
                    ))
                    .await;
                }
            }
        }
        completed += 1;
    }

    state.touch_activity();
    let locked = recover(state.wda_health.lock()).locked;
    *recover(state.wda_health.lock()) = crate::wda::WdaHealth {
        up: true,
        actionable: true,
        locked,
    };
    agent_actions_json(
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "completed": completed,
            "applied_actions": applied_actions,
            "steps": step_results
        }),
    )
}

#[derive(Debug, Default, Deserialize)]
struct AgentInputQuery {
    /// `delta`: after a successfully applied Direct action, wait for the UI to
    /// settle and include the resulting element-tree change in the SAME
    /// response — collapsing the act-then-read round trip pair into one.
    #[serde(rename = "return", default)]
    return_mode: Option<String>,
    /// Explicit baseline snapshot for the returned delta. Defaults to the
    /// action's own `snapshot` field (present on snapshot-bound actions).
    #[serde(default)]
    since: Option<String>,
    /// Settle budget in milliseconds (default 1200, capped): how long to wait
    /// for two consecutive identical tree reads before answering.
    #[serde(default)]
    settle_ms: Option<u64>,
}

const AGENT_INPUT_SETTLE_DEFAULT_MS: u64 = 1_200;
const AGENT_INPUT_SETTLE_MAX_MS: u64 = 5_000;

/// One post-action tree read with a single stale-session retry (mirroring
/// `/agent/elements`' read loop, but bounded — this runs inside an action's
/// deadline).
async fn read_elements_once(
    w: &mut crate::wda::WdaClient,
) -> anyhow::Result<(String, Vec<crate::wda::ElementRow>)> {
    let rows = match w.elements().await {
        Ok(rows) => rows,
        Err(error) => {
            w.invalidate_session();
            w.elements()
                .await
                .map_err(|retry| retry.context(format!("first error: {error:#}")))?
        }
    };
    let id = element_snapshot_id(&rows)?;
    Ok((id, rows))
}

/// Wait (bounded) for the post-action UI to quiesce, then return the settled
/// tree: poll until two consecutive reads hash identically or the budget runs
/// out, and return the latest read either way.
async fn settle_and_read_elements(
    w: &mut crate::wda::WdaClient,
    budget: std::time::Duration,
) -> anyhow::Result<(String, Vec<crate::wda::ElementRow>)> {
    let deadline = tokio::time::Instant::now() + budget;
    tokio::time::sleep(std::cmp::min(std::time::Duration::from_millis(150), budget)).await;
    let (mut id, mut rows) = read_elements_once(w).await?;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok((id, rows));
        }
        tokio::time::sleep(std::cmp::min(
            std::time::Duration::from_millis(250),
            remaining,
        ))
        .await;
        let (next_id, next_rows) = read_elements_once(w).await?;
        let stable = next_id == id;
        id = next_id;
        rows = next_rows;
        if stable {
            return Ok((id, rows));
        }
    }
}

/// `POST /agent/input` — inject one control message (same JSON shape as the
/// WebRTC control channel): `{"type":"tap","x":0.5,"y":0.5}`,
/// `{"type":"text","text":"hi"}`, `{"type":"scroll","x":..,"y":..,"dx":..,"dy":..}`,
/// `{"type":"shortcut","name":"home"}`, `{"type":"key","name":"return"}`,
/// `{"type":"uninstall","bundle":"com.example.app"}` (via devicectl), etc.
///
/// `?return=delta` (optional, Direct only): after an applied action the
/// response also carries the settled post-action element tree — as a `delta`
/// against `?since=` / the action's own `snapshot` when that baseline is still
/// cached, else as full `elements` — plus the fresh `snapshot` token. A failed
/// observation never fails the applied action; it is reported as `delta_error`.
///
/// Coordinates are normalized `[0,1]` over the phone content rect (geometry-agnostic,
/// like the web client). Acquiring an `Agent` control lease makes the injector gate
/// allow the event; this preempts a human viewer (single shared cursor, last actor
/// wins). Returns 200 on accept, 400 on an unparseable message.
async fn agent_input(
    State(state): State<Arc<AppState>>,
    Query(query): Query<AgentInputQuery>,
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
    if !has_phone_control_header(&headers) {
        return missing_phone_control_header_response();
    }
    // The MCP client waits 30 seconds. Keep the daemon's complete Direct WDA
    // budget comfortably below that so the authoritative HTTP outcome arrives
    // before the client can abandon a still-running action.
    let agent_wda_deadline = tokio::time::Instant::now() + AGENT_INPUT_WDA_DEADLINE;
    // App uninstall via CoreDevice (`devicectl`) — WDA can't remove apps and
    // UI-driven deletion is unreliable to automate, so this is the dependable
    // "Delete App (with data)" primitive (e.g. resetting a wedged app to its
    // login state). `{"type":"uninstall","bundle":"com.example.app"}`; optional
    // `"udid"` targets a specific paired phone; otherwise reuse the daemon's
    // persisted target before falling back to CoreDevice auto-detection.
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
            let udid = match v.get("udid") {
                None => state.device_udid.clone(),
                Some(serde_json::Value::String(udid))
                    if !udid.is_empty()
                        && udid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') =>
                {
                    if state
                        .device_udid
                        .as_deref()
                        .is_some_and(|configured| configured != udid)
                    {
                        return with_security_headers(
                            Response::builder()
                                .status(StatusCode::CONFLICT)
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Body::from(
                                    r#"{"ok":false,"error":"target_change_requires_restart"}"#,
                                ))
                                .unwrap_or_else(|_| {
                                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                                }),
                        );
                    }
                    Some(udid.clone())
                }
                Some(_) => {
                    return with_security_headers(
                        (
                            StatusCode::BAD_REQUEST,
                            "uninstall \"udid\" must be non-empty hex and dashes",
                        )
                            .into_response(),
                    );
                }
            };
            let bundle = bundle.to_string();
            let r =
                tokio::task::spawn_blocking(move || devicectl_uninstall(udid.as_deref(), &bundle))
                    .await
                    .unwrap_or_else(|e| Err(DevicectlError::Failed(format!("join error: {e}"))));
            return match r {
                Ok(()) => {
                    with_security_headers((StatusCode::OK, "ok (uninstalled)").into_response())
                }
                Err(DevicectlError::Timeout) => {
                    tracing::warn!("devicectl uninstall exceeded server deadline and was killed");
                    with_security_headers(
                        (
                            StatusCode::GATEWAY_TIMEOUT,
                            "uninstall timed out; devicectl was terminated",
                        )
                            .into_response(),
                    )
                }
                Err(DevicectlError::TargetRequired(count)) => {
                    tracing::warn!(
                        "devicectl uninstall requires an explicit target ({count} connected candidates)"
                    );
                    with_security_headers(
                        Response::builder()
                            .status(StatusCode::CONFLICT)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(format!(
                                r#"{{"ok":false,"error":"target_required","connected_candidates":{count},"hint":"configure PHONE_REMOTE_UDID or pass an explicit matching udid"}}"#
                            )))
                            .unwrap_or_else(|_| {
                                StatusCode::INTERNAL_SERVER_ERROR.into_response()
                            }),
                    )
                }
                Err(DevicectlError::Failed(e)) => {
                    tracing::warn!("devicectl uninstall failed: {e}");
                    with_security_headers(
                        (StatusCode::BAD_GATEWAY, format!("uninstall failed: {e}")).into_response(),
                    )
                }
            };
        }
    }
    if state.wda_lifecycle.is_releasing() {
        return with_security_headers(
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::RETRY_AFTER, "5")
                .body(Body::from(
                    r#"{"ok":false,"error":"device_release_in_progress"}"#,
                ))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    // If the idle watchdog released the phone, one caller starts recovery while
    // `released` remains true. Only a successful supervisor bootstrap clears it;
    // failed recovery therefore remains honest and retryable instead of briefly
    // reporting an active device that never restarted.
    if state.released.load(std::sync::atomic::Ordering::Acquire) {
        if !state.managed_wda {
            return with_security_headers(
                Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"ok":false,"error":"wda_is_externally_managed","recovery_owner":"external","reconnecting":false,"hint":"restart WDA on the configured endpoint's owning host"}"#,
                    ))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            );
        }
        let won = state.wda_lifecycle.try_begin_reconnecting();
        if won {
            let recovery_state = state.clone();
            let home = std::env::var("HOME").unwrap_or_default();
            let setup_sh = format!("{home}/.iphone-use/setup-wda.sh");
            let log = format!("{home}/.iphone-use/wda-agent.log");
            let udid = state.device_udid.clone().unwrap_or_default();
            tokio::spawn(async move {
                let bootstrapped = tokio::task::spawn_blocking(move || {
                    write_and_bootstrap_wda_agent(&home, &setup_sh, &log, &udid)
                })
                .await
                .unwrap_or(false);
                if bootstrapped {
                    *recover(recovery_state.wda_health.lock()) = crate::wda::WdaHealth::down();
                    recovery_state
                        .wda_actionable
                        .store(false, std::sync::atomic::Ordering::Release);
                    spawn_wda_readiness_wait(recovery_state);
                } else {
                    recovery_state.wda_lifecycle.finish_reconnecting();
                }
            });
        }
        if state.wda_lifecycle.is_releasing() {
            return with_security_headers(
                Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::RETRY_AFTER, "5")
                    .body(Body::from(
                        r#"{"ok":false,"error":"device_release_in_progress","reconnecting":false}"#,
                    ))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            );
        }
        return with_security_headers(
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::RETRY_AFTER, "5")
                .body(Body::from(
                    r#"{"ok":false,"reconnecting":true,"hint":"phone was idle-released to free it for hands-on use; managed WDA is restarting (~30-90s) — retry. If the phone is locked, unlock it once."}"#,
                ))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    if state.backend == crate::config::DeviceBackend::Direct && state.managed_wda_pending {
        return target_not_configured_response();
    }
    if state.wda_lifecycle.is_reconnecting() {
        return with_security_headers(
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::RETRY_AFTER, "5")
                .body(Body::from(
                    r#"{"ok":false,"error":"reconnect_in_progress","reconnecting":true}"#,
                ))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    // Every real driving request resets the idle clock so the watchdog only
    // fires during genuine inactivity.
    state.touch_activity();
    if state.wda_lifecycle.is_releasing() {
        return with_security_headers(
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::RETRY_AFTER, "5")
                .body(Body::from("device release in progress"))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        );
    }
    // Direct is a single at-most-once WDA path. One server deadline covers lock
    // acquisition plus the whole compound action, and no failure is replayed.
    if state.backend == crate::config::DeviceBackend::Direct {
        let value = match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(value) if value.is_object() => value,
            _ => {
                return with_security_headers(
                    (
                        StatusCode::BAD_REQUEST,
                        r#"{"ok":false,"error":"invalid_control_message"}"#,
                    )
                        .into_response(),
                );
            }
        };
        let Some(wda) = &state.wda else {
            return with_security_headers(
                Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"ok":false,"error":"wda_not_configured","fallback":"disabled"}"#,
                    ))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            );
        };
        if tokio::time::Instant::now() >= agent_wda_deadline {
            return wda_deadline_response(false);
        }
        let _priority = state.begin_wda_control();
        let dispatched = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let dispatch_marker = dispatched.clone();
        let want_delta = query.return_mode.as_deref() == Some("delta");
        let outcome = tokio::time::timeout_at(agent_wda_deadline, async {
            let mut client = wda.lock().await;
            if tokio::time::Instant::now() >= agent_wda_deadline
                || state.wda_lifecycle.is_transitioning()
                || state.released.load(std::sync::atomic::Ordering::Acquire)
            {
                return None;
            }
            dispatch_marker.store(true, std::sync::atomic::Ordering::Release);
            let outcome = direct_agent_action(&mut client, &state.wda_actionable, &value).await;
            // Post-action observation (`?return=delta`), in the SAME lock scope
            // so no other control interleaves between the action and its read.
            // The budget stays under the endpoint deadline with a safety margin
            // so a slow observation can never turn an applied action into an
            // "outcome unknown" timeout.
            let mut settled = None;
            if want_delta && outcome == WdaControlOutcome::Applied {
                let requested = query
                    .settle_ms
                    .unwrap_or(AGENT_INPUT_SETTLE_DEFAULT_MS)
                    .min(AGENT_INPUT_SETTLE_MAX_MS);
                let remaining = agent_wda_deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .saturating_sub(std::time::Duration::from_secs(3));
                let budget = std::cmp::min(std::time::Duration::from_millis(requested), remaining);
                settled = Some(
                    settle_and_read_elements(&mut client, budget)
                        .await
                        .map(|(snapshot, rows)| (snapshot, Arc::new(rows)))
                        .map_err(|error| format!("{error:#}")),
                );
            }
            Some((outcome, settled))
        })
        .await;
        let (outcome, settled) = match outcome {
            Ok(Some(pair)) => pair,
            Ok(None) => return wda_deadline_response(false),
            Err(_) => {
                return wda_deadline_response(
                    dispatched.load(std::sync::atomic::Ordering::Acquire),
                );
            }
        };
        return match outcome {
            WdaControlOutcome::Applied => {
                let body = match settled {
                    None => r#"{"ok":true,"transport":"wda"}"#.to_string(),
                    // The action DID apply; a failed observation is reported
                    // alongside the success, never as a failure.
                    Some(Err(error)) => serde_json::json!({
                        "ok": true,
                        "transport": "wda",
                        "delta_error": error,
                    })
                    .to_string(),
                    Some(Ok((snapshot, rows))) => {
                        remember_element_snapshot(&state, &snapshot, &rows);
                        let baseline = query
                            .since
                            .as_deref()
                            .filter(|since| !since.is_empty())
                            .or_else(|| value.get("snapshot").and_then(serde_json::Value::as_str))
                            .and_then(|since| {
                                lookup_element_snapshot(&state, since)
                                    .map(|baseline| (since, baseline))
                            });
                        match baseline {
                            Some((baseline_id, baseline_rows)) => {
                                let delta = diff_element_rows(&baseline_rows, &rows);
                                serde_json::json!({
                                    "ok": true,
                                    "transport": "wda",
                                    "snapshot": snapshot,
                                    "baseline": baseline_id,
                                    "delta": elements_delta_json(&delta, &rows),
                                })
                                .to_string()
                            }
                            None => serde_json::json!({
                                "ok": true,
                                "transport": "wda",
                                "snapshot": snapshot,
                                "elements": &*rows,
                            })
                            .to_string(),
                        }
                    }
                };
                with_security_headers(
                    Response::builder()
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                )
            }
            WdaControlOutcome::NotSent => {
                mark_wda_read_path_unactionable(&state);
                wda_failed_before_dispatch_response()
            }
            WdaControlOutcome::Unsupported => with_security_headers(
                Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"ok":false,"error":"wda_unavailable_or_unsupported","fallback":"disabled"}"#,
                    ))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            ),
            WdaControlOutcome::InvalidElementSnapshot => invalid_element_snapshot_response(),
            WdaControlOutcome::StaleElementSnapshot => stale_element_snapshot_response(),
            WdaControlOutcome::ElementNotFound => element_not_found_response(),
            WdaControlOutcome::AmbiguousElement => ambiguous_element_response(),
            WdaControlOutcome::InvalidElementTarget => invalid_element_target_response(),
            WdaControlOutcome::Failed => wda_failed_after_dispatch_response(),
        };
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
                "yielded to human: iPhone Mirroring is not frontmost — retry when status human_active is false; on-device control requires PHONE_REMOTE_BACKEND=direct plus a daemon restart",
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
    recover(state.lease_state.lock()).acquire(core::control::Holder::Agent(agent_id), now_secs());
    // Deliverability check (issue #25): an L3 event only lands if iPhone
    // Mirroring can be brought frontmost. When a human is on the Mac, macOS
    // refuses to let a background LaunchAgent steal focus, so the event is
    // silently dropped — and returning "ok" makes an agent loop blindly. Bring
    // it frontmost up front; if that fails, report the drop instead of lying.
    #[cfg(target_os = "macos")]
    {
        // Same deadline the injector loop uses (#29). This used to be a
        // hardcoded 1200ms — under the >2s an osascript activation needs on
        // first use — so a fresh activation on a completely idle Mac was
        // reported back to the agent as `dropped: human is using the Mac`.
        let delivered = tokio::task::spawn_blocking(|| {
            crate::macos::ensure_mirroring_frontmost(crate::macos::front_deadline())
        })
        .await
        .unwrap_or(false);
        if !delivered {
            return with_security_headers(
                Response::builder()
                    .status(StatusCode::CONFLICT)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"ok":false,"dropped":true,"reason":"iPhone Mirroring could not be brought frontmost (a human is using the Mac, or it is paused/in-use) — poll /agent/status until human_active is false and drivable is true; on-device control requires PHONE_REMOTE_BACKEND=direct plus a daemon restart"}"#,
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
/// text — an order of magnitude cheaper. Prefer snapshot-bound element indexes;
/// exact label taps are accepted only when one current row matches. 503 when
/// WDA is not configured; 502 when it's configured but unreachable.
///
/// `?since=<snapshot>` (optional): when the daemon still holds that snapshot's
/// tree, the response replaces `elements` with a `delta`
/// (`{added,changed,removed,unchanged}` — see [`diff_element_rows`]) against it
/// plus the fresh `snapshot` token. iOS trees are large and multi-step flows
/// change little of them per step, so this is the main token/latency saver.
/// An unknown or evicted `since` falls back to the full tree, so old callers
/// and cold caches behave exactly as before.
async fn agent_elements(
    State(state): State<Arc<AppState>>,
    Query(query): Query<AgentElementsQuery>,
    headers: HeaderMap,
) -> Response {
    // The browser's accessible-controls drawer reads the same on-device tree
    // that agents use. It is read-only, so accept the authenticated browser
    // session just like `/agent/status` and `/agent/screenshot`; machine callers
    // continue to use the dedicated bearer token.
    match browser_or_agent_auth(&state, &headers) {
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
    // Always answer with parseable JSON, while preserving failure in the HTTP
    // status. A 200 empty tree is indistinguishable from a genuinely empty
    // screen and caused MCP clients to continue from false state.
    let json_body = |status: StatusCode, body: String| {
        with_security_headers(
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        )
    };
    if state.backend != crate::config::DeviceBackend::Direct {
        return json_body(
            StatusCode::CONFLICT,
            r#"{"elements":[],"error":"backend_is_mirror"}"#.to_string(),
        );
    }
    if state.managed_wda_pending {
        return json_body(
            StatusCode::CONFLICT,
            r#"{"elements":[],"error":"target_not_configured","hint":"run setup-wda.sh to select and persist the canonical iPhone before using Direct control"}"#.to_string(),
        );
    }
    // Inspecting the element tree is part of driving — keep the phone held.
    state.touch_activity();
    if state.wda_lifecycle.is_transitioning()
        || state.released.load(std::sync::atomic::Ordering::Acquire)
    {
        return json_body(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"elements":[],"error":"device_transition_in_progress","transitioning":true}"#
                .to_string(),
        );
    }
    let Some(wda) = &state.wda else {
        return json_body(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"elements":[],"error":"wda_not_configured"}"#.to_string(),
        );
    };
    let _priority = state.begin_wda_control();
    // MCP waits 45 seconds for this endpoint. Bound mutex wait + optional
    // read-only stale-session retry + screen-size lookup to 35 seconds so the
    // daemon, not a disconnected client, owns the final outcome.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(35);
    let result = tokio::time::timeout_at(deadline, async {
        let mut w = wda.lock().await;
        let mut first_source_error = None;
        let rows = loop {
            match w.elements().await {
                Ok(rows) => break rows,
                Err(error) => {
                    let error = format!("{error:#}");
                    if first_source_error.is_none() {
                        first_source_error = Some(error.clone());
                    }
                    // Source reads are idempotent. System document pickers can
                    // briefly restart the WDA relay/session, so two immediate
                    // attempts are not a meaningful recovery window. Keep
                    // rebuilding with a bounded delay until the endpoint's
                    // existing total deadline; no mutation is replayed.
                    w.invalidate_session();
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        anyhow::bail!(
                            "WDA source never recovered; last error: {error}; first error: {}",
                            first_source_error.as_deref().unwrap_or("unknown")
                        );
                    }
                    tokio::time::sleep(std::cmp::min(
                        std::time::Duration::from_millis(250),
                        remaining,
                    ))
                    .await;
                }
            }
        };
        // Screen size lets callers normalize point-space rects. Failure is
        // non-fatal; the element tree itself is still useful.
        let screen = w.window_size().await.ok();
        Ok::<_, anyhow::Error>((rows, screen))
    })
    .await;
    let (rows, screen) = match result {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!("wda elements failed: {error:#}");
            mark_wda_read_path_unactionable(&state);
            return json_body(
                StatusCode::BAD_GATEWAY,
                r#"{"elements":[],"error":"wda_source_failed","transitioning":true}"#.to_string(),
            );
        }
        Err(_) => {
            mark_wda_read_path_unactionable(&state);
            return json_body(
                StatusCode::GATEWAY_TIMEOUT,
                r#"{"elements":[],"error":"wda_source_timeout","transitioning":true}"#.to_string(),
            );
        }
    };
    let snapshot = match element_snapshot_id(&rows) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!("serialize WDA element snapshot: {error:#}");
            return json_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"elements":[],"error":"serialization_failed"}"#.to_string(),
            );
        }
    };
    let rows = Arc::new(rows);
    remember_element_snapshot(&state, &snapshot, &rows);
    let screen =
        screen.map(|(width, height)| serde_json::json!({"width": width, "height": height}));
    // `?since=` with a still-cached baseline answers with a delta instead of
    // the full tree; anything else (no param, evicted, unknown) stays the
    // exact pre-diff response shape.
    let body = match query
        .since
        .as_deref()
        .filter(|since| !since.is_empty())
        .and_then(|since| lookup_element_snapshot(&state, since).map(|rows| (since, rows)))
    {
        Some((since, baseline)) => {
            let delta = diff_element_rows(&baseline, &rows);
            serde_json::json!({
                "screen": screen,
                "snapshot": snapshot,
                "baseline": since,
                "delta": elements_delta_json(&delta, &rows),
            })
        }
        None => serde_json::json!({
            "screen": screen,
            "snapshot": snapshot,
            "elements": &*rows,
        }),
    };
    match serde_json::to_string(&body) {
        Ok(body) => json_body(StatusCode::OK, body),
        Err(_) => json_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"elements":[],"error":"serialization_failed"}"#.to_string(),
        ),
    }
}

#[derive(Debug, Default, Deserialize)]
struct AgentElementsQuery {
    /// Prior `snapshot` token to diff against (see [`agent_elements`]).
    #[serde(default)]
    since: Option<String>,
}

/// A read-path failure is enough to revoke `drivable`, even when the last
/// background health probe still says the WDA runner was up.
///
/// Keep reachability and lock knowledge intact: a WebView transition can make
/// `/source` fail while WDA itself remains reachable. The next bounded status
/// probe decides whether the runner is down; until then, actions fail closed.
fn mark_wda_read_path_unactionable(state: &AppState) {
    state
        .wda_actionable
        .store(false, std::sync::atomic::Ordering::Release);
    recover(state.wda_health.lock()).actionable = false;
}

/// `GET /agent/screenshot` — current phone screen as a PNG.
///
/// Direct captures on-device through WDA and fails closed if WDA is unavailable.
/// Mirror compatibility captures its configured Mirroring window through
/// [`core::capture::screenshot_mirroring_png`]. The two paths never fall
/// through to one another.
///
/// Auth: agent bearer **or** a valid session cookie. The cookie path exists for
/// the web client's stills-fallback (when Mirroring dies the page polls this
/// endpoint) — a logged-in viewer already sees these pixels as video, so the
/// privilege is identical. The cookie is checked FIRST so browser polling never
/// touches the bearer auth-limiter (5 misses there lock the agent API for 30s).
async fn agent_screenshot(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    // Match `/phone`: password=None intentionally makes the browser UI open.
    // A separate agent token still protects machine-only mutation endpoints.
    match browser_or_agent_auth(&state, &headers) {
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
    if state.backend == crate::config::DeviceBackend::Direct && state.managed_wda_pending {
        return target_not_configured_response();
    }
    if state.backend == crate::config::DeviceBackend::Direct
        && (state.wda_lifecycle.is_transitioning()
            || state.released.load(std::sync::atomic::Ordering::Relaxed))
    {
        return with_security_headers(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "direct device is released, releasing, or reconnecting",
            )
                .into_response(),
        );
    }
    // A screenshot means someone is looking at the phone — keep it held.
    state.touch_activity();
    // The configured backend owns capture end-to-end. Direct uses WDA bytes
    // from its canonical phone and returns an error when that path is down;
    // Mirror alone reaches the legacy host-window capture below. This prevents
    // a failed Direct request from silently returning pixels from another
    // mirrored phone.
    if state.backend == crate::config::DeviceBackend::Direct {
        let _priority = state.wda.as_ref().map(|_| state.begin_wda_control());
        let Some(wda) = &state.wda else {
            return with_security_headers(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "direct device screenshot unavailable (WDA is not configured)",
                )
                    .into_response(),
            );
        };
        return match tokio::time::timeout(std::time::Duration::from_secs(20), async {
            wda.lock().await.screenshot_png().await
        })
        .await
        {
            Ok(Ok(bytes)) if is_valid_png(&bytes) => {
                let response = Response::builder()
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
                with_security_headers(response)
            }
            Ok(Ok(bytes)) => {
                tracing::warn!(
                    "agent screenshot: Direct WDA returned {} bytes, not a valid PNG",
                    bytes.len()
                );
                mark_wda_read_path_unactionable(&state);
                with_security_headers(
                    (StatusCode::BAD_GATEWAY, "WDA returned an invalid PNG").into_response(),
                )
            }
            Ok(Err(error)) => {
                tracing::warn!("agent screenshot: Direct WDA failed: {error:#}");
                mark_wda_read_path_unactionable(&state);
                with_security_headers(
                    (StatusCode::BAD_GATEWAY, "WDA screenshot failed").into_response(),
                )
            }
            Err(_) => {
                mark_wda_read_path_unactionable(&state);
                with_security_headers(
                    (
                        StatusCode::GATEWAY_TIMEOUT,
                        "WDA screenshot exceeded the server deadline",
                    )
                        .into_response(),
                )
            }
        };
    }
    let png = tokio::task::spawn_blocking(core::capture::screenshot_mirroring_png).await;
    // Mirror is an isolated compatibility backend: a failed/runt host-window
    // capture returns 503 and never reaches WDA, even if an invalid AppState
    // accidentally contains a WDA client.
    match png {
        Ok(Ok(bytes)) if is_valid_png(&bytes) => {
            let resp = Response::builder()
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            return with_security_headers(resp);
        }
        Ok(Ok(bytes)) => {
            tracing::warn!(
                "agent screenshot: Mirror capture returned {} bytes, not a valid PNG",
                bytes.len()
            );
        }
        Ok(Err(e)) => {
            tracing::warn!("agent screenshot: no Mirroring window: {e:#}");
        }
        Err(e) => {
            tracing::warn!("agent screenshot: Mirror capture task panicked: {e:?}");
        }
    }
    with_security_headers(
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "no valid screenshot frame available",
        )
            .into_response(),
    )
}

/// `GET /agent/mjpeg` — LIVE video in agent mode by proxying WDA's on-device
/// MJPEG stream (`multipart/x-mixed-replace`). The MJPEG server runs inside the
/// same XCUITest session as control, so video and driving coexist — unlike
/// iPhone Mirroring, which is mutually exclusive with WDA. A browser renders
/// this directly in an `<img src="/agent/mjpeg">`. ~28 fps at the tuned
/// settings applied here (framerate/scaling/quality), regardless of USB vs Wi-Fi
/// (the cap is WDA's screenshot rate, not the transport).
fn is_mjpeg_content_type(value: &str) -> bool {
    value.split(';').next().is_some_and(|media_type| {
        media_type
            .trim()
            .eq_ignore_ascii_case("multipart/x-mixed-replace")
    })
}

async fn agent_mjpeg(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MjpegStreamQuery>,
    headers: HeaderMap,
) -> Response {
    // Same cookie-or-bearer rule as `agent_screenshot`.
    match browser_or_agent_auth(&state, &headers) {
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
    let stream_id = match query.stream_id {
        Some(stream_id) if valid_mjpeg_stream_id(&stream_id) => Some(stream_id),
        Some(_) => {
            return with_security_headers(
                (StatusCode::BAD_REQUEST, "invalid MJPEG stream id").into_response(),
            )
        }
        None => None,
    };
    if state.backend != crate::config::DeviceBackend::Direct {
        return with_security_headers(
            (
                StatusCode::CONFLICT,
                "WDA MJPEG is disabled for the Mirror backend",
            )
                .into_response(),
        );
    }
    if state.managed_wda_pending {
        return target_not_configured_response();
    }
    if state.wda_lifecycle.is_transitioning()
        || state.released.load(std::sync::atomic::Ordering::Relaxed)
    {
        return with_security_headers(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "direct device is released, releasing, or reconnecting",
            )
                .into_response(),
        );
    }
    // Opening the live feed counts as watching — stamp now, and hold a stream
    // guard (below) for the whole connection so the idle watchdog won't release
    // the phone while a viewer is on it.
    state.touch_activity();
    if state.wda_lifecycle.is_transitioning()
        || state.released.load(std::sync::atomic::Ordering::Acquire)
    {
        return with_security_headers(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "device transition in progress",
            )
                .into_response(),
        );
    }
    const MAX_MJPEG_VIEWERS: usize = 4;
    let Some(stream_guard) =
        StreamGuard::try_reserve(state.live_streams.clone(), MAX_MJPEG_VIEWERS)
    else {
        return with_security_headers(
            (
                StatusCode::TOO_MANY_REQUESTS,
                "too many live viewers (maximum 4)",
            )
                .into_response(),
        );
    };
    let Some(url) = state.mjpeg_url.clone() else {
        return with_security_headers(
            (StatusCode::SERVICE_UNAVAILABLE, "no WDA MJPEG configured").into_response(),
        );
    };
    // Best-effort: tune the stream for a smooth feed (idempotent). A failure
    // here just leaves WDA's defaults (~9 fps) — still usable. Never wait behind
    // a control/status holder: opening video must not outlive the browser's own
    // first-frame timeout merely to change optional settings.
    if let Some(wda) = &state.wda {
        let _priority = state.begin_wda_control();
        if let Ok(mut client) = wda.try_lock() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(750),
                client.set_mjpeg_settings(30, 50, 60),
            )
            .await;
        }
    }
    // Proxy the upstream MJPEG stream straight through. Keep the request itself
    // unbounded because the body is intentionally long-lived, but cap the TCP
    // connect phase so a dead relay cannot hang the handler indefinitely.
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let upstream = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get(&url).send(),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            return with_security_headers(
                (
                    StatusCode::GATEWAY_TIMEOUT,
                    "WDA MJPEG did not return response headers before the deadline",
                )
                    .into_response(),
            )
        }
    };
    match upstream {
        Ok(up) if up.status().is_success() => {
            let content_type = up
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .filter(|value| is_mjpeg_content_type(value));
            let Some(content_type) = content_type else {
                return with_security_headers(
                    (
                        StatusCode::BAD_GATEWAY,
                        "WDA MJPEG upstream returned a non-MJPEG content type",
                    )
                        .into_response(),
                );
            };
            let content_type = content_type.to_string();
            // Carry a StreamGuard alongside the proxied stream: it increments
            // live_streams now and decrements when this stream is dropped (the
            // viewer disconnects), so an open feed keeps the phone from being
            // idle-released and the count falls cleanly when they leave.
            use futures_util::StreamExt;
            let guard = stream_guard;
            let upstream = Box::pin(up.bytes_stream());
            // This is an inactivity timeout, not a total stream timeout. Every
            // received chunk resets the 8-second window; if WDA silently stalls
            // after its first frame, close the response so the browser's
            // img.onerror/fallback logic can reconnect.
            let activity_guard = stream_id.map(|stream_id| {
                MjpegActivityGuard::register(state.mjpeg_stream_activity.clone(), stream_id)
            });
            let timed = futures_util::stream::unfold(
                (upstream, guard, activity_guard, false),
                |(mut upstream, guard, activity_guard, done)| async move {
                    if done {
                        return None;
                    }
                    match tokio::time::timeout(MJPEG_INACTIVITY_TIMEOUT, upstream.next()).await {
                        Ok(Some(Ok(bytes))) => {
                            if let Some(activity) = &activity_guard {
                                activity.touch();
                            }
                            Some((
                                Ok::<_, std::io::Error>(bytes),
                                (upstream, guard, activity_guard, false),
                            ))
                        }
                        Ok(Some(Err(error))) => Some((
                            Err(std::io::Error::other(error)),
                            (upstream, guard, activity_guard, true),
                        )),
                        Ok(None) => None,
                        Err(_) => Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "WDA MJPEG stream was idle for 8 seconds",
                            )),
                            (upstream, guard, activity_guard, true),
                        )),
                    }
                },
            );
            let body = Body::from_stream(timed);
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
    if !has_phone_control_header(&headers) {
        return missing_phone_control_header_response();
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

fn inbox_items_response(items: Vec<InboxItem>) -> Response {
    let json = serde_json::json!({ "items": items }).to_string();
    let response = Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    with_security_headers(response)
}

/// `GET /agent/inbox` — safely peek at pending phone results without mutation.
///
/// `?peek=1` remains accepted for compatibility but is now equivalent to the
/// default. Destructive consumption belongs to `POST /agent/inbox/drain`.
async fn agent_inbox_get(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
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
    let items = state
        .inbox
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .cloned()
        .collect();
    inbox_items_response(items)
}

/// `POST /agent/inbox/drain` — atomically consume all pending phone results.
///
/// This state-changing operation requires both bearer authentication (unless
/// explicitly running open mode) and the custom CSRF header.
async fn agent_inbox_drain(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
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
    if !has_phone_control_header(&headers) {
        return missing_phone_control_header_response();
    }
    let items = state
        .inbox
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .drain(..)
        .collect();
    inbox_items_response(items)
}

async fn ws_upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if state.backend != crate::config::DeviceBackend::Mirror {
        return with_security_headers(
            (
                StatusCode::CONFLICT,
                "WebRTC signaling is disabled for the direct device backend",
            )
                .into_response(),
        );
    }
    // Browser WebSockets are not protected by CORS preflight. In open mirror
    // mode, reject a cross-site page before it can acquire a viewer lease or
    // reach the legacy data-channel control path. Non-browser clients may omit
    // Origin; when it is present, it must match this request's Host exactly.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let same_origin = origin
            .parse::<axum::http::Uri>()
            .ok()
            .and_then(|uri| {
                let scheme_ok = matches!(uri.scheme_str(), Some("http") | Some("https"));
                let authority = uri.authority()?.as_str();
                let host = headers.get(header::HOST)?.to_str().ok()?;
                Some(scheme_ok && authority.eq_ignore_ascii_case(host))
            })
            .unwrap_or(false);
        if !same_origin {
            return with_security_headers(
                (StatusCode::FORBIDDEN, "cross-origin WebSocket denied").into_response(),
            );
        }
    }
    if !is_authed(&state, &headers) {
        return with_security_headers((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return with_security_headers(rejection.into_response()),
    };
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
    fn element_snapshot_changes_with_the_actionable_tree() {
        let first = vec![crate::wda::ElementRow {
            kind: "Button".to_string(),
            label: "继续".to_string(),
            identifier: Some("continue-button".to_string()),
            rect: [10.0, 20.0, 80.0, 44.0],
            depth: 2,
            value: None,
            enabled: None,
            visible: None,
            accessible: Some(true),
            focused: None,
            placeholder: None,
        }];
        let mut changed = vec![crate::wda::ElementRow {
            kind: "Button".to_string(),
            label: "继续".to_string(),
            identifier: Some("continue-button".to_string()),
            rect: [10.0, 20.0, 80.0, 44.0],
            depth: 2,
            value: None,
            enabled: None,
            visible: None,
            accessible: Some(true),
            focused: None,
            placeholder: None,
        }];

        let snapshot = element_snapshot_id(&first).unwrap();
        assert_eq!(snapshot, element_snapshot_id(&first).unwrap());
        assert!(!snapshot.is_empty());

        changed[0].rect[1] = 120.0;
        assert_ne!(snapshot, element_snapshot_id(&changed).unwrap());
    }

    fn delta_row(kind: &str, label: &str, y: f64) -> crate::wda::ElementRow {
        crate::wda::ElementRow {
            kind: kind.to_string(),
            label: label.to_string(),
            identifier: None,
            rect: [10.0, y, 80.0, 44.0],
            depth: 2,
            value: None,
            enabled: None,
            visible: None,
            accessible: None,
            focused: None,
            placeholder: None,
        }
    }

    #[test]
    fn diff_identical_trees_is_all_unchanged() {
        let baseline = vec![
            delta_row("Button", "继续", 20.0),
            delta_row("Cell", "设置", 80.0),
        ];
        let current = vec![
            delta_row("Button", "继续", 20.0),
            delta_row("Cell", "设置", 80.0),
        ];

        let delta = diff_element_rows(&baseline, &current);
        assert_eq!(
            delta,
            ElementRowsDelta {
                added: vec![],
                changed: vec![],
                removed: vec![],
                unchanged: 2,
            }
        );
    }

    #[test]
    fn diff_matches_identity_across_insertion_shift() {
        // One row inserted at the top must NOT report every later row as
        // changed — identity matching, not index alignment.
        let baseline = vec![
            delta_row("Button", "继续", 20.0),
            delta_row("Cell", "设置", 80.0),
        ];
        let current = vec![
            delta_row("Other", "新横幅", 0.0),
            delta_row("Button", "继续", 20.0),
            delta_row("Cell", "设置", 80.0),
        ];

        let delta = diff_element_rows(&baseline, &current);
        assert_eq!(delta.added, vec![0]);
        assert_eq!(delta.changed, Vec::<usize>::new());
        assert_eq!(delta.removed, Vec::<usize>::new());
        assert_eq!(delta.unchanged, 2);
    }

    #[test]
    fn diff_reports_changed_state_and_removed_rows() {
        let baseline = vec![
            delta_row("Button", "继续", 20.0),
            delta_row("Cell", "已删除的行", 80.0),
            delta_row("Switch", "飞行模式", 140.0),
        ];
        let mut moved = delta_row("Button", "继续", 20.0);
        moved.rect[1] = 300.0;
        let mut toggled = delta_row("Switch", "飞行模式", 140.0);
        toggled.value = Some("1".to_string());
        let current = vec![moved, toggled];

        let delta = diff_element_rows(&baseline, &current);
        assert_eq!(delta.added, Vec::<usize>::new());
        assert_eq!(delta.changed, vec![0, 1]);
        assert_eq!(delta.removed, vec![1]);
        assert_eq!(delta.unchanged, 0);
    }

    #[test]
    fn diff_pairs_duplicate_identities_in_document_order() {
        // Two rows with the same identity (e.g. two unlabeled TextFields):
        // dropping one is a removal, not a change to the survivor.
        let baseline = vec![
            delta_row("TextField", "", 20.0),
            delta_row("TextField", "", 80.0),
        ];
        let current = vec![delta_row("TextField", "", 20.0)];

        let delta = diff_element_rows(&baseline, &current);
        assert_eq!(delta.added, Vec::<usize>::new());
        assert_eq!(delta.changed, Vec::<usize>::new());
        assert_eq!(delta.removed, vec![1]);
        assert_eq!(delta.unchanged, 1);
    }

    #[test]
    fn elements_delta_json_carries_current_rows_with_indexes() {
        let baseline = vec![delta_row("Button", "继续", 20.0)];
        let current = vec![
            delta_row("Other", "横幅", 0.0),
            delta_row("Button", "继续", 20.0),
        ];
        let delta = diff_element_rows(&baseline, &current);

        let json = elements_delta_json(&delta, &current);
        assert_eq!(json["unchanged"], 1);
        assert_eq!(json["removed"].as_array().unwrap().len(), 0);
        let added = json["added"].as_array().unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0]["index"], 0);
        assert_eq!(added[0]["element"]["label"], "横幅");
    }

    fn validate_action(value: serde_json::Value) -> Result<(), String> {
        validate_agent_action_value(value.as_object().unwrap(), 0)
    }

    #[test]
    fn validate_scroll_accepts_element_mode_and_rejects_mixed_targets() {
        assert!(validate_action(
            serde_json::json!({"type":"scroll","element":3,"snapshot":"abc","dy":120.0})
        )
        .is_ok());
        // Element scroll with coordinates is contradictory.
        assert!(validate_action(
            serde_json::json!({"type":"scroll","element":3,"snapshot":"abc","x":0.5,"dy":120.0})
        )
        .is_err());
        // Element scroll still needs a snapshot and a non-zero delta.
        assert!(
            validate_action(serde_json::json!({"type":"scroll","element":3,"dy":120.0})).is_err()
        );
        assert!(
            validate_action(serde_json::json!({"type":"scroll","element":3,"snapshot":"abc"}))
                .is_err()
        );
        // The classic coordinate mode is untouched.
        assert!(
            validate_action(serde_json::json!({"type":"scroll","x":0.5,"y":0.5,"dy":80.0})).is_ok()
        );
        assert!(validate_action(serde_json::json!({"type":"scroll","x":0.5,"y":0.5})).is_err());
    }

    #[test]
    fn validate_set_value_requires_element_snapshot_and_bounded_value() {
        assert!(validate_action(
            serde_json::json!({"type":"set_value","element":2,"snapshot":"abc","value":"你好"})
        )
        .is_ok());
        // Empty string means "clear the field" and is valid.
        assert!(validate_action(
            serde_json::json!({"type":"set_value","element":2,"snapshot":"abc","value":""})
        )
        .is_ok());
        assert!(validate_action(
            serde_json::json!({"type":"set_value","snapshot":"abc","value":"你好"})
        )
        .is_err());
        assert!(validate_action(
            serde_json::json!({"type":"set_value","element":2,"value":"你好"})
        )
        .is_err());
        assert!(validate_action(
            serde_json::json!({"type":"set_value","element":2,"snapshot":"abc"})
        )
        .is_err());
        let oversized = "字".repeat(1_001);
        assert!(validate_action(
            serde_json::json!({"type":"set_value","element":2,"snapshot":"abc","value":oversized})
        )
        .is_err());
    }

    #[test]
    fn locator_wda_query_uses_element_clickable_predicate_fields() {
        let locator = AgentElementLocator {
            label: Some("保存到“文件”".to_string()),
            identifier: Some("actionGroupCell".to_string()),
            kind: Some("Cell".to_string()),
            value: None,
            focused: Some(false),
            enabled: Some(true),
            visible: Some(true),
        };

        let (using, value) = locator_wda_query(&locator).unwrap();
        assert_eq!(using, "predicate string");
        assert_eq!(
            value,
            "type == 'XCUIElementTypeCell' AND (label == '保存到“文件”' OR name == '保存到“文件”') AND focused == 0 AND enabled == 1 AND visible == 1"
        );
    }

    #[test]
    fn locator_wda_query_falls_back_to_identifier_and_escapes_predicates() {
        let locator = AgentElementLocator {
            label: None,
            identifier: Some("unique-control".to_string()),
            kind: None,
            value: None,
            focused: None,
            enabled: None,
            visible: None,
        };
        assert_eq!(
            locator_wda_query(&locator),
            Some(("accessibility id", "unique-control".to_string()))
        );
        assert_eq!(
            wda_predicate_literal("O'Reilly\\Files"),
            "'O\\'Reilly\\\\Files'"
        );
    }

    #[test]
    fn snapshot_row_locator_uses_semantics_instead_of_system_rectangle() {
        let row = crate::wda::ElementRow {
            kind: "Button".to_string(),
            label: "保存".to_string(),
            identifier: None,
            rect: [358.0, 24.0, 58.0, 36.0],
            depth: 3,
            value: None,
            enabled: Some(true),
            visible: Some(true),
            accessible: Some(true),
            focused: Some(false),
            placeholder: None,
        };

        let locator = snapshot_row_locator(&row).unwrap();
        assert_eq!(locator.label.as_deref(), Some("保存"));
        assert_eq!(locator.kind.as_deref(), Some("Button"));
        assert_eq!(locator.enabled, Some(true));
        assert_eq!(locator.visible, Some(true));
    }

    #[test]
    fn snapshot_row_locator_allows_coordinate_only_without_semantics() {
        let row = crate::wda::ElementRow {
            kind: String::new(),
            label: String::new(),
            identifier: None,
            rect: [10.0, 20.0, 80.0, 44.0],
            depth: 1,
            value: None,
            enabled: None,
            visible: None,
            accessible: None,
            focused: None,
            placeholder: None,
        };

        assert!(snapshot_row_locator(&row).is_none());
    }

    #[test]
    fn batch_expectations_match_application_and_strict_element_state() {
        let rows = vec![
            crate::wda::ElementRow {
                kind: "Application".to_string(),
                label: "招商银行".to_string(),
                identifier: None,
                rect: [0.0, 0.0, 440.0, 956.0],
                depth: 0,
                value: None,
                enabled: None,
                visible: None,
                accessible: None,
                focused: None,
                placeholder: None,
            },
            crate::wda::ElementRow {
                kind: "TextField".to_string(),
                label: "搜索".to_string(),
                identifier: Some("search-field".to_string()),
                rect: [20.0, 80.0, 400.0, 44.0],
                depth: 4,
                value: Some("示例联系人".to_string()),
                enabled: None,
                visible: None,
                accessible: Some(true),
                focused: Some(true),
                placeholder: Some("搜索交易".to_string()),
            },
        ];
        let expect = AgentUiExpectation {
            application: Some("招商银行".to_string()),
            present: vec![AgentElementLocator {
                label: Some("搜索".to_string()),
                identifier: Some("search-field".to_string()),
                kind: Some("TextField".to_string()),
                value: Some("示例联系人".to_string()),
                focused: Some(true),
                enabled: Some(true),
                visible: Some(true),
            }],
            absent: vec![AgentElementLocator {
                label: Some("确认转账".to_string()),
                identifier: None,
                kind: None,
                value: None,
                focused: None,
                enabled: None,
                visible: None,
            }],
        };

        let (matches, observation) = agent_expectation_observation(&rows, &expect);
        assert!(matches);
        assert_eq!(observation["application"], "招商银行");
        assert_eq!(observation["missing_present"], serde_json::json!([]));
        assert_eq!(observation["violated_absent"], serde_json::json!([]));

        let wrong_app = AgentUiExpectation {
            application: Some("聚焦".to_string()),
            present: vec![],
            absent: vec![],
        };
        assert!(!agent_expectation_observation(&rows, &wrong_app).0);
    }

    #[test]
    fn setup_status_accepts_every_setup_script_blocker() {
        for blocker in ["warp", "proxy", "usb", "trust", "ddi", "wda"] {
            let payload = format!(r#"{{"blocked_on":"{blocker}","ts":1000}}"#);
            assert_eq!(parse_setup_blocked_on(&payload, 1100), blocker);
            assert!(
                setup_blocker_hint(blocker).is_some(),
                "{blocker} must have an actionable status hint"
            );
        }
        assert!(setup_blocker_hint("").is_none());
        assert!(setup_blocker_hint("surprise").is_none());
        assert!(setup_blocker_hint("warp").unwrap().contains("fd00::/8"));
        assert!(setup_blocker_hint("warp")
            .unwrap()
            .contains("Traffic only mode"));
    }

    // --- wda_died_reason (#26 §2) ------------------------------------------

    fn health(up: bool, actionable: bool, locked: Option<bool>) -> crate::wda::WdaHealth {
        crate::wda::WdaHealth {
            up,
            actionable,
            locked,
        }
    }

    #[test]
    fn a_severed_session_is_not_blamed_on_a_human() {
        // The reported symptom: WDA still answers /status but every action
        // fails Code=41 after a WARP reconnect or a sleep.
        let reason = classify_wda_death(
            health(true, true, Some(false)),
            health(true, false, Some(false)),
            false,
            false,
        );
        assert_eq!(reason, Some("session_severed"));
        assert!(wda_death_hint("session_severed").contains("WARP"));
    }

    #[test]
    fn death_reasons_separate_the_four_real_causes() {
        let alive = health(true, true, Some(false));
        // Runner/relay gone entirely, or the phone's Wi-Fi address moved.
        assert_eq!(
            classify_wda_death(alive, health(false, false, None), false, false),
            Some("unreachable")
        );
        // Phone locked under a live runner.
        assert_eq!(
            classify_wda_death(alive, health(true, false, Some(true)), false, false),
            Some("device_locked")
        );
        // We stopped it ourselves — nobody needs to go repair anything.
        assert_eq!(
            classify_wda_death(alive, health(false, false, None), true, false),
            Some("idle_release")
        );
        assert_eq!(
            classify_wda_death(alive, health(false, false, None), false, true),
            Some("idle_release")
        );
    }

    #[test]
    fn only_a_fall_from_working_counts_as_a_death() {
        let alive = health(true, true, Some(false));
        let dead = health(false, false, None);
        // Still fine → not a death.
        assert_eq!(classify_wda_death(alive, alive, false, false), None);
        // Was already down → not a NEW death; must not overwrite the real cause
        // recorded at the original transition with a later generic one.
        assert_eq!(classify_wda_death(dead, dead, false, false), None);
        // Coming back up is not a death either.
        assert_eq!(classify_wda_death(dead, alive, false, false), None);
    }

    #[test]
    fn intentional_release_outranks_the_crash_signatures() {
        // An idle release also presents as "up:false" — reporting that as
        // `unreachable` would send agents chasing a phantom outage.
        let alive = health(true, true, Some(false));
        assert_eq!(
            classify_wda_death(alive, health(true, false, Some(true)), true, false),
            Some("idle_release")
        );
    }

    #[test]
    fn every_death_reason_carries_recovery_guidance() {
        for reason in [
            "idle_release",
            "device_locked",
            "session_severed",
            "unreachable",
        ] {
            assert!(
                !wda_death_hint(reason).is_empty(),
                "{reason} has no recovery hint"
            );
        }
        assert_eq!(wda_death_hint(""), "");
        assert_eq!(wda_death_hint("something-new"), "");
    }

    #[test]
    fn recovery_clears_the_recorded_cause() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let slot = Mutex::new(health(true, true, Some(false)));
        let actionable = AtomicBool::new(true);
        let released = AtomicBool::new(false);
        let death = Mutex::new(WdaDeath::default());

        // Dies...
        apply_wda_health_probe_tracked(
            &slot,
            &actionable,
            &released,
            false,
            Some(&death),
            health(true, false, Some(false)),
        );
        assert_eq!(recover(death.lock()).reason, "session_severed");
        assert!(!actionable.load(Ordering::Acquire));

        // ...and comes back. A stale epitaph next to a healthy runner would be
        // read as a live problem.
        apply_wda_health_probe_tracked(
            &slot,
            &actionable,
            &released,
            false,
            Some(&death),
            health(true, true, Some(false)),
        );
        assert_eq!(recover(death.lock()).reason, "");
        assert!(actionable.load(Ordering::Acquire));
    }

    // --- wda_build (#26 §1) ------------------------------------------------

    fn build_status(phase: &str, ts: u64) -> WdaSetupStatus {
        WdaSetupStatus {
            phase: phase.to_string(),
            blocked_on: String::new(),
            message: String::new(),
            ts,
        }
    }

    #[test]
    fn build_state_separates_working_from_gave_up() {
        // The whole point of #26 §1: these two look identical in
        // `setup_blocked_on` (both empty, both wda:false).
        assert_eq!(classify_build_state("building", 90), "building");
        assert_eq!(classify_build_state("building-fail", 90), "failed");
    }

    #[test]
    fn build_state_covers_the_helper_phase_vocabulary() {
        for phase in ["prereq", "ddi-wait", "trust", "serving", "supervisor"] {
            assert_eq!(classify_build_state(phase, 10), "building", "{phase}");
        }
        for phase in [
            "ddi-fail",
            "building-fail",
            "signing-fail",
            "supervisor-fail",
            "daemon-fail",
        ] {
            assert_eq!(classify_build_state(phase, 10), "failed", "{phase}");
        }
        assert_eq!(classify_build_state("ready", 10), "ready");
        assert_eq!(classify_build_state("", 10), "unknown");
    }

    #[test]
    fn build_state_calls_a_silent_helper_stalled_not_building() {
        // setup-wda.sh rewrites its status every poll while building, so this
        // much silence means the process died without writing a -fail phase.
        assert_eq!(
            classify_build_state("building", BUILD_STALE_SECS + 1),
            "stalled"
        );
        // A finished run stays terminal no matter how old it is.
        assert_eq!(classify_build_state("ready", 99_999), "ready");
        assert_eq!(classify_build_state("building-fail", 99_999), "failed");
    }

    #[test]
    fn wda_build_attaches_a_log_tail_only_when_the_log_is_the_answer() {
        let log = || "line one\n\nline two\nboom: xcodebuild failed\n".to_string();

        let failed = derive_wda_build(Some(&build_status("building-fail", 1000)), 1100, log);
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.age_secs, 100);
        assert_eq!(failed.since, 1000);
        assert!(failed.log_tail.contains("boom: xcodebuild failed"));
        // Blank lines are dropped so the tail carries signal, not padding.
        assert!(!failed.log_tail.contains("\n\n"));

        // Mid-build and ready poll constantly; don't ship a log on every poll.
        let building = derive_wda_build(Some(&build_status("building", 1000)), 1100, log);
        assert_eq!(building.state, "building");
        assert!(building.log_tail.is_empty());

        let ready = derive_wda_build(Some(&build_status("ready", 1000)), 1100, log);
        assert_eq!(ready.state, "ready");
        assert!(ready.log_tail.is_empty());

        // A stalled helper is the other case where you must read the log.
        let stalled = derive_wda_build(Some(&build_status("building", 1000)), 9000, log);
        assert_eq!(stalled.state, "stalled");
        assert!(stalled.log_tail.contains("boom"));
    }

    #[test]
    fn wda_build_without_a_status_file_is_unknown_not_failed() {
        let b = derive_wda_build(None, 1000, || "irrelevant".to_string());
        assert_eq!(b.state, "unknown");
        assert_eq!(b.since, 0);
        assert!(b.log_tail.is_empty());
    }

    #[test]
    fn wda_build_json_is_valid_and_escapes_log_text() {
        let b = derive_wda_build(Some(&build_status("building-fail", 1000)), 1100, || {
            "error: \"quoted\"\n\tand a backslash \\ and a newline".to_string()
        });
        let v: serde_json::Value = serde_json::from_str(&b.to_json())
            .expect("wda_build must be valid JSON inside the status body");
        assert_eq!(v["state"], "failed");
        assert_eq!(v["phase"], "building-fail");
        assert_eq!(v["age_secs"], 100);
        assert!(v["log_tail"].as_str().unwrap().contains("\"quoted\""));
    }

    #[test]
    fn build_log_tail_is_bounded_on_both_lines_and_bytes() {
        let many = (0..500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_lines(&many, 12, 1200);
        assert_eq!(tail.lines().count(), 12);
        assert!(tail.contains("line 499"), "keeps the END of the log");
        assert!(!tail.contains("line 400"));

        // A single pathological line is capped by bytes, not left unbounded.
        let huge = "x".repeat(50_000);
        assert!(tail_lines(&huge, 12, 1200).len() <= 1200);
    }

    #[test]
    fn build_log_tail_never_splits_a_utf8_char() {
        // A byte-cap naively applied to CJK build output would panic.
        let cjk = "构建失败：找不到设备\n".repeat(500);
        let tail = tail_lines(&cjk, 12, 1200);
        assert!(tail.len() <= 1200);
        assert!(tail.contains("构建失败"));
    }

    #[test]
    fn setup_status_rejects_stale_unknown_or_invalid_input() {
        assert_eq!(
            parse_setup_blocked_on(r#"{"blocked_on":"wda","ts":1000}"#, 1301),
            ""
        );
        assert_eq!(
            parse_setup_blocked_on(r#"{"blocked_on":"surprise","ts":1000}"#, 1000),
            ""
        );
        assert_eq!(parse_setup_blocked_on("not-json", 1000), "");
    }

    #[test]
    fn setup_status_preserves_fresh_progress_without_calling_it_a_blocker() {
        let status = parse_setup_status(
            r#"{"phase":"building","blocked_on":"","message":"building + launching WDA (90s elapsed)","ts":1000}"#,
            1100,
        )
        .unwrap();
        assert_eq!(status.phase, "building");
        assert_eq!(status.blocked_on, "");
        assert_eq!(status.message, "building + launching WDA (90s elapsed)");
    }

    #[test]
    fn setup_log_fallback_recognizes_usb_failure_from_latest_attempt_only() {
        let unplugged = "\u{1b}[1m== Checking prerequisites\u{1b}[0m\n\
            == Resolving target device\n\
            target 00008150-000A60EC1A02401C is not currently connected over USB.";
        assert_eq!(parse_setup_log_blocked_on(unplugged), "usb");

        let recovered = "== Checking prerequisites\n\
            target 00008150-000A60EC1A02401C is not currently connected over USB.\n\
            == Checking prerequisites\n\
            iPhone on USB: 00008150-000A60EC1A02401C\n\
            prerequisites passed";
        assert_eq!(parse_setup_log_blocked_on(recovered), "");
        assert_eq!(
            parse_setup_log_blocked_on("USB relay diagnostics completed"),
            ""
        );
    }

    #[test]
    fn setup_log_fallback_recognizes_warp_from_latest_attempt_only() {
        let connected = "== Checking prerequisites\n\
            WARP is ON and will block WDA (the CoreDevice tunnel dies).";
        assert_eq!(parse_setup_log_blocked_on(connected), "warp");

        let recovered = "== Checking prerequisites\n\
            WARP is ON and will block WDA (the CoreDevice tunnel dies).\n\
            == Checking prerequisites\n\
            System proxies (HTTP/HTTPS/SOCKS): none enabled\n\
            prerequisites passed";
        assert_eq!(parse_setup_log_blocked_on(recovered), "");

        let missing_bypass = "== Checking prerequisites\n\
            WARP is connected, but its effective Split Tunnel exclusions do not cover the CoreDevice device tunnel.";
        assert_eq!(parse_setup_log_blocked_on(missing_bypass), "warp");
    }

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
        assert!(
            !limiter.is_locked(),
            "4 failures should not trigger lockout (max=5)"
        );
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
        assert!(
            !limiter.is_locked(),
            "expired lockout should not block requests"
        );
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
        assert_eq!(
            v["iceServers"][0]["urls"][0],
            "stun:stun.l.google.com:19302"
        );
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
        assert!(INDEX_HTML.contains("id=\"flowPanel\""));
        assert!(INDEX_HTML.contains("aria-label=\"录制并运行自动化流程\""));
        assert!(INDEX_HTML.contains("id=\"flowAvailability\""));
        assert!(INDEX_HTML.contains("id=\"flowSafetyGate\""));
        assert!(INDEX_HTML.contains("id=\"flowOpenFile\""));
        assert!(INDEX_HTML.contains("validateImportedFlowDocument"));
        assert!(INDEX_HTML.contains("正常重放不需要 AI 逐步操作"));
        assert!(INDEX_HTML.contains("写死了文字；请改用 input 运行参数"));
        assert!(INDEX_HTML.contains("文字只会变成运行参数"));
        assert!(INDEX_HTML.contains("只用于本次执行，不写入 JSON"));
        assert!(INDEX_HTML.contains("document.inputs = Object.fromEntries"));
        assert!(INDEX_HTML.contains("kind: 'type'"));
        assert!(INDEX_HTML.contains("input: key"));
        assert!(INDEX_HTML.contains("界面检查点"));
        assert!(INDEX_HTML.contains("chooseFlowCheckpoint"));
        assert!(INDEX_HTML.contains("kind: 'wait_for'"));
        assert!(INDEX_HTML.contains("fetch('/agent/actions'"));
        assert!(INDEX_HTML.contains("X-Phone-Control"));
        assert!(INDEX_HTML.contains("function managedSetupWillRetry"));
        assert!(INDEX_HTML.contains("连接后会自动继续"));
        assert!(INDEX_HTML.contains("fd00::/8"));
        assert!(INDEX_HTML.contains("fe80::/10"));
        assert!(INDEX_HTML.contains("Traffic only + Split Tunnels Include"));
        assert!(!INDEX_HTML.contains("请手动断开 WARP"));
        assert!(!INDEX_HTML.contains("请连接并解锁 iPhone，保持亮屏，然后在手机上点「信任」"));
        assert!(INDEX_HTML
            .contains("a, button, input, textarea, select, summary, [contenteditable=\"true\"]"));
    }

    #[test]
    fn embedded_setup_html_is_the_connection_guide() {
        assert!(SETUP_HTML.contains("连接真实 iPhone"));
        assert!(SETUP_HTML.contains("fetch('/agent/status'"));
        assert!(SETUP_HTML.contains("setup_blocked_on"));
        assert!(SETUP_HTML.contains("recovery_owner"));
        assert!(SETUP_HTML.contains("aria-disabled=\"true\""));
        assert!(SETUP_HTML.contains("href=\"/phone\""));
        assert!(SETUP_HTML.contains("fd00::/8"));
        assert!(SETUP_HTML.contains("fe80::/10"));
        assert!(SETUP_HTML.contains("Traffic only mode"));
        assert!(!SETUP_HTML.contains("id=\"copyBlocker\""));
        assert!(!SETUP_HTML.contains("是否断开 VPN 由你决定"));
        assert!(!SETUP_HTML.contains("/agent/mode"));
        assert!(!SETUP_HTML.contains("/agent/actions"));
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

    #[test]
    fn launch_agent_values_are_xml_escaped() {
        assert_eq!(
            xml_escape(r#"/Users/A&B/<phone>"quoted".sh"#),
            "/Users/A&amp;B/&lt;phone&gt;&quot;quoted&quot;.sh"
        );
    }

    #[test]
    fn plist_staging_preserves_live_file_until_atomic_rename() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("agent.plist");
        std::fs::write(&live, b"old").unwrap();

        let staged = stage_file(&live, b"new").unwrap();
        assert_eq!(std::fs::read(&live).unwrap(), b"old");
        assert_eq!(std::fs::read(&staged).unwrap(), b"new");

        std::fs::rename(&staged, &live).unwrap();
        assert_eq!(std::fs::read(&live).unwrap(), b"new");
    }

    #[test]
    fn managed_wda_target_requires_a_canonical_udid() {
        assert!(valid_wda_udid("00008110-001234567890001E"));
        assert!(!valid_wda_udid(""));
        assert!(!valid_wda_udid("phone one"));
        assert!(!valid_wda_udid("../other-device"));
    }

    #[test]
    fn normalized_wda_coordinates_stay_inside_touchable_bounds() {
        assert_eq!(normalized_wda_axis(0.0, 390.0).unwrap(), 1.0);
        assert_eq!(normalized_wda_axis(1.0, 390.0).unwrap(), 389.0);
        assert_eq!(normalized_wda_axis(0.5, 390.0).unwrap(), 195.0);
        assert!(normalized_wda_axis(0.5, 2.0).is_err());
        assert!(normalized_wda_axis(f64::NAN, 390.0).is_err());
    }

    #[test]
    fn idle_release_aborts_stuck_health_probe_but_never_pending_control() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let control_pending = std::sync::atomic::AtomicUsize::new(0);
            let stuck = tokio::spawn(std::future::pending::<()>());
            let stuck_abort = stuck.abort_handle();
            let slot = Mutex::new(Some(stuck));

            assert!(abort_health_probe_for_idle(&control_pending, &slot));
            tokio::task::yield_now().await;
            assert!(recover(slot.lock()).is_none());
            assert!(stuck_abort.is_finished());

            control_pending.store(1, std::sync::atomic::Ordering::Release);
            let protected = tokio::spawn(std::future::pending::<()>());
            let protected_abort = protected.abort_handle();
            *recover(slot.lock()) = Some(protected);

            assert!(!abort_health_probe_for_idle(&control_pending, &slot));
            tokio::task::yield_now().await;
            assert!(recover(slot.lock()).is_some());
            assert!(!protected_abort.is_finished());
            recover(slot.lock()).take().unwrap().abort();
        });
    }

    #[test]
    fn wda_lifecycle_serializes_release_and_reconnect_in_both_orders() {
        let lifecycle = WdaLifecycle::new();

        assert!(lifecycle.try_begin_reconnecting());
        assert!(lifecycle.is_reconnecting());
        assert!(!lifecycle.try_begin_releasing());
        lifecycle.finish_reconnecting();

        assert!(lifecycle.try_begin_releasing());
        assert!(lifecycle.is_releasing());
        assert!(!lifecycle.try_begin_reconnecting());
        lifecycle.finish_releasing();

        assert!(!lifecycle.is_transitioning());
    }

    #[test]
    fn simultaneous_wda_lifecycle_starts_have_exactly_one_owner() {
        let lifecycle = std::sync::Arc::new(WdaLifecycle::new());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let reconnect_lifecycle = lifecycle.clone();
        let reconnect_barrier = barrier.clone();
        let reconnect = std::thread::spawn(move || {
            reconnect_barrier.wait();
            reconnect_lifecycle.try_begin_reconnecting()
        });
        let release_lifecycle = lifecycle.clone();
        let release_barrier = barrier.clone();
        let release = std::thread::spawn(move || {
            release_barrier.wait();
            release_lifecycle.try_begin_releasing()
        });

        barrier.wait();
        let reconnect_won = reconnect.join().unwrap();
        let release_won = release.join().unwrap();
        assert_ne!(reconnect_won, release_won);

        if reconnect_won {
            assert!(lifecycle.is_reconnecting());
            lifecycle.finish_reconnecting();
        } else {
            assert!(lifecycle.is_releasing());
            lifecycle.finish_releasing();
        }
        assert!(!lifecycle.is_transitioning());
    }

    #[test]
    fn locked_but_up_reconnect_clears_released_before_timeout_finishes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let health_slot = Mutex::new(crate::wda::WdaHealth::down());
        let actionable = AtomicBool::new(false);
        let released = AtomicBool::new(true);
        let lifecycle = WdaLifecycle::new();
        assert!(lifecycle.try_begin_reconnecting());
        let locked = crate::wda::WdaHealth {
            up: true,
            actionable: false,
            locked: Some(true),
        };

        assert!(!apply_wda_health_probe(
            &health_slot,
            &actionable,
            &released,
            locked,
        ));
        assert!(!released.load(Ordering::Acquire));
        assert!(!actionable.load(Ordering::Acquire));
        let cached = *recover(health_slot.lock());
        assert!(cached.up);
        assert!(!cached.actionable);
        assert_eq!(cached.locked, Some(true));

        // Model the readiness deadline expiring without actionability: the
        // runner still owns the device, while reconnecting ends and status can
        // honestly tell the user to unlock instead of reconnecting again.
        finish_wda_readiness_wait(&lifecycle);
        assert!(!lifecycle.is_reconnecting());
        assert_eq!(recover(health_slot.lock()).locked, Some(true));
    }

    #[test]
    fn direct_control_deadline_is_server_monotonic_and_bounded() {
        let now = tokio::time::Instant::now();
        let valid = serde_json::json!({
            "type": "tap",
            "ttl_ms": 2000,
            // A remote browser's wall clock is audit-only and may differ.
            "issued_at_ms": 1
        });
        let deadline = direct_control_deadline(&valid, now).unwrap();
        assert_eq!(
            deadline.duration_since(now),
            std::time::Duration::from_millis(2000)
        );
        assert_eq!(
            direct_control_deadline(&serde_json::json!({"ttl_ms": 0}), now),
            Err(ControlFreshnessError::Invalid)
        );
        assert_eq!(
            direct_control_deadline(&serde_json::json!({"ttl_ms": 2501}), now),
            Err(ControlFreshnessError::Invalid)
        );
        assert_eq!(
            direct_control_deadline(&serde_json::json!({}), now),
            Err(ControlFreshnessError::Missing)
        );
    }

    #[test]
    fn stream_guard_reserves_viewer_slot_atomically() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(3));
        let fourth = StreamGuard::try_reserve(count.clone(), 4).expect("fourth slot");
        assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 4);
        assert!(StreamGuard::try_reserve(count.clone(), 4).is_none());
        drop(fourth);
        assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 3);
    }

    #[test]
    fn mjpeg_stream_ids_are_bounded_and_url_safe() {
        assert!(valid_mjpeg_stream_id("browser_01234567"));
        assert!(valid_mjpeg_stream_id("ABC-def_123"));
        assert!(!valid_mjpeg_stream_id("short"));
        assert!(!valid_mjpeg_stream_id("contains/slash"));
        assert!(!valid_mjpeg_stream_id("contains space"));
        assert!(!valid_mjpeg_stream_id(&"a".repeat(65)));
    }

    #[test]
    fn stale_mjpeg_guard_cannot_remove_a_newer_stream_registration() {
        let activity = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let older = MjpegActivityGuard::register(activity.clone(), "browser_01234567".into());
        let newer = MjpegActivityGuard::register(activity.clone(), "browser_01234567".into());

        drop(older);
        assert!(
            recover(activity.lock()).contains_key("browser_01234567"),
            "dropping an old response must not erase the replacement stream heartbeat"
        );

        drop(newer);
        assert!(recover(activity.lock()).is_empty());
    }

    #[test]
    fn mjpeg_proxy_rejects_successful_html_responses() {
        assert!(is_mjpeg_content_type(
            "multipart/x-mixed-replace; boundary=--BoundaryString"
        ));
        assert!(is_mjpeg_content_type("Multipart/X-Mixed-Replace"));
        assert!(!is_mjpeg_content_type("text/html; charset=utf-8"));
        assert!(!is_mjpeg_content_type("image/jpeg"));
        assert!(!is_mjpeg_content_type(""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn child_deadline_kills_a_wedged_process() {
        let started = Instant::now();
        let result = run_child_with_deadline(
            std::process::Command::new("/bin/sleep").arg("2"),
            std::time::Duration::from_millis(50),
        );
        assert!(matches!(result, Err(DevicectlError::Timeout)));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
