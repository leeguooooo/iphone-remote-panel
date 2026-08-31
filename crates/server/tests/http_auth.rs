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

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

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
        backend: server::config::DeviceBackend::Direct,
        pipeline,
        ice,
        password: password.map(|s| s.to_string()),
        secret: b"test-secret-key-0123456789abcdef".to_vec(),
        session_ttl_secs: 3600,
        cookie_secure: false,
        lease_state: Arc::new(Mutex::new(http::LeaseState::new())),
        injector,
        auth_limiter: Arc::new(Mutex::new(http::AuthLimiter::new())),
        agent_token: None,
        device_udid: None,
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        wda: None,
        managed_wda: false,
        managed_wda_pending: false,
        latest_release: std::sync::Arc::new(std::sync::Mutex::new(None)),
        viewers: std::sync::Arc::new(std::sync::Mutex::new(
            server::signaling::ViewerRegistry::default(),
        )),
        mirror_paused_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
        mjpeg_url: None,
        wda_actionable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wda_health: std::sync::Arc::new(std::sync::Mutex::new(server::wda::WdaHealth::down())),
        wda_death: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        wda_health_probe: std::sync::Arc::new(std::sync::Mutex::new(None)),
        wda_control_pending: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_activity: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
        released: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wda_lifecycle: std::sync::Arc::new(http::WdaLifecycle::new()),
        live_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        mjpeg_stream_activity: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        element_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::new(),
        )),
    })
}

fn build_mirror_state(password: Option<&str>) -> Arc<AppState> {
    let state = build_state(password);
    let mut state = match Arc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => panic!("test state unexpectedly shared"),
    };
    state.backend = server::config::DeviceBackend::Mirror;
    Arc::new(state)
}

fn build_state_with_wda(base_url: &str) -> Arc<AppState> {
    let state = build_state(None);
    let mut state = match Arc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => panic!("test state unexpectedly shared"),
    };
    state.wda = Some(Arc::new(tokio::sync::Mutex::new(
        server::wda::WdaClient::new(base_url).unwrap(),
    )));
    Arc::new(state)
}

/// Deliberately violate the production constructor's backend invariant.
///
/// Boundary tests use this state to prove that Mirror handlers ignore WDA even
/// if a future refactor accidentally leaves a client and MJPEG URL attached.
fn build_invalid_mirror_state_with_wda(base_url: &str) -> Arc<AppState> {
    let state = build_state_with_wda(base_url);
    let mut state = match Arc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => panic!("test state unexpectedly shared"),
    };
    state.backend = server::config::DeviceBackend::Mirror;
    state.mjpeg_url = Some(base_url.to_string());
    state.wda_actionable.store(true, Ordering::Release);
    *state.wda_health.lock().unwrap() = server::wda::WdaHealth {
        up: true,
        actionable: true,
        locked: Some(false),
    };
    Arc::new(state)
}

fn assert_no_wda_connection(listener: &TcpListener) {
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("Mirror backend unexpectedly connected to WDA"),
        Err(error) => panic!("could not inspect mock WDA listener: {error}"),
    }
}

fn mock_wda(
    requests: usize,
    responder: impl Fn(&str, usize) -> Option<(std::time::Duration, String)> + Send + 'static,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let task = std::thread::spawn(move || {
        for index in 0..requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 8_192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let Some((delay, body)) = responder(&request, index) else {
                continue; // close without a response: ambiguous transport loss
            };
            std::thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}"), task)
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
        backend: server::config::DeviceBackend::Direct,
        pipeline,
        ice,
        password: password.map(|s| s.to_string()),
        secret: b"test-secret-key-0123456789abcdef".to_vec(),
        session_ttl_secs: 3600,
        cookie_secure: false,
        lease_state: Arc::new(Mutex::new(http::LeaseState::new())),
        injector,
        auth_limiter: Arc::new(Mutex::new(http::AuthLimiter::new())),
        agent_token: agent_token.map(|s| s.to_string()),
        device_udid: None,
        inbox: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        wda: None,
        managed_wda: false,
        managed_wda_pending: false,
        latest_release: std::sync::Arc::new(std::sync::Mutex::new(None)),
        viewers: std::sync::Arc::new(std::sync::Mutex::new(
            server::signaling::ViewerRegistry::default(),
        )),
        mirror_paused_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
        mjpeg_url: None,
        wda_actionable: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wda_health: std::sync::Arc::new(std::sync::Mutex::new(server::wda::WdaHealth::down())),
        wda_death: std::sync::Arc::new(std::sync::Mutex::new(Default::default())),
        wda_health_probe: std::sync::Arc::new(std::sync::Mutex::new(None)),
        wda_control_pending: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        last_activity: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
        released: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        wda_lifecycle: std::sync::Arc::new(http::WdaLifecycle::new()),
        live_streams: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        mjpeg_stream_activity: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        element_snapshots: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::new(),
        )),
    })
}

#[test]
fn phone_requires_auth_redirects_to_login() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/phone")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Unauthed → redirect to /login.
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/login");
        // Security headers present.
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(
            resp.headers().get("referrer-policy").unwrap(),
            "no-referrer"
        );
    });
}

#[test]
fn setup_guide_uses_the_same_session_gate_as_phone() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/login?next=%2Fsetup"
        );

        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("password=hunter2&next=%2Fsetup"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        assert_eq!(login.headers().get(header::LOCATION).unwrap(), "/setup");
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
                    .uri("/setup")
                    .header(header::COOKIE, cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("连接真实 iPhone"));
        assert!(html.contains("/agent/status"));
        assert!(!html.contains("/agent/mode"));
    });
}

#[test]
fn login_form_names_the_password_and_error_relationship() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains(r#"<label for="password">控制密码</label>"#));
        assert!(html.contains(r#"id="passwordHint""#));
        assert!(html.contains(r#"aria-describedby="passwordHint loginError""#));
        assert!(html.contains(r#"aria-invalid="false""#));
        assert!(html.contains(r#"role="alert""#));
        assert!(html.contains(r#"<form method="POST" action="/login" novalidate>"#));
        assert!(html.contains(".err:empty{display:none}"));
        assert!(html.contains("忘记后，请回到 Mac 重新运行安装程序查看或重设"));
        assert!(html.contains("button:focus-visible"));
        assert!(!html.contains(r#"name="next""#));
        assert!(!html.contains("__ERR__"));
        assert!(!html.contains("__INVALID__"));
        assert!(!html.contains("__NEXT_INPUT__"));
    });
}

#[test]
fn login_preserves_only_the_allowlisted_setup_destination() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login?next=%2Fsetup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains(r#"<input type="hidden" name="next" value="/setup">"#));

        for (password, expected_status, expected_error) in [
            ("", StatusCode::BAD_REQUEST, "请输入控制密码"),
            (
                "wrong",
                StatusCode::UNAUTHORIZED,
                "密码错误，请检查安装时保存的控制密码",
            ),
        ] {
            let body = format!("password={password}&next=%2Fsetup");
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/login")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), expected_status);
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let html = String::from_utf8_lossy(&body);
            assert!(html.contains(expected_error));
            assert!(html.contains(r#"<input type="hidden" name="next" value="/setup">"#));
        }

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login?next=https%3A%2F%2Fevil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(!html.contains(r#"name="next""#));

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "password=hunter2&next=https%3A%2F%2Fevil.example",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/phone");
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
fn login_empty_password_returns_inline_error_without_consuming_attempts() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        for _ in 0..6 {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/login")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("password="))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert!(resp.headers().get(header::SET_COOKIE).is_none());
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let html = String::from_utf8_lossy(&body);
            assert!(html.contains("请输入控制密码"));
            assert!(html.contains(r#"aria-invalid="true""#));
        }

        // Empty submissions are form-validation mistakes, not credential
        // attempts, so the first actual wrong password must still be a 401.
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
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("密码错误，请检查安装时保存的控制密码"));
        assert!(html.contains(r#"aria-invalid="true""#));
        assert!(html.contains(r#"role="alert""#));
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
        assert_eq!(
            v["iceServers"][0]["urls"][0],
            "stun:stun.l.google.com:19302"
        );
    });
}

#[test]
fn open_mode_serves_phone_without_cookie() {
    block(async {
        // No password configured → open LAN mode; /phone serves directly.
        let state = build_state(None);
        let app = http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/phone")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    });
}

#[test]
fn open_mode_serves_setup_guide_without_cookie() {
    block(async {
        let app = http::router(build_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/setup")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("连接真实 iPhone"));
    });
}

#[test]
fn logout_clears_cookie() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/logout")
                    .body(Body::empty())
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
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
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
        assert!(json.contains(r#""backend":"direct""#), "{json}");
        assert!(json.contains(r#""managed_wda":false"#), "{json}");
        assert!(json.contains(r#""recovery_owner":"external""#), "{json}");
        assert!(json.contains(r#""reconnecting":false"#), "{json}");
        assert!(json.contains(r#""phone_target":false"#), "{json}");
        assert!(json.contains(r#""mirror_state":"disabled""#), "{json}");
    });
}

#[test]
fn agent_status_serializes_release_tags_as_json_data() {
    block(async {
        let state = build_state(None);
        *state.latest_release.lock().unwrap() = Some("v1\"</script>".to_string());
        let app = http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["latest"], "v1\"</script>");
        assert_eq!(json["update_available"], true);
    });
}

#[test]
fn agent_status_reports_freshness_only_for_the_requested_mjpeg_stream() {
    block(async {
        let stream_id = "browser_01234567";
        let state = build_state_with_wda("http://127.0.0.1:9");
        state.wda_actionable.store(true, Ordering::Release);
        *state.wda_health.lock().unwrap() = server::wda::WdaHealth {
            up: true,
            actionable: true,
            locked: Some(false),
        };
        state
            .mjpeg_stream_activity
            .lock()
            .unwrap()
            .insert(stream_id.to_string(), (1, std::time::Instant::now()));
        let app = http::router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/agent/status?stream_id={stream_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["mjpeg_stream_fresh"], true);
        assert!(status["mjpeg_stream_age_ms"].as_u64().is_some(), "{status}");
        assert_eq!(status["screen_state"], "live");

        let invalid = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status?stream_id=contains%2Fslash")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    });
}

#[test]
fn token_only_config_keeps_the_open_browser_ui_usable() {
    block(async {
        let app = http::router(build_state_with_agent_token(None, Some("agent-secret")));

        let status = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);

        let screenshot = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/screenshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(screenshot.status(), StatusCode::SERVICE_UNAVAILABLE);

        let mjpeg = app
            .oneshot(
                Request::builder()
                    .uri("/agent/mjpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mjpeg.status(), StatusCode::SERVICE_UNAVAILABLE);
    });
}

#[test]
fn stale_browser_polls_do_not_lock_out_a_valid_bearer() {
    block(async {
        let app = http::router(build_state_with_agent_token(
            Some("browser-password"),
            Some("agent-secret"),
        ));

        for _ in 0..8 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/agent/status")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .header(header::AUTHORIZATION, "Bearer agent-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    });
}

#[test]
fn direct_browser_control_requires_cookie_and_custom_header() {
    block(async {
        let state = build_state(Some("hunter2"));
        let app = http::router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .body(Body::from(r#"{"type":"tap","x":0.5,"y":0.5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

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
        let cookie = login
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
                    .method("POST")
                    .uri("/control")
                    .header(header::COOKIE, cookie)
                    .body(Body::from(r#"{"type":"tap","x":0.5,"y":0.5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    });
}

#[test]
fn direct_browser_control_fails_closed_when_wda_is_unavailable() {
    block(async {
        let app = http::router(build_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"type":"tap","x":0.5,"y":0.5,"ttl_ms":2000}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("wda_not_configured"));
    });
}

#[test]
fn direct_backend_rejects_runtime_switch_to_mirroring() {
    block(async {
        let app = http::router(build_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/mode")
                    .header("x-phone-control", "1")
                    .body(Body::from(r#"{"mode":"mirror"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    });
}

#[test]
fn mirror_backend_rejects_runtime_switch_to_agent() {
    block(async {
        let app = http::router(build_mirror_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/mode")
                    .header("x-phone-control", "1")
                    .body(Body::from(r#"{"mode":"agent"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("backend_is_mirror"));
    });
}

#[test]
fn mirror_backend_ignores_wda_even_when_state_is_misconfigured() {
    block(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let app = http::router(build_invalid_mirror_state_with_wda(&base_url));

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.clone().oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("Mirror status must not wait on WDA")
        .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let body = status.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["backend"], "mirror");
        assert_eq!(json["wda"], false);
        assert_eq!(json["wda_actionable"], false);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_no_wda_connection(&listener);

        let elements = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/elements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(elements.status(), StatusCode::CONFLICT);
        let body = elements.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "backend_is_mirror");
        assert_no_wda_connection(&listener);

        let mjpeg = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/mjpeg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mjpeg.status(), StatusCode::CONFLICT);
        assert_no_wda_connection(&listener);

        let screenshot = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.oneshot(
                Request::builder()
                    .uri("/agent/screenshot")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("Mirror screenshot must not wait on WDA")
        .unwrap();
        assert!(
            screenshot.status() == StatusCode::OK
                || screenshot.status() == StatusCode::SERVICE_UNAVAILABLE,
            "unexpected Mirror screenshot status: {}",
            screenshot.status()
        );
        assert_no_wda_connection(&listener);
    });
}

#[test]
fn pending_managed_direct_backend_requires_a_persisted_target() {
    block(async {
        let state = build_state(None);
        let mut state = match Arc::try_unwrap(state) {
            Ok(state) => state,
            Err(_) => panic!("test state unexpectedly shared"),
        };
        state.managed_wda_pending = true;
        let app = http::router(Arc::new(state));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/mode")
                    .header("x-phone-control", "1")
                    .body(Body::from(
                        r#"{"mode":"agent","udid":"00008110-001234567890001E"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("target_not_configured"));
    });
}

#[test]
fn external_direct_backend_without_local_target_stays_externally_owned() {
    block(async {
        let app = http::router(build_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/mode")
                    .header("x-phone-control", "1")
                    .body(Body::from(r#"{"mode":"agent"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("wda_is_externally_managed"));
    });
}

#[test]
fn pending_managed_target_is_reported_as_unconfigured() {
    block(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = build_state_with_wda(&base_url);
        let mut state = match Arc::try_unwrap(state) {
            Ok(state) => state,
            Err(_) => panic!("test state unexpectedly shared"),
        };
        state.managed_wda_pending = true;
        state.mjpeg_url = Some(base_url);
        state.wda_actionable.store(true, Ordering::Release);
        *state.wda_health.lock().unwrap() = server::wda::WdaHealth {
            up: true,
            actionable: true,
            locked: Some(false),
        };
        let app = http::router(Arc::new(state));
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["managed_wda"], false);
        assert_eq!(status["managed_wda_pending"], true);
        assert_eq!(status["target_configured"], false);
        assert_eq!(status["recovery_owner"], "unconfigured");
        assert_eq!(status["wda"], false);
        assert_eq!(status["wda_actionable"], false);
        assert!(status["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("setup-wda.sh")));

        for uri in ["/agent/elements", "/agent/screenshot", "/agent/mjpeg"] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CONFLICT, "uri={uri}");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert!(
                String::from_utf8_lossy(&body).contains("target_not_configured"),
                "uri={uri}, body={}",
                String::from_utf8_lossy(&body)
            );
        }

        for (uri, body) in [
            (
                "/control",
                r#"{"type":"tap","x":0.5,"y":0.5,"ttl_ms":1000}"#,
            ),
            ("/agent/input", r#"{"type":"tap","x":0.5,"y":0.5}"#),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("x-phone-control", "1")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CONFLICT, "uri={uri}");
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert!(
                String::from_utf8_lossy(&body).contains("target_not_configured"),
                "uri={uri}, body={}",
                String::from_utf8_lossy(&body)
            );
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_no_wda_connection(&listener);
    });
}

#[test]
fn direct_browser_control_requires_a_short_server_deadline() {
    block(async {
        let app = http::router(build_state(None));
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .body(Body::from(r#"{"type":"tap","x":0.5,"y":0.5}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let too_long = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .body(Body::from(
                        r#"{"type":"tap","x":0.5,"y":0.5,"ttl_ms":10000}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(too_long.status(), StatusCode::BAD_REQUEST);
    });
}

#[test]
fn direct_browser_control_timeout_before_dispatch_is_retry_safe_408() {
    block(async {
        let state = build_state_with_wda("http://127.0.0.1:9");
        let wda = state.wda.as_ref().unwrap().clone();
        let held = wda.lock().await;
        let app = http::router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"tap","x":0.5,"y":0.5,"ttl_ms":30}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        drop(held);

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "ok": false,
                "error": "not_sent",
                "outcome": "not_sent",
                "retry_safe": true
            })
        );
    });
}

#[test]
fn direct_browser_control_timeout_after_dispatch_is_unknown_504() {
    block(async {
        let action_count = Arc::new(AtomicUsize::new(0));
        let observed = action_count.clone();
        let (base, server) = mock_wda(3, move |request, _| {
            if request.starts_with("POST /session ") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ))
            } else if request.contains("/window/size") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"width":390,"height":844}}"#.to_string(),
                ))
            } else {
                observed.fetch_add(1, Ordering::SeqCst);
                Some((
                    std::time::Duration::from_millis(400),
                    r#"{"value":null}"#.to_string(),
                ))
            }
        });
        let app = http::router(build_state_with_wda(&base));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"tap","x":1.0,"y":1.0,"ttl_ms":150}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "ok": false,
                "error": "outcome_unknown",
                "outcome": "unknown",
                "retry_safe": false
            })
        );
        assert_eq!(action_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn direct_browser_control_can_tap_an_accessibility_label() {
    block(async {
        let click_count = Arc::new(AtomicUsize::new(0));
        let observed = click_count.clone();
        let (base, server) = mock_wda(3, move |request, _| {
            if request.starts_with("POST /session ") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ))
            } else if request.contains("/source?format=json") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"type":"XCUIElementTypeApplication","children":[{"type":"XCUIElementTypeButton","label":"完成","rect":{"x":300,"y":700,"width":60,"height":44}}]}}"#.to_string(),
                ))
            } else if request.contains("/actions") {
                observed.fetch_add(1, Ordering::SeqCst);
                Some((std::time::Duration::ZERO, r#"{"value":null}"#.to_string()))
            } else {
                panic!("unexpected WDA request: {request}");
            }
        });
        let app = http::router(build_state_with_wda(&base));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"tap","label":"完成","ttl_ms":2000}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(click_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn direct_browser_label_tap_rejects_ambiguous_matches_without_tapping() {
    block(async {
        let (base, server) = mock_wda(1, move |request, _| {
            assert!(
                request.contains("/source?format=json"),
                "ambiguous label resolution must stop before an action: {request}"
            );
            Some((
                std::time::Duration::ZERO,
                r#"{"value":{"type":"XCUIElementTypeApplication","children":[{"type":"XCUIElementTypeButton","label":"关注","rawIdentifier":"author-a","rect":{"x":300,"y":200,"width":60,"height":44}},{"type":"XCUIElementTypeButton","label":"关注","rawIdentifier":"author-b","rect":{"x":300,"y":400,"width":60,"height":44}}]}}"#.to_string(),
            ))
        });
        let app = http::router(build_state_with_wda(&base));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"tap","label":"关注","ttl_ms":2000}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "ambiguous_element_label");
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(json["retry_safe"], true);
    });
}

#[test]
fn direct_browser_label_tap_rejects_missing_match_without_tapping() {
    block(async {
        let (base, server) = mock_wda(1, move |request, _| {
            assert!(
                request.contains("/source?format=json"),
                "missing label resolution must stop before an action: {request}"
            );
            Some((
                std::time::Duration::ZERO,
                r#"{"value":{"type":"XCUIElementTypeApplication","children":[{"type":"XCUIElementTypeButton","label":"取消","rect":{"x":20,"y":700,"width":60,"height":44}}]}}"#.to_string(),
            ))
        });
        let app = http::router(build_state_with_wda(&base));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"tap","label":"完成","ttl_ms":2000}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "element_not_found");
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(json["retry_safe"], true);
    });
}

#[test]
fn indexed_browser_tap_requires_an_element_snapshot() {
    block(async {
        let app = http::router(build_state_with_wda("http://127.0.0.1:9"));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"tap","element":0,"ttl_ms":2000}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid_element_snapshot");
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(json["retry_safe"], true);
    });
}

#[test]
fn indexed_browser_tap_applies_only_when_the_snapshot_still_matches() {
    block(async {
        let action_count = Arc::new(AtomicUsize::new(0));
        let observed = action_count.clone();
        let source = r#"{"value":{"type":"XCUIElementTypeApplication","children":[{"type":"XCUIElementTypeButton","label":"更多","rect":{"x":20,"y":40,"width":80,"height":44},"children":[]},{"type":"XCUIElementTypeButton","label":"更多","rect":{"x":20,"y":100,"width":80,"height":44},"children":[]}]}}"#;
        let source = source.replacen("\"label\":\"更多\"", "\"label\":\"菜单\"", 1);
        let (base, server) = mock_wda(6, move |request, _| {
            if request.starts_with("POST /session ") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ))
            } else if request.contains("/source?format=json") {
                Some((std::time::Duration::ZERO, source.to_string()))
            } else if request.contains("/window/size") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"width":390,"height":844}}"#.to_string(),
                ))
            } else if request.contains("/elements") {
                assert!(
                    request.contains("label == '更多'"),
                    "snapshot row must be re-resolved semantically: {request}"
                );
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":[{"ELEMENT":"MORE"}]}"#.to_string(),
                ))
            } else {
                assert!(
                    request.contains("/element/MORE/click"),
                    "unexpected WDA request: {request}"
                );
                observed.fetch_add(1, Ordering::SeqCst);
                Some((std::time::Duration::ZERO, r#"{"value":null}"#.to_string()))
            }
        });
        let app = http::router(build_state_with_wda(&base));

        let elements_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/elements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(elements_response.status(), StatusCode::OK);
        let elements_body = elements_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let elements: serde_json::Value = serde_json::from_slice(&elements_body).unwrap();
        let snapshot = elements["snapshot"].as_str().unwrap();
        assert!(!snapshot.is_empty());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "type": "tap",
                            "element": 1,
                            "snapshot": snapshot,
                            "ttl_ms": 2000
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        let status = response.status();
        let response_body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::OK,
            "unexpected response: {}",
            String::from_utf8_lossy(&response_body)
        );
        assert_eq!(action_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn indexed_browser_tap_rejects_a_stale_snapshot_without_tapping() {
    block(async {
        let source_count = Arc::new(AtomicUsize::new(0));
        let observed_sources = source_count.clone();
        let action_count = Arc::new(AtomicUsize::new(0));
        let observed_actions = action_count.clone();
        let (base, server) = mock_wda(4, move |request, _| {
            if request.starts_with("POST /session ") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ))
            } else if request.contains("/source?format=json") {
                let source_index = observed_sources.fetch_add(1, Ordering::SeqCst);
                let y = if source_index == 0 { 40 } else { 240 };
                Some((
                    std::time::Duration::ZERO,
                    format!(
                        r#"{{"value":{{"type":"XCUIElementTypeApplication","children":[{{"type":"XCUIElementTypeButton","label":"更多","rect":{{"x":20,"y":{y},"width":80,"height":44}},"children":[]}}]}}}}"#
                    ),
                ))
            } else if request.contains("/window/size") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"width":390,"height":844}}"#.to_string(),
                ))
            } else {
                observed_actions.fetch_add(1, Ordering::SeqCst);
                panic!("stale snapshot must not send a WDA action: {request}");
            }
        });
        let app = http::router(build_state_with_wda(&base));

        let elements_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/agent/elements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elements_body = elements_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let elements: serde_json::Value = serde_json::from_slice(&elements_body).unwrap();
        let snapshot = elements["snapshot"].as_str().unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "type": "tap",
                            "element": 0,
                            "snapshot": snapshot,
                            "ttl_ms": 2000
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "stale_element_snapshot");
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(json["retry_safe"], true);
        assert_eq!(action_count.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn direct_backend_rejects_websocket_signaling() {
    block(async {
        let app = http::router(build_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ws")
                    .header(header::HOST, "phone.test")
                    .header(header::CONNECTION, "Upgrade")
                    .header(header::UPGRADE, "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    });
}

#[test]
fn mirror_backend_rejects_cross_origin_websocket() {
    block(async {
        let app = http::router(build_mirror_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ws")
                    .header(header::HOST, "phone.test")
                    .header(header::ORIGIN, "https://evil.test")
                    .header(header::CONNECTION, "Upgrade")
                    .header(header::UPGRADE, "websocket")
                    .header("sec-websocket-version", "13")
                    .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
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
fn all_mutating_api_posts_require_the_control_header() {
    block(async {
        let app = http::router(build_state(None));
        for (uri, body) in [
            (
                "/control",
                r#"{"type":"tap","x":0.5,"y":0.5,"ttl_ms":2000}"#,
            ),
            ("/agent/mode", r#"{"mode":"agent"}"#),
            ("/agent/input", r#"{"type":"home"}"#),
            (
                "/agent/actions",
                r#"{"steps":[{"kind":"action","action":{"type":"home"}}]}"#,
            ),
            ("/agent/inbox", r#"{"message":"hello"}"#),
            ("/agent/inbox/drain", ""),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "uri={uri}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], "missing_control_header", "uri={uri}");
            assert_eq!(json["required_header"], "X-Phone-Control: 1", "uri={uri}");
            assert!(
                json["hint"]
                    .as_str()
                    .is_some_and(|hint| hint.contains("X-Phone-Control: 1")),
                "uri={uri}"
            );
        }
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
                        .header("x-phone-control", "1")
                        .header(header::AUTHORIZATION, "Bearer hunter2")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            // Bearer accepted (not 401) and the message parsed (not 400). The
            // status may be 409 "dropped" — no iPhone Mirroring window in the
            // test env to deliver the L3 event to (issue #25). This test covers
            // auth + parsing, so assert neither rejected it.
            assert_ne!(resp.status(), StatusCode::UNAUTHORIZED, "body={body}");
            assert_ne!(resp.status(), StatusCode::BAD_REQUEST, "body={body}");
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
                    .header("x-phone-control", "1")
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
fn agent_actions_validates_the_entire_batch_before_touching_wda() {
    block(async {
        let app = http::router(build_state(None));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/actions")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"steps":[
                            {"kind":"action","action":{"type":"home"}},
                            {"kind":"action","action":{"type":"uninstall","bundle":"com.example.app"}}
                        ]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "invalid_actions_request");
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(json["retry_safe"], true);
        assert!(json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("cannot batch destructive uninstall")));
    });
}

#[test]
fn agent_actions_executes_a_guarded_sequence_under_one_request() {
    block(async {
        let (base, server) = mock_wda(3, |request, _| {
            let body = if request.starts_with("POST /session ") {
                r#"{"value":{"sessionId":"SESSION"}}"#
            } else if request.contains("/wda/pressButton") {
                r#"{"value":null}"#
            } else {
                assert!(
                    request.contains("/source?format=json"),
                    "unexpected WDA request: {request}"
                );
                r#"{"value":{
                    "type":"XCUIElementTypeApplication",
                    "label":"主屏幕",
                    "rect":{"x":0,"y":0,"width":390,"height":844},
                    "children":[{
                        "type":"XCUIElementTypeButton",
                        "label":"搜索",
                        "rect":{"x":160,"y":700,"width":70,"height":40},
                        "children":[]
                    }]
                }}"#
            };
            Some((std::time::Duration::ZERO, body.to_string()))
        });
        let app = http::router(build_state_with_wda(&base));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/actions")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"steps":[
                            {"kind":"action","action":{"type":"home"}},
                            {"kind":"wait_for","expect":{
                                "application":"主屏幕",
                                "present":[{"kind":"Button","label":"搜索"}]
                            },"timeout_ms":1000,"poll_ms":50}
                        ]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["completed"], 2);
        assert_eq!(json["applied_actions"], 1);
        assert_eq!(json["steps"][1]["kind"], "wait_for");
        assert_eq!(json["steps"][1]["attempts"], 1);
        assert_eq!(json["steps"][1]["observation"]["application"], "主屏幕");
    });
}

#[test]
fn agent_actions_taps_one_strict_semantic_locator() {
    block(async {
        let action_count = Arc::new(AtomicUsize::new(0));
        let observed_actions = action_count.clone();
        let (base, server) = mock_wda(4, move |request, _| {
            let body = if request.starts_with("POST /session ") {
                r#"{"value":{"sessionId":"SESSION"}}"#.to_string()
            } else if request.contains("/source?format=json") {
                r#"{"value":{
                    "type":"XCUIElementTypeApplication",
                    "label":"测试应用",
                    "children":[{
                        "type":"XCUIElementTypeButton",
                        "label":"搜索",
                        "rawIdentifier":"search-action",
                        "isEnabled":true,
                        "isVisible":true,
                        "rect":{"x":160,"y":700,"width":70,"height":40},
                        "children":[]
                    }]
                }}"#
                .to_string()
            } else if request.contains("/elements") {
                assert!(
                    request.contains("enabled == 1 AND visible == 1"),
                    "strict locator state must reach the native WDA query: {request}"
                );
                r#"{"value":[{"ELEMENT":"SEARCH"}]}"#.to_string()
            } else {
                assert!(
                    request.contains("/element/SEARCH/click"),
                    "unexpected WDA request: {request}"
                );
                observed_actions.fetch_add(1, Ordering::SeqCst);
                r#"{"value":null}"#.to_string()
            };
            Some((std::time::Duration::ZERO, body))
        });
        let app = http::router(build_state_with_wda(&base));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/actions")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"steps":[{
                            "kind":"action",
                            "action":{
                                "type":"tap_locator",
                                "locator":{
                                    "identifier":"search-action",
                                    "kind":"Button",
                                    "enabled":true,
                                    "visible":true
                                }
                            }
                        }]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["completed"], 1);
        assert_eq!(json["applied_actions"], 1);
        assert_eq!(action_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn agent_actions_wait_for_retries_one_stale_source_read() {
    block(async {
        let source_count = Arc::new(AtomicUsize::new(0));
        let observed_sources = source_count.clone();
        let (base, server) = mock_wda(2, move |request, _| {
            assert!(
                request.contains("/source?format=json"),
                "wait_for retries only its read-only source observation: {request}"
            );
            let body = if observed_sources.fetch_add(1, Ordering::SeqCst) == 0 {
                r#"{"value":{"error":"invalid session id","message":"stale source"}}"#.to_string()
            } else {
                r#"{"value":{
                        "type":"XCUIElementTypeApplication",
                        "label":"测试应用",
                        "children":[]
                    }}"#
                .to_string()
            };
            Some((std::time::Duration::ZERO, body))
        });
        let app = http::router(build_state_with_wda(&base));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/actions")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"steps":[{
                            "kind":"wait_for",
                            "expect":{"application":"测试应用"},
                            "timeout_ms":1000,
                            "poll_ms":50
                        }]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["completed"], 1);
        assert_eq!(json["steps"][0]["attempts"], 1);
        assert_eq!(source_count.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn agent_actions_stops_before_later_steps_when_an_element_is_missing() {
    block(async {
        let (base, server) = mock_wda(3, |request, _| {
            let body = if request.starts_with("POST /session ") {
                r#"{"value":{"sessionId":"SESSION"}}"#
            } else if request.contains("/wda/pressButton") {
                r#"{"value":null}"#
            } else {
                assert!(
                    request.contains("/source?format=json"),
                    "unexpected WDA request: {request}"
                );
                r#"{"value":{
                    "type":"XCUIElementTypeApplication",
                    "label":"测试应用",
                    "children":[]
                }}"#
            };
            Some((std::time::Duration::ZERO, body.to_string()))
        });
        let app = http::router(build_state_with_wda(&base));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/actions")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"steps":[
                            {"kind":"action","action":{"type":"home"}},
                            {"kind":"action","action":{"type":"tap","label":"不存在"}},
                            {"kind":"action","action":{"type":"home"}}
                        ]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["failed_step"], 1);
        assert_eq!(json["completed"], 1);
        assert_eq!(json["applied_actions"], 1);
        assert_eq!(json["error"], "element_not_found");
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(json["retry_safe"], false);
        assert_eq!(json["steps"].as_array().unwrap().len(), 1);
        assert_eq!(json["steps"][0]["index"], 0);
        assert_eq!(json["steps"][0]["ok"], true);
    });
}

#[test]
fn direct_agent_input_does_not_replay_after_response_loss() {
    block(async {
        let action_count = Arc::new(AtomicUsize::new(0));
        let observed = action_count.clone();
        let (base, server) = mock_wda(2, move |request, _| {
            if request.starts_with("POST /session ") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ))
            } else {
                observed.fetch_add(1, Ordering::SeqCst);
                None
            }
        });
        let app = http::router(build_state_with_wda(&base));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/input")
                    .header("x-phone-control", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"home"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "outcome_unknown");
        assert_eq!(json["retry_safe"], false);
        assert_eq!(action_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn delayed_cold_wda_session_eventually_becomes_actionable() {
    block(async {
        let (base, server) = mock_wda(4, |request, _| {
            let (delay, body) = if request.starts_with("POST /session ") {
                (
                    std::time::Duration::from_millis(1_800),
                    r#"{"value":{"sessionId":"SESSION"}}"#,
                )
            } else if request.contains("/wda/locked") {
                (std::time::Duration::ZERO, r#"{"value":false}"#)
            } else if request.contains("/wda/apps/list") {
                (
                    std::time::Duration::ZERO,
                    r#"{"value":[{"bundleId":"com.apple.springboard","pid":123}]}"#,
                )
            } else {
                (std::time::Duration::ZERO, r#"{"value":{"ready":true}}"#)
            };
            Some((delay, body.to_string()))
        });
        let state = build_state_with_wda(&base);
        let app = http::router(state.clone());

        let first = app
            .oneshot(
                Request::builder()
                    .uri("/agent/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(2_200)).await;
        server.join().unwrap();

        let health = *state.wda_health.lock().unwrap();
        assert!(health.up);
        assert!(health.actionable);
        assert_eq!(health.locked, Some(false));
    });
}

#[test]
fn externally_managed_released_wda_is_not_bootstrapped_locally() {
    block(async {
        let state = build_state(None);
        state
            .released
            .store(true, std::sync::atomic::Ordering::Release);
        let observed = state.clone();
        let app = http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/input")
                    .header("x-phone-control", "1")
                    .body(Body::from(r#"{"type":"tap","x":0.1,"y":0.1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("wda_is_externally_managed"));
        assert!(observed.released.load(std::sync::atomic::Ordering::Acquire));
        assert!(!observed.wda_lifecycle.is_reconnecting());
    });
}

#[test]
fn agent_elements_reports_unavailable_instead_of_an_empty_success() {
    block(async {
        let app = http::router(build_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/elements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["elements"], serde_json::json!([]));
        assert_eq!(json["error"], "wda_not_configured");
    });
}

#[test]
fn failed_element_read_revokes_cached_actionability() {
    block(async {
        let (base, server) = mock_wda(4, |request, _| {
            if request.starts_with("POST /session ") {
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"sessionId":"SESSION"}}"#.to_string(),
                ))
            } else {
                assert!(
                    request.contains("/source?format=json"),
                    "unexpected WDA request: {request}"
                );
                Some((
                    std::time::Duration::ZERO,
                    r#"{"value":{"error":"unknown error","message":"source unavailable"}}"#
                        .to_string(),
                ))
            }
        });
        let state = build_state_with_wda(&base);
        state.wda_actionable.store(true, Ordering::Release);
        *state.wda_health.lock().unwrap() = server::wda::WdaHealth {
            up: true,
            actionable: true,
            locked: Some(false),
        };
        let observed = state.clone();
        let app = http::router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agent/elements")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(!observed.wda_actionable.load(Ordering::Acquire));
        let health = *observed.wda_health.lock().unwrap();
        assert!(health.up);
        assert!(!health.actionable);
        assert_eq!(health.locked, Some(false));
    });
}

#[test]
fn failed_screenshot_read_revokes_cached_actionability() {
    block(async {
        let (base, server) = mock_wda(1, |request, _| {
            assert!(
                request.starts_with("GET /screenshot "),
                "unexpected WDA request: {request}"
            );
            Some((
                std::time::Duration::ZERO,
                r#"{"value":{"error":"unknown error","message":"screenshot unavailable"}}"#
                    .to_string(),
            ))
        });
        let state = build_state_with_wda(&base);
        state.wda_actionable.store(true, Ordering::Release);
        *state.wda_health.lock().unwrap() = server::wda::WdaHealth {
            up: true,
            actionable: true,
            locked: Some(false),
        };
        let observed = state.clone();
        let app = http::router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agent/screenshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(!observed.wda_actionable.load(Ordering::Acquire));
        let health = *observed.wda_health.lock().unwrap();
        assert!(health.up);
        assert!(!health.actionable);
        assert_eq!(health.locked, Some(false));
    });
}

#[test]
fn agent_elements_accepts_the_logged_in_browser_session() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
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
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/agent/elements")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Auth passed. The test state has no WDA, so the honest result is 503
        // rather than a misleading empty tree or an authentication failure.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "wda_not_configured");
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
                    .header("x-phone-control", "1")
                    .body(Body::from(r#"{"type":"tap","x":0.1,"y":0.1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Auth passed (open mode) — NOT rejected as 401. Status may be 409
        // "dropped": no iPhone Mirroring window in the test env to deliver the
        // L3 tap to (issue #25 — input reports deliverability, not a blind
        // "ok"). This test is about the auth gate, so assert auth let it through.
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    });
}

#[test]
fn agent_screenshot_unauthed_is_401() {
    block(async {
        // Auth still required before any screenshot attempt.
        let app = http::router(build_state(Some("hunter2")));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/agent/screenshot")
                    .body(Body::empty())
                    .unwrap(),
            )
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
            resp.status() == StatusCode::SERVICE_UNAVAILABLE || resp.status() == StatusCode::OK,
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
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "attempt {i} should be 401"
            );
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
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "30");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("尝试次数过多。为保护手机，请 30 秒后再试"));
        assert!(html.contains(r#"aria-invalid="true""#));
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
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "correct login should succeed"
        );
        assert!(
            resp.headers().get(header::SET_COOKIE).is_some(),
            "should set session cookie"
        );

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
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "first wrong after reset should be 401"
        );
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
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "agent attempt {i} should be 401"
            );
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
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "agent token should be accepted"
        );

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
        let state = build_state_with_agent_token(Some("human-pass"), Some("sk-agent-secret"));
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
fn inbox_get_peeks_and_explicit_post_drain_is_atomic() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));

        // Phone (shortcut) POSTs a result.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/inbox")
                    .header("x-phone-control", "1")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::from(r#"{"verb":"battery","level":0.87}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Plain GET is safe and repeatable: both reads see the same item.
        for _ in 0..2 {
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
        }

        // The explicit, authenticated + CSRF-protected POST atomically drains.
        let drained = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/inbox/drain")
                    .header("x-phone-control", "1")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(drained.status(), StatusCode::OK);
        let body = drained.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("\"verb\":\"battery\""));

        let empty = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agent/inbox/drain")
                    .header("x-phone-control", "1")
                    .header(header::AUTHORIZATION, "Bearer hunter2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = empty.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(String::from_utf8_lossy(&body), r#"{"items":[]}"#);
    });
}

#[test]
fn inbox_mutations_require_auth() {
    block(async {
        let app = http::router(build_state(Some("hunter2")));
        for uri in ["/agent/inbox", "/agent/inbox/drain"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("x-phone-control", "1")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "uri={uri}");
        }
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
                    .header("x-phone-control", "1")
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
