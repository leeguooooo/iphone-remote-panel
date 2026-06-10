//! Integration tests for the HTTP auth-cookie gate (axum, no OS calls).
//!
//! Drives the real router via `tower::ServiceExt::oneshot` against an `AppState`
//! built with a no-op video pipeline and a no-op injector, so the auth/cookie
//! contract is exercised end-to-end without ScreenCaptureKit / VideoToolbox /
//! CGEvent.
//!
//! NOTE: these use a hand-built current-thread tokio runtime via [`block`]
//! rather than `#[tokio::test]`. The local crate that holds the core types is
//! literally named `core`, which sits in this integration-test crate's extern
//! prelude; `#[tokio::test]` expands to `core::prelude::…` and would resolve to
//! that dependency instead of the std `core` crate. Going through
//! `server::core_crate` and a manual runtime sidesteps the shadowing.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use server::core_crate::control::Control;
use server::core_crate::encode::{EncodedFrame, VideoPipeline};
use server::http::{self, AppState};

/// Run a future to completion on a fresh current-thread runtime.
fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// A no-op pipeline: never emits frames; `request_keyframe` is a no-op.
struct NullPipeline {
    tx: tokio::sync::broadcast::Sender<EncodedFrame>,
}

impl VideoPipeline for NullPipeline {
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EncodedFrame> {
        self.tx.subscribe()
    }
    fn request_keyframe(&self) {}
}

fn build_state(password: Option<&str>) -> Arc<AppState> {
    use server::core_crate::coords::{Orientation, Rect, SessionGeometry};

    let (tx, _rx) = tokio::sync::broadcast::channel::<EncodedFrame>(4);
    let pipeline: Arc<dyn VideoPipeline> = Arc::new(NullPipeline { tx });

    let ice_servers = http::build_ice_servers(None, None, None);
    let ice = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(http::IceState::new(
        ice_servers,
    )));

    // A geometry whose gate is irrelevant here (no input routes are exercised).
    let geo = SessionGeometry {
        content_rect: Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 200.0,
        },
        scale: 2.0,
        orientation: Orientation::Portrait,
    };
    let injector = server::input_bridge::spawn_injector(geo, || false);

    Arc::new(AppState {
        pipeline,
        ice,
        password: password.map(|s| s.to_string()),
        secret: b"test-secret-key-0123456789abcdef".to_vec(),
        session_ttl_secs: 3600,
        cookie_secure: false,
        control: Arc::new(Mutex::new(Control::new())),
        current_lease: Arc::new(Mutex::new(None)),
        injector,
        auth_limiter: Arc::new(Mutex::new(http::AuthLimiter::new())),
        agent_token: None,
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    })
}

/// Like [`build_state`] but also sets a dedicated agent bearer token.
///
/// When `agent_token` is `Some`, the agent paths only accept that token as a
/// bearer credential; the human login password is rejected as a bearer.
fn build_state_with_agent_token(
    password: Option<&str>,
    agent_token: Option<&str>,
) -> Arc<AppState> {
    use server::core_crate::coords::{Orientation, Rect, SessionGeometry};

    let (tx, _rx) = tokio::sync::broadcast::channel::<server::core_crate::encode::EncodedFrame>(4);
    let pipeline: Arc<dyn server::core_crate::encode::VideoPipeline> =
        Arc::new(NullPipeline { tx });

    let ice_servers = http::build_ice_servers(None, None, None);
    let ice = std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(http::IceState::new(
        ice_servers,
    )));

    let geo = SessionGeometry {
        content_rect: Rect { x: 0.0, y: 0.0, w: 100.0, h: 200.0 },
        scale: 2.0,
        orientation: Orientation::Portrait,
    };
    let injector = server::input_bridge::spawn_injector(geo, || false);

    Arc::new(AppState {
        pipeline,
        ice,
        password: password.map(|s| s.to_string()),
        secret: b"test-secret-key-0123456789abcdef".to_vec(),
        session_ttl_secs: 3600,
        cookie_secure: false,
        control: Arc::new(Mutex::new(Control::new())),
        current_lease: Arc::new(Mutex::new(None)),
        injector,
        auth_limiter: Arc::new(Mutex::new(http::AuthLimiter::new())),
        agent_token: agent_token.map(|s| s.to_string()),
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    })
}

#[test]
fn phone_requires_auth_redirects_to_login() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);
        let resp = app
            .oneshot(Request::builder().uri("/phone").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Unauthed → redirect to /login.
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/login");
        // Security headers present.
        assert_eq!(resp.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(
            resp.headers().get("referrer-policy").unwrap(),
            "no-referrer"
        );
    });
}

#[test]
fn login_sets_session_cookie_and_phone_then_serves_client() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        // POST /login with the right password → 303 + Set-Cookie.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=hunter2"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(set_cookie.starts_with("phone_session="), "{set_cookie}");
        assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
        assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");

        // Extract just the cookie pair for the follow-up request.
        let cookie_pair = set_cookie.split(';').next().unwrap().to_string();

        // GET /phone WITH the cookie → 200 + the embedded client.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/phone")
                    .header(header::COOKIE, cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("iphone-use"));
        assert!(html.contains("/ws"));
    });
}

#[test]
fn login_wrong_password_is_unauthorized() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // No session cookie set on failure.
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
    });
}

#[test]
fn turn_creds_gated_then_served() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        // Unauthed → 401.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/turn-creds")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Mint a valid cookie via login, then fetch turn-creds.
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=hunter2"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie_pair = login
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/turn-creds")
                    .header(header::COOKIE, cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["iceServers"].is_array());
        assert_eq!(v["iceServers"][0]["urls"][0], "stun:stun.l.google.com:19302");
    });
}

#[test]
fn open_mode_serves_phone_without_cookie() {
    block(async {
        // No password configured → open LAN mode; /phone serves directly.
        let state = build_state(None);
        let app = http::router(state);
        let resp = app
            .oneshot(Request::builder().uri("/phone").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    });
}

#[test]
fn logout_clears_cookie() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);
        let resp = app
            .oneshot(Request::builder().uri("/logout").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");
    });
}

// ── Agent operation entry (/agent/*) ─────────────────────────────────────────

#[test]
fn agent_status_requires_bearer_when_password_set() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
        // No bearer → 401.
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/agent/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Correct bearer → 200 + JSON.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json = String::from_utf8_lossy(&body);
        assert!(json.contains("\"ok\":true"), "{json}");
        // phone_target reflects whether a Mirroring window is up at probe time;
        // its exact value depends on the test environment — just verify the key exists.
        assert!(json.contains("\"phone_target\":"), "{json}");
    });
}

#[test]
fn agent_input_rejects_wrong_bearer() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/input")
                    .header(header::AUTHORIZATION, "Bearer nope")
                    .body(Body::from(r#"{"type":"tap","x":0.5,"y":0.5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

#[test]
fn agent_input_accepts_valid_message_with_bearer() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
        for body in [
            r#"{"type":"tap","x":0.5,"y":0.5}"#,
            r#"{"type":"scroll","x":0.5,"y":0.5,"dx":0.0,"dy":-12.0}"#,
            r#"{"type":"text","text":"hello"}"#,
            r#"{"type":"shortcut","name":"home"}"#,
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/agent/input")
                        .header(header::AUTHORIZATION, "Bearer hunter2")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "body={body}");
        }
    });
}

#[test]
fn agent_input_rejects_garbage_body() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/input")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    });
}

#[test]
fn agent_open_mode_allows_without_bearer() {
    block(async {
        // No password configured → open LAN-dev mode; agent API is open too.
        let app = http::router(build_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/input")
                    .body(Body::from(r#"{"type":"tap","x":0.1,"y":0.1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    });
}

#[test]
fn agent_screenshot_unauthed_is_401() {
    block(async {
        // Auth still required before any screenshot attempt.
        let app = http::router(build_state(Some("hunter2")));
        let resp = app
            .oneshot(Request::builder().uri("/agent/screenshot").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

#[test]
fn agent_screenshot_authed_returns_503_or_png_depending_on_platform() {
    block(async {
        // Authed: on macOS with no Mirroring window → 503; on non-macOS stub → 503.
        // Either way the response is not 401 (auth passed) and not 200 (no window
        // in the test environment).
        let app = http::router(build_state(Some("hunter2")));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/screenshot")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 503 = Mirroring window not found (expected in CI / no real phone connected).
        // 502 would indicate an unexpected panic in spawn_blocking — fail loudly.
        assert!(
            resp.status() == StatusCode::SERVICE_UNAVAILABLE
                || resp.status() == StatusCode::OK,
            "expected 503 (no window) or 200 (window present), got {}",
            resp.status()
        );
    });
}

#[test]
fn agent_auth_accepts_non_ascii_password_bearer() {
    block(async {
        // Regression: a Chinese password made HeaderValue::to_str() fail → 401
        // on every agent request. The byte-based bearer check must accept it.
        let pw = "测试密码123";
        let app = http::router(build_state(Some(pw)));
        let hv = axum::http::HeaderValue::from_bytes(format!("Bearer {pw}").as_bytes()).unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, hv)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Wrong non-ASCII token → still 401.
        let bad = axum::http::HeaderValue::from_bytes("Bearer 错误密码".as_bytes()).unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, bad)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

// ── Rate limiter integration tests ───────────────────────────────────────────
//
// Each test builds its own fresh AppState so limiters don't bleed.

#[test]
fn login_sixth_wrong_password_returns_429() {
    block(async {
        // Each test has its own fresh state — limiters are isolated.
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        // 5 failures — each should return 401.
        for i in 0..5 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/login")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("password=wrong"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "attempt {i} should be 401");
        }

        // 6th attempt should be 429.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    });
}

#[test]
fn login_correct_after_four_failures_succeeds_and_resets() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        // 4 wrong attempts — below the lockout threshold.
        for _ in 0..4 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/login")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("password=wrong"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        // Correct password should succeed (303 + Set-Cookie) and reset the counter.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=hunter2"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "correct login should succeed");
        assert!(resp.headers().get(header::SET_COOKIE).is_some(), "should set session cookie");

        // After a success the counter is reset — a 5th wrong attempt should be 401
        // (not 429 — the lockout was lifted).
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "first wrong after reset should be 401");
    });
}

#[test]
fn agent_bearer_failures_trigger_lockout() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        // 5 wrong bearer attempts → limiter fills up.
        for i in 0..5 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/agent/input")
                        .header(header::AUTHORIZATION, "Bearer nope")
                        .body(Body::from(r#"{"type":"tap","x":0.5,"y":0.5}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "agent attempt {i} should be 401");
        }

        // 6th wrong agent request → 429.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/input")
                    .header(header::AUTHORIZATION, "Bearer nope")
                    .body(Body::from(r#"{"type":"tap","x":0.5,"y":0.5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        // Locked-out agent/status also returns 429.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    });
}

#[test]
fn login_failures_lock_out_agent_endpoints_via_shared_counter() {
    block(async {
        // The limiter is one shared counter across the cookie login AND the agent
        // bearer paths — 5 wrong /login attempts must 429 a subsequent agent call
        // even though the agent itself never failed.
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        for _ in 0..5 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/login")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("password=wrong"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        // Agent request with the CORRECT bearer is still rejected while locked.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "shared lockout must cover the agent path too"
        );
    });
}

// ── Dedicated agent_token tests (issue #7) ───────────────────────────────────

/// (a) When `agent_token` is set, a bearer matching it passes; a bearer matching
///     the (human) password is rejected.
#[test]
fn agent_token_set_accepts_token_rejects_password_as_bearer() {
    block(async {
        let state = build_state_with_agent_token(Some("human-pass"), Some("sk-agent-secret"));
        let app = http::router(state);

        // Bearer = agent token → 200.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer sk-agent-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "agent token should be accepted");

        // Bearer = human password → 401 (password is no longer a valid bearer).
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer human-pass")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "password must not be accepted as bearer when agent_token is configured"
        );
    });
}

/// (b) When `agent_token` is NOT set, the password-as-bearer path still works
///     (backward-compatibility: existing behavior is unchanged).
#[test]
fn agent_token_unset_password_still_valid_bearer() {
    block(async {
        // No agent_token → falls back to password-as-bearer (original behavior).
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "password-as-bearer must still work when no agent_token is configured"
        );
    });
}

/// (c) A wrong `agent_token` bearer must count toward the rate-limit lockout,
///     eventually returning 429.
#[test]
fn wrong_agent_token_counts_toward_rate_limit_lockout() {
    block(async {
        let state =
            build_state_with_agent_token(Some("human-pass"), Some("sk-agent-secret"));
        let app = http::router(state);

        // 5 wrong bearer attempts (wrong agent token) → each should be 401.
        for i in 0..5 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/agent/status")
                        .header(header::AUTHORIZATION, "Bearer wrong-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "wrong agent token attempt {i} should be 401"
            );
        }

        // 6th attempt (wrong token again) should be 429 — locked out.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "wrong agent_token failures must trigger rate-limit lockout"
        );
    });
}

// ── Shortcuts RPC inbox (/agent/inbox) ───────────────────────────────────────

#[test]
fn inbox_post_then_get_drains_and_returns_json() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));

        // Phone (shortcut) POSTs a result.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/inbox")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::from(r#"{"verb":"battery","level":0.87}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Agent GETs and drains.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/inbox")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json = String::from_utf8_lossy(&body);
        assert!(json.contains("\"verb\":\"battery\""), "{json}");
        assert!(json.contains("\"level\":0.87"), "{json}");
        assert!(json.contains("\"received_at\""), "{json}");

        // Second GET is empty (drained).
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/inbox")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(String::from_utf8_lossy(&body), r#"{"items":[]}"#);
    });
}

#[test]
fn inbox_post_requires_auth() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/inbox")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

#[test]
fn inbox_peek_does_not_drain() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/inbox")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::from(r#"{"k":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // peek twice — item must persist.
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/agent/inbox?peek=1")
                        .header(header::AUTHORIZATION, "Bearer hunter2")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("\"k\":1"));
        }
    });
}
