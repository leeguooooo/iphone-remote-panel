//! `POST /agent/input?return=delta` — the action result and the post-action
//! observation are separate things.
//!
//! The contract these tests pin: once the mutation is dispatched and WDA
//! reports it applied, NOTHING that happens during the follow-up observation
//! may downgrade that result to an unknown outcome, and the mutation is never
//! sent twice. Observation quality is reported in the additive `settle` block.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use server::http::AppState;
use support::{block, build_state_with_wda, mock_wda};

const SESSION: &str = r#"{"value":{"sessionId":"SESSION"}}"#;

/// Application + one real Button: a deliberately minimal screen that still has
/// something to act on, so it MUST be allowed to settle.
fn simple_tree(label: &str) -> String {
    format!(
        r#"{{"value":{{
            "type":"XCUIElementTypeApplication",
            "label":"测试应用",
            "rect":{{"x":0,"y":0,"width":390,"height":844}},
            "children":[{{
                "type":"XCUIElementTypeButton",
                "label":"{label}",
                "rect":{{"x":160,"y":700,"width":70,"height":40}},
                "isEnabled":true,
                "children":[]
            }}]
        }}}}"#
    )
}

/// Container-only: nothing to act on. Two identical reads of this are evidence
/// we cannot see the screen, not evidence it settled.
fn bare_tree() -> String {
    r#"{"value":{
        "type":"XCUIElementTypeApplication",
        "label":"测试应用",
        "rect":{"x":0,"y":0,"width":390,"height":844},
        "children":[]
    }}"#
    .to_string()
}

fn is_session(request: &str) -> bool {
    request.starts_with("POST /session ")
}
fn is_mutation(request: &str) -> bool {
    request.contains("/wda/pressButton")
}
fn is_source(request: &str) -> bool {
    request.contains("/source?format=json")
}

/// One request through the real router against a shared state (so element
/// snapshots taken by an earlier call are still cached for a later baseline).
async fn request_json(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value, Duration) {
    let app = server::http::router(state.clone());
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-phone-control", "1");
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let started = std::time::Instant::now();
    let response = app.oneshot(request).await.unwrap();
    let elapsed = started.elapsed();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json, elapsed)
}

/// Drive one `home` press through the real router and return
/// (status, json, elapsed).
async fn press_home(base: &str, query: &str) -> (StatusCode, serde_json::Value, Duration) {
    let state = build_state_with_wda(base);
    request_json(
        &state,
        "POST",
        &format!("/agent/input{query}"),
        Some(r#"{"type":"home"}"#),
    )
    .await
}

#[test]
fn settle_reports_stable_on_a_minimal_but_readable_screen() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let sources = Arc::new(AtomicUsize::new(0));
        let (seen_mutations, seen_sources) = (mutations.clone(), sources.clone());
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                seen_sources.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, simple_tree("搜索")));
            }
            None // alert probe: no alert
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["settled"], true);
        assert_eq!(json["settle"]["reason"], "stable");
        assert_eq!(json["settle"]["captures"], 2);
        assert!(json["settle"]["sparse"].is_null(), "{json}");
        assert!(json["settle"]["stale"].is_null(), "{json}");
        assert!(json["snapshot"].is_string());
        assert_eq!(mutations.load(Ordering::Acquire), 1);
    });
}

#[test]
fn a_screen_that_keeps_changing_is_reported_unsettled_not_failed() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let seen_mutations = mutations.clone();
        let reads = AtomicUsize::new(0);
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                let n = reads.fetch_add(1, Ordering::AcqRel);
                return Some((Duration::ZERO, simple_tree(&format!("按钮{n}"))));
            }
            None
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta&settle_ms=800").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["settled"], false);
        assert_eq!(json["settle"]["reason"], "budget_exhausted", "{json}");
        assert!(json["settle"]["captures"].as_u64().unwrap() >= 2, "{json}");
        assert!(
            json["snapshot"].is_string(),
            "the last read is still returned"
        );
        assert_eq!(mutations.load(Ordering::Acquire), 1);
    });
}

#[test]
fn a_zero_settle_budget_reads_no_source_at_all() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let sources = Arc::new(AtomicUsize::new(0));
        let (seen_mutations, seen_sources) = (mutations.clone(), sources.clone());
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                seen_sources.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, simple_tree("搜索")));
            }
            None
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta&settle_ms=0").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["reason"], "budget_exhausted");
        assert_eq!(json["settle"]["captures"], 0);
        assert!(json["snapshot"].is_null(), "no tree was read: {json}");
        assert_eq!(
            sources.load(Ordering::Acquire),
            0,
            "an expired budget must not spend a read"
        );
        assert_eq!(mutations.load(Ordering::Acquire), 1);
    });
}

/// A budget too small to survive the settle delay is still a *positive*
/// budget, so it takes a different path than `settle_ms=0`: the zero case
/// returns before sleeping at all, while this one sleeps first and must then
/// notice its deadline has passed. Both must cost zero reads — polling an
/// expired deadline would spend a `/source` and report a self-inflicted
/// observation failure instead of an honest "no budget".
#[test]
fn a_budget_smaller_than_the_settle_delay_reads_no_source_either() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let sources = Arc::new(AtomicUsize::new(0));
        let (seen_mutations, seen_sources) = (mutations.clone(), sources.clone());
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                seen_sources.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, simple_tree("搜索")));
            }
            None
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta&settle_ms=1").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true, "the action itself must still succeed: {json}");
        assert_eq!(json["settle"]["reason"], "budget_exhausted");
        assert_eq!(json["settle"]["captures"], 0);
        assert_eq!(json["settle"]["budget_ms"], 1);
        assert!(json["snapshot"].is_null(), "no tree was read: {json}");
        assert_eq!(
            sources.load(Ordering::Acquire),
            0,
            "a budget that expires during the settle delay must not spend a read"
        );
        assert_eq!(
            mutations.load(Ordering::Acquire),
            1,
            "the mutation is sent exactly once regardless of the observation budget"
        );
    });
}

/// THE regression. Before the split, the observation ran inside the action's
/// own 15s deadline with no per-read bound, so one hung `/source` turned an
/// applied action into `504 outcome_unknown / retry_safe:false` — the one
/// answer that forbids the caller from retrying and forces a human in.
#[test]
fn a_hung_source_read_never_downgrades_an_applied_action_to_unknown() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let seen_mutations = mutations.clone();
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                // Longer than the endpoint's whole 15s action deadline.
                return Some((Duration::from_secs(20), simple_tree("搜索")));
            }
            None
        });

        let (status, json, elapsed) = press_home(wda.url(), "?return=delta&settle_ms=300").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["settled"], false);
        // Running out of time is not the read path breaking, and is not
        // reported as one. `captures:0` is what says nothing was seen.
        assert_eq!(json["settle"]["reason"], "budget_exhausted", "{json}");
        assert_eq!(json["settle"]["captures"], 0);
        assert!(json["snapshot"].is_null(), "{json}");
        assert!(
            elapsed < Duration::from_secs(10),
            "the answer must arrive on the settle budget, not the action deadline: {elapsed:?}"
        );
        assert_eq!(
            mutations.load(Ordering::Acquire),
            1,
            "the mutation must not be re-sent"
        );
    });
}

#[test]
fn a_failing_source_read_is_reported_beside_the_applied_action() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let seen_mutations = mutations.clone();
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            None // every source read (and its retry) loses the connection
        });

        // Same reasoning as the stale case: every read fails immediately, so a
        // generous budget costs nothing and keeps the test from turning into
        // `budget_exhausted` under load.
        let (status, json, _) = press_home(wda.url(), "?return=delta&settle_ms=5000").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["reason"], "observation_failed");
        assert!(json["settle"]["error"].is_string(), "{json}");
        assert!(json["delta_error"].is_string(), "legacy field kept: {json}");
        assert_eq!(mutations.load(Ordering::Acquire), 1);
    });
}

#[test]
fn a_hung_alert_probe_does_not_hold_up_the_applied_action() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let seen_mutations = mutations.clone();
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                return Some((Duration::ZERO, simple_tree("搜索")));
            }
            // The alert probe hangs well past its own 1.5s cap.
            Some((Duration::from_secs(20), r#"{"value":"确认"}"#.to_string()))
        });

        let (status, json, elapsed) = press_home(wda.url(), "?return=delta").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["reason"], "stable");
        assert!(
            json["alert"].is_null(),
            "an unreadable alert is omitted: {json}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the alert probe must stay bounded: {elapsed:?}"
        );
        assert_eq!(mutations.load(Ordering::Acquire), 1);
    });
}

/// The other side of the contract: when the MUTATION itself is what hangs, the
/// outcome genuinely is unknown and must stay that way.
#[test]
fn a_hung_mutation_still_reports_an_unknown_outcome() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let seen_mutations = mutations.clone();
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::from_secs(20), r#"{"value":null}"#.to_string()));
            }
            None
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta").await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{json}");
        assert_eq!(json["outcome"], "unknown");
        assert_eq!(json["retry_safe"], false);
        assert_eq!(
            mutations.load(Ordering::Acquire),
            1,
            "sent exactly once, never retried into a double press"
        );
    });
}

/// A session that never establishes is a DIFFERENT failure: no mutation ever
/// reached the phone. The count proves it (the daemon's own verdict stays
/// conservative, which is why this is asserted separately from the case above).
#[test]
fn a_hung_session_handshake_sends_no_mutation() {
    block(async {
        let mutations = Arc::new(AtomicUsize::new(0));
        let seen_mutations = mutations.clone();
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::from_secs(20), SESSION.to_string()));
            }
            if is_mutation(request) {
                seen_mutations.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            None
        });

        let (status, _json, _) = press_home(wda.url(), "?return=delta").await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            mutations.load(Ordering::Acquire),
            0,
            "nothing was pressed on the phone"
        );
    });
}

/// Two identical container-only trees are not a settled screen.
#[test]
fn two_identical_container_only_trees_are_not_called_stable() {
    block(async {
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                return Some((Duration::ZERO, bare_tree()));
            }
            None
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta&settle_ms=700").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["settled"], false);
        assert_ne!(json["settle"]["reason"], "stable");
        assert_eq!(json["settle"]["sparse"], true);
    });
}

/// `?since=` against a baseline the daemon still holds returns a DELTA, and the
/// legacy delta shape is untouched by the new `settle` block.
#[test]
fn a_live_baseline_still_produces_a_delta_not_a_full_tree() {
    block(async {
        let reads = AtomicUsize::new(0);
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                // The first read is the caller's baseline; every read after the
                // action shows the changed screen and then holds still.
                let n = reads.fetch_add(1, Ordering::AcqRel);
                let label = if n == 0 { "搜索" } else { "取消" };
                return Some((Duration::ZERO, simple_tree(label)));
            }
            None
        });
        let state = build_state_with_wda(wda.url());

        let (status, baseline, _) = request_json(&state, "GET", "/agent/elements", None).await;
        assert_eq!(status, StatusCode::OK, "{baseline}");
        let since = baseline["snapshot"].as_str().unwrap().to_string();

        let (status, json, _) = request_json(
            &state,
            "POST",
            &format!("/agent/input?return=delta&since={since}"),
            Some(r#"{"type":"home"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["baseline"], since);
        assert!(json["delta"].is_object(), "{json}");
        assert!(
            json["elements"].is_null(),
            "a live baseline must not fall back to a full tree"
        );
        assert_eq!(json["settle"]["reason"], "stable");
    });
}

/// A baseline the daemon no longer holds degrades to the full tree — the
/// pre-existing contract, unchanged by `settle`.
#[test]
fn an_unknown_baseline_falls_back_to_the_full_tree() {
    block(async {
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                return Some((Duration::ZERO, simple_tree("搜索")));
            }
            None
        });

        let (status, json, _) =
            press_home(wda.url(), "?return=delta&since=not-a-cached-snapshot").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert!(json["elements"].is_array(), "{json}");
        assert!(json["baseline"].is_null());
        assert!(json["delta"].is_null());
        assert_eq!(json["settle"]["reason"], "stable");
    });
}

/// The default (no `?return=`) must stay exactly as cheap as it was: one
/// mutation, no observation, no `settle` block, no added latency.
#[test]
fn the_default_response_reads_nothing_extra() {
    block(async {
        let sources = Arc::new(AtomicUsize::new(0));
        let seen_sources = sources.clone();
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                seen_sources.fetch_add(1, Ordering::Release);
                return Some((Duration::ZERO, simple_tree("搜索")));
            }
            None
        });

        let (status, json, elapsed) = press_home(wda.url(), "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert!(
            json["settle"].is_null(),
            "no observation was asked for: {json}"
        );
        assert!(json["snapshot"].is_null());
        assert_eq!(sources.load(Ordering::Acquire), 0);
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    });
}

/// A read that succeeds and then stops working hands back the last tree it did
/// read — labelled `stale`, never as a fresh settled screen.
#[test]
fn a_read_that_fails_after_a_good_one_returns_a_stale_observation() {
    block(async {
        let reads = AtomicUsize::new(0);
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                let n = reads.fetch_add(1, Ordering::AcqRel);
                if n == 0 {
                    return Some((Duration::ZERO, simple_tree("搜索")));
                }
                return None; // every later read loses the connection
            }
            None
        });

        // The budget is deliberately the maximum, and the run still finishes in
        // well under a second: the second read fails immediately, so the budget
        // only ever bounds the worst case.
        //
        // A tight budget made this test load-dependent — under a full parallel
        // suite the deadline could arrive before the failing read did, and the
        // case quietly turned into the OTHER outcome (`budget_exhausted`),
        // which is exactly the pair of meanings this file exists to keep apart.
        // A test that silently changes which scenario it exercises is worse
        // than one that fails.
        let (status, json, _) = press_home(wda.url(), "?return=delta&settle_ms=5000").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["reason"], "observation_failed");
        assert_eq!(json["settle"]["settled"], false);
        assert_eq!(json["settle"]["stale"], true);
        assert_eq!(json["settle"]["captures"], 1);
        assert!(
            json["snapshot"].is_string(),
            "the last good tree is still returned"
        );
    });
}

/// What the widened budget actually buys, demonstrated mechanically rather
/// than by hoping a loaded machine misbehaves.
///
/// The first read is deliberately slowed by ~1s — comfortably inside the 5s
/// budget, and far more than the ~400ms of nominal work. The case must still
/// end where it is supposed to: on the FAILING read, not on the deadline.
///
/// This validates the headroom against a controlled delay. It does NOT
/// reproduce the scheduling delay that made the tight-budget version flaky on
/// a loaded machine; that observation stands on its own.
#[test]
fn a_slow_first_read_still_ends_on_the_failure_not_the_budget() {
    block(async {
        let reads = AtomicUsize::new(0);
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                let n = reads.fetch_add(1, Ordering::AcqRel);
                if n == 0 {
                    // Slow, but well inside the budget.
                    return Some((Duration::from_millis(1000), simple_tree("搜索")));
                }
                return None; // the read path then dies
            }
            None
        });

        let (status, json, elapsed) = press_home(wda.url(), "?return=delta&settle_ms=5000").await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(
            json["settle"]["reason"], "observation_failed",
            "a slow-but-affordable first read must not turn this into budget_exhausted: {json}"
        );
        assert_eq!(json["settle"]["captures"], 1);
        assert_eq!(json["settle"]["stale"], true);
        assert!(json["settle"]["error"].is_string(), "{json}");
        assert!(
            elapsed < Duration::from_secs(5),
            "and it still returns long before the budget: {elapsed:?}"
        );
    });
}

/// A second sample cancelled by the budget still returns the first tree — but
/// it is the PREVIOUS observation, not the current screen, and must say so.
/// The reason stays `budget_exhausted`: a cancelled read is not a broken one.
#[test]
fn a_sample_cut_off_by_the_budget_marks_the_returned_tree_stale() {
    block(async {
        let reads = AtomicUsize::new(0);
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                let n = reads.fetch_add(1, Ordering::AcqRel);
                if n == 0 {
                    return Some((Duration::ZERO, simple_tree("搜索")));
                }
                return Some((Duration::from_secs(5), simple_tree("搜索")));
            }
            None
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta&settle_ms=600").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(json["settle"]["reason"], "budget_exhausted", "{json}");
        assert_eq!(json["settle"]["settled"], false);
        assert_eq!(json["settle"]["stale"], true, "the second sample never finished: {json}");
        assert_eq!(json["settle"]["captures"], 1);
        assert!(json["snapshot"].is_string(), "the first tree is still returned");
        assert!(json["settle"]["error"].is_null(), "nothing broke: {json}");
    });
}

/// The failure this whole budget rework exists for, reproduced without a phone.
///
/// On the candidate hardware one element read costs 6–7 seconds. Under the old
/// ceiling (settle_ms max 5000) the DEFAULT observation could never finish even
/// one read: every caller got `captures: 0` and a bare `budget_exhausted`, with
/// nothing in the answer to say the budget was simply too small for this phone.
///
/// With a 15s default the same phone gets two reads — so the default call can
/// still reach `stable`, which is the point: the fix is a budget that matches
/// real hardware, not a feature that gives up on observing.
#[test]
fn a_seven_second_read_still_settles_on_the_default_budget() {
    block(async {
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                return Some((Duration::ZERO, r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                // The measured cost on the phone that exposed this.
                return Some((Duration::from_millis(6_500), simple_tree("搜索")));
            }
            None
        });

        let started = std::time::Instant::now();
        let (status, json, _) = press_home(wda.url(), "?return=delta").await;
        let elapsed = started.elapsed();

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(
            json["settle"]["captures"], 2,
            "two reads of a slow tree must fit the default budget: {json}"
        );
        assert_eq!(json["settle"]["settled"], true, "{json}");
        assert_eq!(json["settle"]["reason"], "stable");
        assert!(json["snapshot"].is_string());
        // It really did take the reads, rather than answering from nothing.
        assert!(elapsed >= Duration::from_millis(13_000), "{elapsed:?}");
    });
}

/// A slow ACTION must not eat the observation. The two deadlines compose:
/// dispatch is bounded by the action deadline, and the observation's clock
/// starts once the action is confirmed.
#[test]
fn a_slow_action_still_gets_its_own_observation_window() {
    block(async {
        let wda = mock_wda(move |request, _| {
            if is_session(request) {
                return Some((Duration::ZERO, SESSION.to_string()));
            }
            if is_mutation(request) {
                // Most of the action deadline spent before we can even look.
                return Some((Duration::from_millis(11_000), r#"{"value":null}"#.to_string()));
            }
            if is_source(request) {
                return Some((Duration::from_millis(1_000), simple_tree("搜索")));
            }
            None
        });

        let (status, json, _) = press_home(wda.url(), "?return=delta").await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["ok"], true);
        assert_eq!(
            json["settle"]["captures"], 2,
            "the observation window is not what is left of the action's: {json}"
        );
        assert_eq!(json["settle"]["reason"], "stable");
    });
}
