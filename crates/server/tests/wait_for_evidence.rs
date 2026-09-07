//! `POST /agent/actions` — a screen nobody could read must never pass for
//! evidence that something is gone.
//!
//! `wait_for`'s `absent` expectation is the dangerous one: an empty or
//! container-only tree satisfies it vacuously. WDA hands back exactly such a
//! tree mid-transition and whenever the read path is degraded, so the endpoint
//! has to tell "I looked and it is not there" apart from "I could not see".

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use support::{block, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// Application row only: readable, but nothing to act on.
const CONTAINER_ONLY: &str = r#"{"value":{
    "type":"XCUIElementTypeApplication",
    "label":"测试应用",
    "rect":{"x":0,"y":0,"width":390,"height":844},
    "children":[]
}}"#;

/// A real screen that genuinely does not contain the button under test.
const OTHER_SCREEN: &str = r#"{"value":{
    "type":"XCUIElementTypeApplication",
    "label":"测试应用",
    "rect":{"x":0,"y":0,"width":390,"height":844},
    "children":[{
        "type":"XCUIElementTypeButton",
        "label":"完成",
        "rect":{"x":160,"y":700,"width":70,"height":40},
        "isEnabled":true,
        "children":[]
    }]
}}"#;

/// A screen that DOES contain the button the `absent` expectation forbids.
const PRESENT_SCREEN: &str = r#"{"value":{
    "type":"XCUIElementTypeApplication",
    "label":"测试应用",
    "rect":{"x":0,"y":0,"width":390,"height":844},
    "children":[{
        "type":"XCUIElementTypeButton",
        "label":"搜索",
        "rect":{"x":160,"y":700,"width":70,"height":40},
        "isEnabled":true,
        "children":[]
    }]
}}"#;

// The window is wider than these cases need. Each one is about WHICH outcome
// the daemon reports, not about how fast it gets there, and a tight window
// made that depend on how loaded the machine was — a test that silently
// switches which scenario it exercises is worse than one that fails.
const WAIT_FOR_ABSENT: &str = r#"{"steps":[
    {"kind":"wait_for",
     "expect":{"absent":[{"kind":"Button","label":"搜索"}]},
     "timeout_ms":2000,"poll_ms":100}
]}"#;

async fn run_actions(base: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let app = server::http::router(build_state_with_wda(base));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agent/actions")
                .header("x-phone-control", "1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

fn scripted(tree: &'static str) -> support::MockWda {
    mock_wda(move |request, _| {
        if request.starts_with("POST /session ") {
            return Some((Duration::ZERO, SESSION.to_string()));
        }
        if request.contains("/source?format=json") {
            return Some((Duration::ZERO, tree.to_string()));
        }
        None
    })
}

#[test]
fn a_container_only_tree_never_proves_an_element_is_absent() {
    block(async {
        let wda = scripted(CONTAINER_ONLY);
        let (status, json) = run_actions(wda.url(), WAIT_FOR_ABSENT).await;

        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"], "expectation_timeout");
        assert_eq!(json["failed_step"], 0, "the 0-based failed step is preserved");
        let observation = &json["observation"];
        assert_eq!(observation["read"], true, "the tree WAS read: {json}");
        assert_eq!(observation["sparse"], true);
        assert_eq!(
            observation["absent_unproven"],
            serde_json::json!([0]),
            "the absent expectation was not proven, only unobservable: {json}"
        );
        assert_eq!(observation["violated_absent"], serde_json::json!([]));
    });
}

#[test]
fn a_real_screen_without_the_element_does_satisfy_absent() {
    block(async {
        let wda = scripted(OTHER_SCREEN);
        let (status, json) = run_actions(wda.url(), WAIT_FOR_ABSENT).await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["completed"], 1);
        let observation = &json["steps"][0]["observation"];
        assert_eq!(observation["read"], true);
        assert_eq!(observation["sparse"], false);
        assert_eq!(observation["absent_unproven"], serde_json::json!([]));
    });
}

/// Never obtaining a tree is a THIRD outcome, and it must not arrive as a null
/// observation a caller can squint at and read as "nothing was there".
#[test]
fn a_tree_that_never_reads_says_so_instead_of_reporting_nothing() {
    block(async {
        let wda = mock_wda(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            None // every source read loses the connection
        });
        let (status, json) = run_actions(wda.url(), WAIT_FOR_ABSENT).await;

        // Whether the window ends on a failed read or on the deadline is a
        // race; both are honest. What must NOT vary is the evidence.
        assert!(
            status == StatusCode::BAD_GATEWAY || status == StatusCode::CONFLICT,
            "{status} {json}"
        );
        assert!(
            json["error"] == "wda_source_failed" || json["error"] == "expectation_timeout",
            "{json}"
        );
        assert_eq!(json["outcome"], "not_sent");
        assert_eq!(json["retry_safe"], true, "nothing was sent, so retrying is safe");
        let observation = &json["observation"];
        assert!(!observation.is_null(), "must not be a bare null: {json}");
        assert_eq!(observation["read"], false);
        assert_eq!(observation["reads"], 0, "no read ever succeeded: {json}");
        assert!(observation["stale"].is_null(), "nothing to be stale about");
        assert!(observation["attempts"].as_u64().unwrap() >= 1);
        assert!(observation["read_error"].is_string(), "{json}");
        assert!(observation["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("absent")));
    });
}

/// A late run of read failures must not erase the reads that DID succeed. The
/// screen was seen; the last look just failed. That is `stale`, not "never
/// read" — reporting the latter would throw away the only real evidence the
/// caller has.
#[test]
fn one_good_read_survives_a_failing_tail() {
    block(async {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let wda = mock_wda(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                // One good look at a screen that still holds the button the
                // caller is waiting to see GO — then the read path dies.
                let n = reads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                if n == 0 {
                    return Some((Duration::ZERO, PRESENT_SCREEN.to_string()));
                }
                return None;
            }
            None
        });
        let (status, json) = run_actions(wda.url(), WAIT_FOR_ABSENT).await;

        assert!(
            status == StatusCode::BAD_GATEWAY || status == StatusCode::CONFLICT,
            "{status} {json}"
        );
        let observation = &json["observation"];
        assert_eq!(observation["read"], true, "one read DID succeed: {json}");
        assert_eq!(observation["reads"], 1);
        assert_eq!(observation["stale"], true, "the last read failed: {json}");
        assert!(observation["read_error"].is_string());
        // The evidence from that one good read is intact: the button was there,
        // which is why the `absent` expectation was violated rather than
        // unproven.
        assert_eq!(observation["violated_absent"], serde_json::json!([0]));
        assert_eq!(observation["sparse"], false);
        assert_eq!(json["applied_actions"], 0, "no mutation was in this flow");
        assert_eq!(json["outcome"], "not_sent");
    });
}

/// A read that HANGS is a failed read too. Distinct from a dropped connection,
/// and it must reach the caller as staleness rather than as a fresh look.
#[test]
fn a_good_read_followed_by_a_hanging_one_is_reported_stale() {
    block(async {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let wda = mock_wda(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                let n = reads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                if n == 0 {
                    return Some((Duration::ZERO, PRESENT_SCREEN.to_string()));
                }
                // Hangs past the wait window instead of dropping.
                return Some((Duration::from_secs(5), PRESENT_SCREEN.to_string()));
            }
            None
        });
        let (status, json) = run_actions(wda.url(), WAIT_FOR_ABSENT).await;

        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["error"], "expectation_timeout");
        let observation = &json["observation"];
        assert_eq!(observation["read"], true);
        assert_eq!(observation["reads"], 1);
        assert_eq!(observation["stale"], true, "the read that ended the window hung: {json}");
        assert!(observation["read_error"]
            .as_str()
            .is_some_and(|error| error.contains("timed out")), "{json}");
        assert_eq!(observation["violated_absent"], serde_json::json!([0]));
    });
}

/// The first read hanging means nothing was ever seen — `read:false`, and the
/// reason has to say "timed out", not go missing because no connection broke.
#[test]
fn a_first_read_that_hangs_reports_never_read_with_a_timeout_reason() {
    block(async {
        let wda = mock_wda(move |request, _| {
            if request.starts_with("POST /session ") {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if request.contains("/source?format=json") {
                return Some((Duration::from_secs(5), PRESENT_SCREEN.to_string()));
            }
            None
        });
        let (status, json) = run_actions(wda.url(), WAIT_FOR_ABSENT).await;

        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        let observation = &json["observation"];
        assert_eq!(observation["read"], false, "{json}");
        assert_eq!(observation["reads"], 0);
        assert!(observation["stale"].is_null(), "nothing to be stale about");
        assert!(observation["read_error"]
            .as_str()
            .is_some_and(|error| error.contains("timed out")), "{json}");
        assert!(observation["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("absent")));
    });
}

/// The `wait_for` ceiling moved for the same reason the observation budget
/// did — one element read can cost six or seven seconds, so a 10s ceiling left
/// room for a single poll. What did NOT move is the cumulative cap: a longer
/// single wait must not let a batch run longer overall.
#[test]
fn a_single_wait_may_now_be_thirty_seconds_but_the_batch_cap_holds() {
    block(async {
        let wda = scripted(OTHER_SCREEN);

        // 30s exactly: accepted.
        let (status, json) = run_actions(
            wda.url(),
            r#"{"steps":[{"kind":"wait_for",
                "expect":{"absent":[{"kind":"Button","label":"搜索"}]},
                "timeout_ms":30000,"poll_ms":100}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "30s must be accepted: {json}");

        // One millisecond over: refused, and nothing is sent.
        let (status, json) = run_actions(
            wda.url(),
            r#"{"steps":[{"kind":"wait_for",
                "expect":{"absent":[{"kind":"Button","label":"搜索"}]},
                "timeout_ms":30001,"poll_ms":100}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_eq!(json["outcome"], "not_sent");

        // Three 30s waits declare 90s, over the unchanged 60s cumulative cap.
        let (status, json) = run_actions(
            wda.url(),
            r#"{"steps":[
                {"kind":"wait_for","expect":{"present":[{"kind":"Button"}]},"timeout_ms":30000,"poll_ms":100},
                {"kind":"wait_for","expect":{"present":[{"kind":"Button"}]},"timeout_ms":30000,"poll_ms":100},
                {"kind":"wait_for","expect":{"present":[{"kind":"Button"}]},"timeout_ms":30000,"poll_ms":100}
            ]}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a longer single wait must not lengthen a batch: {json}"
        );
        assert_eq!(json["outcome"], "not_sent");
    });
}
