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
    let turn_creds_json = http::ice_servers_json(&ice_servers);

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
        ice_servers,
        turn_creds_json,
        password: password.map(|s| s.to_string()),
        secret: b"test-secret-key-0123456789abcdef".to_vec(),
        session_ttl_secs: 3600,
        cookie_secure: false,
        control: Arc::new(Mutex::new(Control::new())),
        current_lease: Arc::new(Mutex::new(None)),
        injector,
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
        assert!(html.contains("iPhone Remote"));
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
