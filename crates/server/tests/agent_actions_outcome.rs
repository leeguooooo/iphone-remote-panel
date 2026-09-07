//! `POST /agent/actions` — what the failed STEP did is not what the BATCH did.
//!
//! Hardware acceptance produced a failure body carrying `applied_actions: 2`
//! next to `outcome: "not_sent"`. Both were true of different things: two
//! actions really had reached the phone, and the step that failed really had
//! not been sent. Read as a batch verdict, it says the opposite of what
//! happened.
//!
//! `outcome` is frozen — callers read it and it keeps meaning the failed step.
//! `failed_step_outcome` is the same value under an honest name, and
//! `batch_outcome` is the batch's own verdict. `retry_safe` is unchanged and
//! remains the only authorisation to send a batch again; none of these three
//! fields may be used to infer it.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// An application with no children: readable, but nothing matches a locator.
const BARE_TREE: &str = r#"{"value":{
    "type":"XCUIElementTypeApplication",
    "label":"测试应用",
    "rect":{"x":0,"y":0,"width":390,"height":844},
    "children":[]
}}"#;

async fn post_actions(base: Option<&str>, body: &str) -> (StatusCode, serde_json::Value) {
    let state = match base {
        Some(base) => build_state_with_wda(base),
        None => build_state(None),
    };
    let app = server::http::router(state);
    let request = Request::builder()
        .method("POST")
        .uri("/agent/actions")
        .header("x-phone-control", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// The shape found on hardware: an action lands, then an expectation fails.
#[test]
fn an_applied_action_before_a_failed_expectation_is_a_partial_batch() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                return Some((Duration::ZERO, BARE_TREE.to_string()));
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[
                {"kind":"action","action":{"type":"home"}},
                {"kind":"wait_for","expect":{"present":[{"label":"nothing here"}]},
                 "timeout_ms":200,"poll_ms":50}
            ]}"#,
        )
        .await;

        assert_eq!(json["ok"], false, "{json}");
        assert!(
            json["applied_actions"].as_u64().is_some_and(|n| n > 0),
            "the first action must have been applied: {json}"
        );
        // Frozen: existing callers keep reading the failed step here.
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert_eq!(json["failed_step_outcome"], "not_sent", "{json}");
        assert_eq!(
            json["batch_outcome"], "partially_applied",
            "actions reached the phone, so the batch is not `nothing_applied`: {json}"
        );
        assert_eq!(
            json["retry_safe"], false,
            "a batch that already applied actions is never safe to replay: {json}"
        );
    });
}

/// The last action's fate is unknown. That must NOT be dressed up as a partial
/// batch, which would assert the earlier actions are settled and only the last
/// one is in doubt.
#[test]
fn an_unknown_step_makes_the_whole_batch_unknown() {
    block(async {
        let wda = mock_wda(|request, index| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            // First mutation lands; the second gets no answer at all.
            if index >= 2 {
                return None;
            }
            Some((Duration::ZERO, r#"{"value":null}"#.to_string()))
        });
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[
                {"kind":"action","action":{"type":"home"}},
                {"kind":"action","action":{"type":"home"}}
            ]}"#,
        )
        .await;

        assert_eq!(json["ok"], false, "{json}");
        // Pin WHICH step failed: without this, a first-step failure would
        // reach the same `unknown` verdict and the test would pass for the
        // wrong reason.
        assert_eq!(
            json["applied_actions"], 1,
            "the first action must have applied: {json}"
        );
        assert_eq!(
            json["failed_step"], 1,
            "it must be the SECOND step whose outcome is unknown: {json}"
        );
        assert_eq!(json["failed_step_outcome"], "unknown", "{json}");
        assert_eq!(
            json["batch_outcome"], "unknown",
            "an unknown step outcome must not be reported as a known partial: {json}"
        );
        assert_ne!(json["batch_outcome"], "partially_applied", "{json}");
        assert_eq!(json["retry_safe"], false, "{json}");
    });
}

/// Nothing ran at all: refused by local validation before the first step.
#[test]
fn a_locally_refused_batch_applied_nothing() {
    block(async {
        let (status, json) = post_actions(
            None,
            r#"{"steps":[
                {"kind":"action","action":{"type":"home"}},
                {"kind":"action","action":{"type":"uninstall","bundle":"com.example.app"}}
            ]}"#,
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["error"], "invalid_actions_request", "{json}");
        assert_eq!(json["failed_step_outcome"], "not_sent", "{json}");
        assert_eq!(
            json["batch_outcome"], "nothing_applied",
            "a batch refused before its first step applied nothing: {json}"
        );
        assert_eq!(json["retry_safe"], true, "{json}");
    });
}

/// Zero actions applied, but the first step's fate is unknown. `nothing_applied`
/// would be a claim the daemon cannot make.
#[test]
fn zero_applied_actions_with_an_unknown_first_step_is_still_unknown() {
    block(async {
        let wda = mock_wda(|request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            // The very first mutation goes out and is never answered.
            None
        });
        let (_, json) = post_actions(
            Some(wda.url()),
            r#"{"steps":[{"kind":"action","action":{"type":"home"}}]}"#,
        )
        .await;

        assert_eq!(json["ok"], false, "{json}");
        assert_eq!(json["applied_actions"], 0, "{json}");
        assert_eq!(json["failed_step_outcome"], "unknown", "{json}");
        assert_eq!(
            json["batch_outcome"], "unknown",
            "a count of zero does not prove nothing was applied when the \
             outcome itself is unknown: {json}"
        );
        assert_eq!(json["retry_safe"], false, "{json}");
    });
}

/// The batch never starts because the backend cannot drive: also zero actions,
/// and it should say so in the same words as every other zero-action refusal.
#[test]
fn a_batch_refused_before_it_starts_reports_nothing_applied() {
    block(async {
        // The fixture state has no WDA configured, so a well-formed batch is
        // refused after validation and before the first step.
        let (status, json) = post_actions(
            None,
            r#"{"steps":[{"kind":"action","action":{"type":"home"}}]}"#,
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
        assert_eq!(json["error"], "wda_not_configured", "{json}");
        assert_eq!(json["outcome"], "not_sent", "{json}");
        assert_eq!(json["failed_step_outcome"], "not_sent", "{json}");
        assert_eq!(json["batch_outcome"], "nothing_applied", "{json}");
        assert_eq!(json["retry_safe"], true, "{json}");
    });
}
