//! `iphone-remote` daemon CLI.
//!
//! ```text
//! iphone-remote serve   # TCC preflight → start capture/encode → axum on host:port
//! iphone-remote stop    # best-effort: kill the recorded pid
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
const PID_FILE: &str = "iphone-remote.pid";
/// Secret file name inside the runtime dir.
const SECRET_FILE: &str = "secret";

#[derive(Parser)]
#[command(name = "iphone-remote", about = "iPhone Mirroring → WebRTC remote daemon")]
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

/// Resolve the iPhone Mirroring window's `(pid, window_id)` by parsing
/// `cua-driver call list_windows`. Picks the largest on-screen phone-shaped
/// Mirroring window (skips menubar extras + the setup/welcome window), matching
/// the validated `core::window::select_mirroring` heuristic. `None` if cua-driver
/// is unavailable or nothing matches — key/text/shortcut are then skipped.
fn find_cua_target(cua_driver: &std::path::Path) -> Option<(i32, u32)> {
    let out = std::process::Command::new(cua_driver)
        .args(["call", "list_windows", "{}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let wins = v.get("windows")?.as_array()?;
    let mut best: Option<(i32, u32, bool, f64)> = None; // (pid, wid, on_screen, area)
    for w in wins {
        let app = w.get("app_name").and_then(|x| x.as_str()).unwrap_or("");
        let bundle = w
            .get("bundle_identifier")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let title = w.get("title").and_then(|x| x.as_str()).unwrap_or("");
        let hay = format!("{app}\u{1}{bundle}\u{1}{title}").to_lowercase();
        let is_mirror = hay.contains("iphone mirroring")
            || hay.contains("screencontinuity")
            || hay.contains('\u{955c}'); // 镜 (镜像)
        if !is_mirror {
            continue;
        }
        let b = match w.get("bounds") {
            Some(b) => b,
            None => continue,
        };
        let width = b.get("width").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let height = b.get("height").and_then(|x| x.as_f64()).unwrap_or(0.0);
        if !((200.0..=900.0).contains(&width) && (400.0..=1600.0).contains(&height)) {
            continue; // skip menubar extras / off-shape
        }
        if title.to_lowercase().contains("welcome") || title.contains('\u{6b22}') {
            continue; // skip the setup/welcome window (欢迎)
        }
        let pid = w.get("pid").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
        let wid = w.get("window_id").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        if pid == 0 || wid == 0 {
            continue;
        }
        let on = w
            .get("is_on_screen")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let area = width * height;
        let better = match best {
            None => true,
            Some((_, _, b_on, b_area)) => (on, area) > (b_on, b_area),
        };
        if better {
            best = Some((pid, wid, on, area));
        }
    }
    best.map(|(pid, wid, _, _)| (pid, wid))
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

    // 6b. Resolve the Mirroring window's (pid, window_id) via cua-driver, for
    // key/text/shortcut injection (cua-driver `call press_key`/`type_text`).
    let cua_path = cfg.cua_driver.to_string_lossy().to_string();
    let cua_target = find_cua_target(&cfg.cua_driver).map(|(pid, wid)| (cua_path.clone(), pid, wid));
    match &cua_target {
        Some((_, pid, wid)) => {
            tracing::info!("cua-driver input target: pid={pid} window_id={wid}")
        }
        None => tracing::warn!(
            "could not resolve Mirroring window pid/window_id via {cua_path}; \
             key/text/shortcut will be skipped (pointer/tap still work)"
        ),
    }

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
        server::input_bridge::spawn_injector(geometry, cua_target, move || {
            let lease = current_lease.lock().unwrap();
            match &*lease {
                Some(l) => control.lock().unwrap().is_current(l),
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
    });

    run_server(cfg, state)
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
    eprintln!(" iphone-remote serving");
    eprintln!("   url:      {url}");
    match &cfg.password {
        Some(_) => eprintln!("   password: (set via PHONE_REMOTE_PASSWORD)"),
        None => eprintln!("   password: (none — open LAN mode)"),
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
    anyhow::bail!("missing TCC permissions; grant them and re-run `iphone-remote serve`")
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
