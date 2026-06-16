//! `iphone-use` daemon CLI.
//!
//! ```text
//! iphone-use serve   # TCC preflight → start capture/encode → axum on host:port
//! iphone-use stop    # best-effort: kill the recorded pid
//! ```
//!
//! The `serve` path ties together every validated module:
//!   1. TCC preflight (`CGPreflightScreenCaptureAccess` + `AXIsProcessTrusted`) —
//!      print the Settings panes and refuse to start if either is missing.
//!   2. `NSApplicationLoad()` to bootstrap AppKit/CG before any SCK call.
//!   3. Load [`server::config::Config`] from the environment.
//!   4. Mint/read the signing secret via [`server::runtime_dir`].
//!   5. `core::encode::start_pipeline(...)` — SCK capture + VideoToolbox H.264.
//!   6. Build the input `SessionGeometry` from the Mirroring window.
//!   7. Start the axum app on `host:port`; print the local URL + password.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use server::config::Config;
use server::http::{self, AppState};

/// PID file name inside the runtime dir.
const PID_FILE: &str = "iphone-use.pid";
/// Secret file name inside the runtime dir.
const SECRET_FILE: &str = "secret";

#[derive(Parser)]
#[command(name = "iphone-use", about = "iPhone Mirroring → WebRTC remote daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon: capture + encode + WebRTC server.
    Serve,
    /// Stop a running daemon (best-effort; kills the recorded pid).
    Stop,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve => serve(),
        Command::Stop => stop(),
    }
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

fn serve() -> Result<()> {
    // 1. TCC preflight (macOS). Refuse to start without both grants.
    preflight_tcc()?;

    // 2. Bootstrap AppKit/CG before any ScreenCaptureKit call (must be main thread).
    server::macos::ns_application_load();

    // 3. Config.
    let cfg = Config::from_env();

    // 4. Runtime dir + signing secret + pid file.
    let dir = server::runtime_dir::runtime_dir().context("create runtime dir")?;
    let secret = load_or_make_secret(&dir, &cfg)?;
    write_pid(&dir)?;

    // 5. Start the capture/encode pipeline.
    let pipeline = core::encode::start_pipeline(core::encode::PipelineConfig::default())
        .context("start_pipeline (capture + VideoToolbox H.264)")?;

    // 6. Input geometry from the Mirroring window.
    let geometry = core::capture::find_mirroring_geometry()
        .context("find iPhone Mirroring window geometry for input mapping")?;
    tracing::info!(
        "input geometry: content_rect={:?} scale={:.2} orientation={:?}",
        geometry.content_rect,
        geometry.scale,
        geometry.orientation
    );

    tracing::info!("input fully native (CGEvent) — zero external runtime dependencies");

    // 7. ICE servers (STUN + optional static env TURN). Cloudflare dynamic TURN,
    //    if configured, is minted + refreshed inside the async runtime and
    //    hot-swapped into this `ArcSwap` (see `run_server`).
    let ice_servers = http::build_ice_servers(
        std::env::var("PHONE_REMOTE_TURN_URLS").ok(),
        std::env::var("PHONE_REMOTE_TURN_USERNAME").ok(),
        std::env::var("PHONE_REMOTE_TURN_CREDENTIAL").ok(),
    );
    let ice = Arc::new(arc_swap::ArcSwap::from_pointee(http::IceState::new(ice_servers)));

    // Control lease + input injector (drains decoded events on its own thread,
    // gated on the human lease being current).
    let control = Arc::new(Mutex::new(core::control::Control::new()));
    let current_lease = Arc::new(Mutex::new(None::<core::control::Lease>));
    let injector = {
        let control = control.clone();
        let current_lease = current_lease.clone();
        server::input_bridge::spawn_injector(geometry, move || {
            // Recover from a poisoned mutex rather than killing the injector
            // thread permanently — the lease state is a small struct and stays
            // consistent across a panic.
            let lease = current_lease.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*lease {
                Some(l) => control.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_current(l),
                None => false,
            }
        })
    };

    // The daemon always serves plain HTTP; whether the cookie is marked `Secure`
    // is decided per-request from `X-Forwarded-Proto` (the Cloudflare tunnel sets
    // it to https). Binding to a LAN IP (0.0.0.0) is still plain HTTP, so forcing
    // Secure here would make browsers reject the cookie over LAN and break /ws
    // auth → WebRTC. Keep this `false`; per-request HTTPS is detected in http.rs.
    let cookie_secure = false;
    let state = Arc::new(AppState {
        pipeline,
        ice,
        password: cfg.password.clone(),
        secret,
        session_ttl_secs: cfg.session_ttl_secs,
        cookie_secure,
        // Same shared handles the injector gate reads — a lease change made by the
        // signaling layer is immediately visible to the injector thread.
        control,
        current_lease,
        injector,
        auth_limiter: Arc::new(Mutex::new(http::AuthLimiter::new())),
        agent_token: cfg.agent_token.clone(),
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        // L2 element-tree control: point PHONE_REMOTE_WDA_URL at a running
        // WebDriverAgent on the phone (e.g. http://<phone-ip>:8100) and agent
        // text/taps auto-route through it (CJK direct, no host cursor); unset
        // = pure L3 pixel path, exactly as before.
        wda: std::env::var("PHONE_REMOTE_WDA_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .and_then(|url| match server::wda::WdaClient::new(&url) {
                Ok(c) => {
                    tracing::info!("L2 element control enabled via WDA at {url}");
                    Some(Arc::new(tokio::sync::Mutex::new(c)))
                }
                Err(e) => {
                    tracing::warn!("PHONE_REMOTE_WDA_URL set but client failed: {e:#}");
                    None
                }
            }),
        latest_release: Arc::new(Mutex::new(None)),
        viewers: Arc::new(Mutex::new(server::signaling::ViewerRegistry::default())),
        mirror_paused_cache: Arc::new(Mutex::new(None)),
        // WDA's MJPEG stream for live video in agent mode (see /agent/mjpeg).
        // Only when WDA is configured; the relay forwards it to 127.0.0.1:9100
        // (override with PHONE_REMOTE_WDA_MJPEG_URL).
        mjpeg_url: std::env::var("PHONE_REMOTE_WDA_MJPEG_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("PHONE_REMOTE_WDA_URL")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|_| "http://127.0.0.1:9100".to_string())
            }),
        wda_actionable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });

    run_server(cfg, state)
}

/// Normalized center of the recovery button on the Mirroring interstitials —
/// "Resume" on "Connection Paused" (≈980/1562 = 0.627) and "Connect" on "iPhone
/// in Use" (≈1014/1562 = 0.649). 0.64 sits in both buttons' overlap; x is dead
/// center. Hardware-measured from 708×1562 captures; stable vs the content rect.
const RECOVERY_BUTTON: (f64, f64) = (0.5, 0.64);

/// Auto-recover the Mirroring interstitials by clicking their recovery button
/// (issue #3). **Opt-in / experimental — default OFF.**
///
/// Why off by default: macOS will NOT let a background LaunchAgent bring the
/// iPhone Mirroring window frontmost while the phone is in active use ("iPhone
/// in Use" — the screen literally says "Lock your iPhone to connect"), so the
/// synthetic click is dropped (`could not be brought frontmost`). It can work
/// for "Connection Paused" (phone locked, human idle), but not reliably enough
/// to enable unattended. The honest signal — `mirror_state` + `drivable` in
/// `/agent/status` — tells a human/agent WHEN to click Resume/Connect manually.
///
/// Set `PHONE_REMOTE_AUTO_RESUME=1` to try it anyway (rate-limited; clicks via
/// the injector thread; skipped while WDA is active).
fn spawn_pause_watchdog(state: Arc<AppState>) {
    if !std::env::var("PHONE_REMOTE_AUTO_RESUME").is_ok_and(|v| !v.is_empty() && v != "0") {
        return; // disabled by default — see doc comment
    }
    tracing::info!("auto-resume watchdog enabled (PHONE_REMOTE_AUTO_RESUME)");
    tokio::spawn(async move {
        use std::time::{Duration, Instant};
        const POLL: Duration = Duration::from_secs(5);
        // Hardware lesson (2026-06-12): a Mirroring reconnect handshake takes
        // 10–30s, and a tap landing mid-handshake CANCELS it — an aggressive
        // retry loop turns "connects fine" into "connects then always drops"
        // (observed live; the blind-tapping session monitor caused exactly
        // that). Cool down long enough for a full handshake to finish.
        const COOLDOWN: Duration = Duration::from_secs(45);
        let mut last_attempt: Option<Instant> = None;
        loop {
            tokio::time::sleep(POLL).await;
            // Mirror isn't in play while WDA drives the phone on-device.
            if let Some(wda) = &state.wda {
                if wda.lock().await.is_up().await {
                    continue;
                }
            }
            let mstate = tokio::task::spawn_blocking(|| {
                core::capture::mirroring_state().unwrap_or(core::capture::MirrorState::Active)
            })
            .await
            .unwrap_or(core::capture::MirrorState::Active);
            // Only the "Connection Paused" interstitial is recoverable by a
            // click. "iPhone in Use" is NOT: the on-screen Connect button does
            // nothing while the phone is in active use (hardware-verified —
            // /agent/status says exactly this in its hint), and the tap itself
            // keeps poking the session. Wait in_use out instead.
            use core::capture::MirrorState;
            if !matches!(mstate, MirrorState::Paused) {
                continue;
            }
            if last_attempt.is_some_and(|t| t.elapsed() < COOLDOWN) {
                continue; // still cooling down from the last click
            }
            last_attempt = Some(Instant::now());
            tracing::info!("Mirroring {} — auto-recover: tapping recovery button (issue #3)", mstate.as_str());
            // Enqueue the click on the INJECTOR thread (the same path agent taps
            // use): it reliably brings Mirroring frontmost, whereas a direct
            // CGEvent from this tokio blocking thread does not (NSWorkspace /
            // app activation misbehaves off the injector thread). Take a short
            // Agent lease first so the injector's gate permits the event.
            {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mut control = state
                    .control
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let lease =
                    control.acquire(core::control::Holder::Agent("auto-recover".into()), now);
                *state
                    .current_lease
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lease);
            }
            state
                .injector
                .send(core::input::InputEvent::Tap { x: RECOVERY_BUTTON.0, y: RECOVERY_BUTTON.1 });
        }
    });
}

/// Background update check: resolve the repo's latest release tag every 24h
/// and stash it in `AppState.latest_release` for `/agent/status` to report.
///
/// Uses the `releases/latest` REDIRECT (no api.github.com): the web tier has
/// no anonymous rate limit (the API caps at 60 req/h per IP — a hardware-
/// tested failure mode, see install.sh), and the Location header carries the
/// tag: `https://github.com/<repo>/releases/tag/v0.2.0`. Set
/// `PHONE_REMOTE_NO_UPDATE_CHECK=1` to disable (air-gapped / privacy).
fn spawn_update_check(state: Arc<AppState>) {
    if std::env::var("PHONE_REMOTE_NO_UPDATE_CHECK").is_ok_and(|v| !v.is_empty() && v != "0") {
        tracing::info!("update check disabled (PHONE_REMOTE_NO_UPDATE_CHECK)");
        return;
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(15))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("update check disabled (client build failed): {e:#}");
                return;
            }
        };
        loop {
            match client
                .get("https://github.com/leeguooooo/iphone-use/releases/latest")
                .send()
                .await
            {
                Ok(resp) => {
                    let tag = resp
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|l| l.to_str().ok())
                        .and_then(|l| l.rsplit("/tag/").next().map(str::to_string))
                        .filter(|t| t.starts_with('v'));
                    if let Some(tag) = tag {
                        let current = env!("CARGO_PKG_VERSION");
                        if tag.trim_start_matches('v') != current {
                            tracing::info!(
                                "update available: {tag} (running v{current}) — \
                                 curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh"
                            );
                        }
                        *state
                            .latest_release
                            .lock()
                            .unwrap_or_else(|p| p.into_inner()) = Some(tag);
                    }
                }
                Err(e) => tracing::debug!("update check fetch failed (will retry): {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
        }
    });
}

/// Build the tokio runtime and serve.
fn run_server(cfg: Config, state: Arc<AppState>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    rt.block_on(async move {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind {addr}"))?;

        print_startup_banner(&cfg);

        // Cloudflare dynamic TURN: if a TURN key is configured, mint ephemeral
        // credentials now and refresh them before they expire, hot-swapping the
        // shared ICE state. Absent config, the daemon stays on STUN + static env
        // TURN (the initial value already in `state.ice`).
        if let Some(cf) = server::turn::CfTurnConfig::from_env() {
            spawn_cloudflare_turn_refresh(cf, state.ice.clone());
        }

        // Daily release check → /agent/status {version, latest, update_available}.
        spawn_update_check(state.clone());

        // Auto-recover the Mirroring "Connection Paused" screen (issue #3).
        spawn_pause_watchdog(state.clone());

        let app = http::router(state);
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("axum serve")?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Print the local URL + password so the operator can open the client.
fn print_startup_banner(cfg: &Config) {
    let url = format!("http://{}:{}/phone", cfg.host, cfg.port);
    eprintln!("──────────────────────────────────────────────");
    eprintln!(" iphone-use serving");
    eprintln!("   url:      {url}");
    match &cfg.password {
        Some(_) => eprintln!("   password: (set via PHONE_REMOTE_PASSWORD)"),
        None => eprintln!("   password: (none — open LAN mode)"),
    }
    match &cfg.agent_token {
        Some(_) => eprintln!("   agent:    (dedicated token set via PHONE_REMOTE_AGENT_TOKEN)"),
        None => eprintln!("   agent:    (no dedicated token — uses password for bearer auth)"),
    }
    if cfg.host == "127.0.0.1" {
        eprintln!("   note:     bound to 127.0.0.1; set PHONE_REMOTE_HOST=0.0.0.0 for LAN access");
    }
    eprintln!("──────────────────────────────────────────────");
}

/// Spawn the Cloudflare TURN refresh loop.
///
/// Mints ephemeral TURN credentials, hot-swaps them (alongside STUN + any static
/// env TURN) into the shared ICE state, and re-mints before they expire. On a
/// mint error it keeps the last-good (or initial STUN-only) ICE state and retries
/// shortly — the daemon never goes credential-less.
fn spawn_cloudflare_turn_refresh(
    cf: server::turn::CfTurnConfig,
    ice: Arc<arc_swap::ArcSwap<http::IceState>>,
) {
    // Base = STUN + any static env TURN; the CF ephemeral relay is appended each refresh.
    let base = http::build_ice_servers(
        std::env::var("PHONE_REMOTE_TURN_URLS").ok(),
        std::env::var("PHONE_REMOTE_TURN_USERNAME").ok(),
        std::env::var("PHONE_REMOTE_TURN_CREDENTIAL").ok(),
    );
    tokio::spawn(async move {
        loop {
            match server::turn::mint(&cf).await {
                Ok(cf_server) => {
                    let mut servers = base.clone();
                    servers.push(cf_server);
                    ice.store(std::sync::Arc::new(http::IceState::new(servers)));
                    tracing::info!(
                        "cloudflare TURN credentials refreshed (ttl {}s)",
                        cf.ttl_secs
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(cf.refresh_after_secs()))
                        .await;
                }
                Err(e) => {
                    tracing::warn!("cloudflare TURN mint failed: {e:#}; retrying in 60s");
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            }
        }
    });
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}

// ---------------------------------------------------------------------------
// TCC preflight
// ---------------------------------------------------------------------------

fn preflight_tcc() -> Result<()> {
    let status = server::macos::tcc_status();
    if status.ok() {
        return Ok(());
    }
    eprintln!("permission preflight failed:");
    if !status.screen_recording {
        eprintln!(
            "  • Screen Recording NOT granted.\n    \
             System Settings → Privacy & Security → Screen Recording → enable this app/terminal"
        );
    }
    if !status.accessibility {
        eprintln!(
            "  • Accessibility NOT granted.\n    \
             System Settings → Privacy & Security → Accessibility → enable this app/terminal"
        );
    }
    // Prompt for screen capture if missing (no-op if already granted).
    if !status.screen_recording {
        server::macos::request_screen_capture();
    }
    anyhow::bail!("missing TCC permissions; grant them and re-run `iphone-use serve`")
}

// ---------------------------------------------------------------------------
// secret + pid management
// ---------------------------------------------------------------------------

/// Load the signing secret, preferring (in order): the configured
/// `PHONE_REMOTE_SECRET`, an existing secret file, or a freshly generated one.
fn load_or_make_secret(dir: &std::path::Path, cfg: &Config) -> Result<Vec<u8>> {
    if let Some(s) = &cfg.secret {
        return Ok(s.clone().into_bytes());
    }
    // Try to read an existing secret file.
    match server::runtime_dir::read_secret(dir, SECRET_FILE) {
        Ok(bytes) if !bytes.is_empty() => return Ok(bytes),
        _ => {}
    }
    // Generate a new 32-byte secret and persist it (best-effort).
    let secret = gen_secret();
    let _ = server::runtime_dir::write_secret(dir, SECRET_FILE, &secret);
    Ok(secret)
}

/// Generate 32 random-ish bytes for the session-signing secret.
///
/// Uses time + pid + address entropy. This is a session-cookie HMAC key for a
/// LAN daemon, not a long-term credential; it is regenerated on each fresh start
/// when no secret is configured/persisted.
fn gen_secret() -> Vec<u8> {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    seed ^= std::process::id() as u64;
    let stack_addr = &seed as *const _ as u64;
    seed ^= stack_addr.rotate_left(17);
    // SplitMix64 expansion to 32 bytes.
    let mut out = Vec::with_capacity(32);
    let mut x = seed;
    for _ in 0..4 {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out
}

/// Write the current pid into the runtime dir (overwriting any stale file).
fn write_pid(dir: &std::path::Path) -> Result<()> {
    let path = dir.join(PID_FILE);
    // Overwrite freely (write_secret refuses to clobber; pid is not a secret).
    std::fs::write(&path, std::process::id().to_string())
        .with_context(|| format!("write pid file {path:?}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

fn stop() -> Result<()> {
    let dir = server::runtime_dir::runtime_dir().context("locate runtime dir")?;
    let path = dir.join(PID_FILE);
    let pid_str = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("no running daemon (no pid file at {path:?})");
            return Ok(());
        }
    };
    let pid: i32 = pid_str
        .trim()
        .parse()
        .with_context(|| format!("invalid pid in {path:?}: {pid_str:?}"))?;
    // SAFETY: kill is a simple signal send; we send SIGTERM for a graceful stop.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc == 0 {
        eprintln!("sent SIGTERM to pid {pid}");
        let _ = std::fs::remove_file(&path);
    } else {
        eprintln!("could not signal pid {pid} (already gone?)");
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}
