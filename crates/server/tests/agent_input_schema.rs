//! `POST /agent/input` — a malformed request is the caller's problem, not the
//! device's.
//!
//! The defect these tests pin was found on hardware: sending the body
//! `{"action":"home"}` (the field is `type`, not `action`) answered
//! `503 wda_unavailable_or_unsupported` while WDA was healthy and answering.
//! A caller reading that would go restart a phone that was never at fault.
//!
//! The contract: an action the server cannot name is refused with `400
//! invalid_action` BEFORE any WDA work, and the mock proves WDA was never
//! contacted — not even for a session.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// Post one body through the real router and report what WDA saw.
async fn post_input(body: &str) -> (StatusCode, serde_json::Value, usize) {
    let contacts = Arc::new(AtomicUsize::new(0));
    let seen = contacts.clone();
    let wda = mock_wda(move |request, _| {
        seen.fetch_add(1, Ordering::Release);
        if request.starts_with("POST /session ") {
            return Some((Duration::ZERO, SESSION.to_string()));
        }
        Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
    });
    let state = build_state_with_wda(wda.url());
    let app = server::http::router(state);
    let request = Request::builder()
        .method("POST")
        .uri("/agent/input")
        .header("x-phone-control", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    let count = contacts.load(Ordering::Acquire);
    drop(wda);
    (status, json, count)
}

/// The exact body that produced the hardware defect.
#[test]
fn the_wrong_field_name_is_a_client_error_and_never_reaches_wda() {
    block(async {
        let (status, json, contacts) = post_input(r#"{"action":"home"}"#).await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a body with no `type` is malformed, not a device failure: {json}"
        );
        assert_eq!(json["error"], "invalid_action", "{json}");
        assert_ne!(
            json["error"], "wda_unavailable_or_unsupported",
            "a healthy phone must never be blamed for a typo: {json}"
        );
        assert_eq!(
            contacts, 0,
            "the request must be refused before WDA is contacted at all"
        );
        // Zero WDA contacts is exactly what makes this safe to fix and resend,
        // so the refusal has to SAY so rather than leave the caller to infer it.
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert_eq!(json["retry_safe"], true, "{json}");
        // The refusal has to be actionable on its own: it says what is wrong
        // and what the server would have accepted.
        assert!(
            json["detail"].as_str().is_some_and(|d| d.contains("type")),
            "{json}"
        );
        assert!(
            json["supported"]
                .as_array()
                .is_some_and(|names| names.iter().any(|name| name == "home")),
            "{json}"
        );
    });
}

/// A well-formed body naming an action that does not exist.
#[test]
fn an_unknown_action_name_is_a_client_error_and_never_reaches_wda() {
    block(async {
        let (status, json, contacts) =
            post_input(r#"{"type":"teleport","x":0.5,"y":0.5}"#).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "invalid_action", "{json}");
        assert!(
            json["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("teleport")),
            "the refusal must name the action it rejected: {json}"
        );
        assert_eq!(contacts, 0, "an unknown action must not open a session");
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert_eq!(json["retry_safe"], true, "{json}");
    });
}

/// `type` present but empty, and `type` of the wrong JSON type: both are
/// nameless actions and take the same path.
#[test]
fn an_empty_or_non_string_type_is_refused_without_touching_wda() {
    block(async {
        for body in [r#"{"type":""}"#, r#"{"type":42}"#, r#"{}"#] {
            let (status, json, contacts) = post_input(body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {json}");
            assert_eq!(json["error"], "invalid_action", "{body} -> {json}");
            assert_eq!(contacts, 0, "{body} must not reach WDA");
            assert_eq!(json["outcome"], "not_sent", "{body} -> {json}");
            assert_eq!(json["retry_safe"], true, "{body} -> {json}");
        }
    });
}

/// The gate must not swallow real work: a named action still goes through and
/// still reaches the device. Without this, "refuse everything" would pass the
/// three tests above.
#[test]
fn a_named_action_still_reaches_wda() {
    block(async {
        let (status, json, contacts) = post_input(r#"{"type":"home"}"#).await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true, "{json}");
        assert!(
            contacts > 0,
            "a valid action must still be dispatched to the device"
        );
    });
}
