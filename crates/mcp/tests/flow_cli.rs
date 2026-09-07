//! `flow run` through the REAL binary: a child process, its actual stdout, and
//! its actual exit code.
//!
//! Calling `run_command` in-process proves the function works; it does not
//! prove the command does. The thing a nightly canary and a human both depend
//! on is that a FAILED run still prints a complete, parseable result on stdout
//! and still exits non-zero — and that nothing is sent twice while that
//! happens. Every daemon-failure shape gets its own case, because the ways a
//! run can fail to produce a verdict are exactly the ways this used to lose
//! the evidence.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;

const FLOW: &str = r#"{
  "version": 1,
  "name": "Open search",
  "description": "A deterministic read-only navigation example.",
  "steps": [
    {"kind":"shortcut","name":"home"},
    {"kind":"wait_for","expect":{"present":[{"kind":"TextField"}]},"timeout_ms":1000,"poll_ms":100}
  ]
}"#;

const STATUS: &str = r#"{"ok":true,"backend":"direct","drivable":true,"version":"0.6.4"}"#;

/// A scripted daemon. `None` closes the connection without answering, which is
/// how a response goes missing after the request was already sent.
fn mock_daemon(responses: &[(&str, Option<&str>)]) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let responses: Vec<(String, Option<String>)> = responses
        .iter()
        .map(|(status, body)| (status.to_string(), body.map(str::to_string)))
        .collect();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, body) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0_u8; 8_192];
            let Ok(read) = stream.read(&mut buffer) else {
                return;
            };
            let _ = tx.send(String::from_utf8_lossy(&buffer[..read]).to_string());
            let Some(body) = body else {
                continue; // the request was sent; the answer never comes
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}"), rx)
}

struct Run {
    code: Option<i32>,
    stdout: String,
    json: Option<serde_json::Value>,
}

/// Run the real binary with an environment scoped to the child alone — the
/// test process's own environment is never touched, so these cases cannot
/// interfere with each other or with anything else in the suite.
fn flow_run(url: &str, flow: &Path, store: &Path, artifacts: Option<&Path>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_iphone-use-mcp"));
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", store)
        .env("PHONE_REMOTE_URL", url)
        .env("PHONE_REMOTE_TOKEN", "test-token")
        .env("IPHONE_USE_FLOWS_DIR", store)
        .args(["flow", "run", flow.to_str().unwrap(), "--force"]);
    if let Some(artifacts) = artifacts {
        command.args(["--artifacts-dir", artifacts.to_str().unwrap()]);
    }
    let output = command.output().expect("the binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let json = serde_json::from_str(&stdout).ok();
    Run {
        code: output.status.code(),
        stdout,
        json,
    }
}

/// The compat pre-flight: no apps, and no udid to fall back on.
fn compat_preamble() -> Vec<(&'static str, Option<&'static str>)> {
    vec![("200 OK", Some("{}")), ("200 OK", Some(r#"{"ok":true}"#))]
}

/// The one evidence file this run wrote, wherever its private run directory is.
fn read_only_evidence(artifacts: &Path) -> serde_json::Value {
    let run_dirs: Vec<_> = std::fs::read_dir(artifacts)
        .expect("the artifacts directory exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(run_dirs.len(), 1, "{run_dirs:?}");
    let files: Vec<_> = std::fs::read_dir(&run_dirs[0])
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap()
}

fn write_flow(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("flow.json");
    std::fs::write(&path, FLOW).unwrap();
    path
}

#[test]
fn a_409_failure_prints_the_whole_result_and_exits_non_zero() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    let artifacts = home.path().join("runs");
    let failure = r#"{"ok":false,"error":"expectation_timeout","failed_step":1,
        "completed":1,"applied_actions":1,"outcome":"not_sent","retry_safe":false,
        "observation":{"read":true,"sparse":false,"missing_present":[0]}}"#;
    let elements = r#"{"ok":true,"snapshot":"S","elements":[{"index":0,"kind":"TextField","label":"搜索"}]}"#;
    let mut script = compat_preamble();
    script.push(("200 OK", Some(STATUS)));
    script.push(("409 Conflict", Some(failure)));
    script.push(("200 OK", Some(elements)));
    let (url, requests) = mock_daemon(&script);

    let run = flow_run(&url, &flow, home.path(), Some(&artifacts));

    assert_eq!(run.code, Some(1), "a failed run must exit non-zero: {}", run.stdout);
    let json = run.json.as_ref().expect("stdout is parseable JSON");
    assert_eq!(json["error"], "expectation_timeout");
    assert_eq!(json["failed_step"], 1);
    assert_eq!(json["applied_actions"], 1, "what the phone did survives");
    assert_eq!(
        json["retry_safe"], false,
        "an applied action makes a replay unsafe, and that reaches the caller"
    );
    assert_eq!(json["http_status"], 409);
    assert_eq!(json["diagnosis"]["observable"], true, "{json}");
    assert!(json["artifact"].is_string(), "evidence was recorded: {json}");

    let sent: Vec<String> = requests.try_iter().collect();
    let mutations = sent
        .iter()
        .filter(|request| request.starts_with("POST /agent/actions"))
        .count();
    assert_eq!(mutations, 1, "exactly one mutation: {sent:?}");
    let reads = sent
        .iter()
        .filter(|request| request.starts_with("GET /agent/elements"))
        .count();
    assert_eq!(reads, 1, "exactly one diagnostic read: {sent:?}");
}

/// The daemon says no with a 200. A caller reading only the status code would
/// call this a success and refresh a verification date on it.
#[test]
fn a_200_with_ok_false_is_a_failure() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    let refusal = r#"{"ok":false,"error":"device_locked","outcome":"not_sent","retry_safe":true}"#;
    let mut script = compat_preamble();
    script.push(("200 OK", Some(STATUS)));
    script.push(("200 OK", Some(refusal)));
    script.push(("200 OK", Some(r#"{"ok":true,"elements":[]}"#)));
    let (url, _requests) = mock_daemon(&script);

    let run = flow_run(&url, &flow, home.path(), None);

    assert_eq!(run.code, Some(1), "200 + ok:false is not a success: {}", run.stdout);
    let json = run.json.as_ref().expect("stdout is parseable JSON");
    assert_eq!(json["error"], "device_locked");
    assert_eq!(json["http_status"], 200);
}

/// A 2xx whose body is not JSON proves the request was accepted, nothing more.
/// It must read as an unknown outcome that is NOT safe to retry — never as a
/// success, and never as an invented failure with a made-up step.
#[test]
fn an_unreadable_body_is_an_unknown_outcome_that_is_not_retry_safe() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    let mut script = compat_preamble();
    script.push(("200 OK", Some(STATUS)));
    script.push(("200 OK", Some("<html>gateway</html>")));
    script.push(("200 OK", Some(r#"{"ok":true,"elements":[]}"#)));
    let (url, _requests) = mock_daemon(&script);

    let run = flow_run(&url, &flow, home.path(), None);

    assert_eq!(run.code, Some(1), "{}", run.stdout);
    let json = run.json.as_ref().expect("stdout is still parseable JSON");
    assert_eq!(json["outcome"], "unknown");
    assert_eq!(json["error"], "outcome_unknown");
    assert_eq!(json["retry_safe"], false, "an unread answer is never retry-safe");
    assert!(json["failed_step"].is_null(), "no step is invented: {json}");
    assert!(json["applied_actions"].is_null(), "no count is invented: {json}");
}

/// A body that parses but says nothing is not a verdict. `{"unrelated":1}`
/// next to a 500 tells us the request was answered, not what happened to the
/// phone — and inventing a verdict from it is how a run gets recorded as
/// something it was not.
#[test]
fn a_parseable_body_with_no_ok_field_is_an_unknown_outcome() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    let mut script = compat_preamble();
    script.push(("200 OK", Some(STATUS)));
    script.push(("500 Internal Server Error", Some(r#"{"unrelated":1}"#)));
    script.push(("200 OK", Some(r#"{"ok":true,"elements":[]}"#)));
    let (url, _requests) = mock_daemon(&script);

    let run = flow_run(&url, &flow, home.path(), None);

    assert_eq!(run.code, Some(1), "{}", run.stdout);
    let json = run.json.as_ref().expect("stdout is parseable JSON");
    assert_eq!(json["outcome"], "unknown", "{json}");
    assert_eq!(json["error"], "outcome_unknown");
    assert_eq!(json["retry_safe"], false);
    assert_eq!(json["reason"], "no_verdict_in_response");
    assert!(
        json["unrelated"].is_null(),
        "a body that said nothing must not be reported as the result: {json}"
    );
}

/// The other side: an explicit refusal IS a verdict, and its body is kept.
#[test]
fn an_explicit_refusal_is_reported_as_the_daemons_own_result() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    let mut script = compat_preamble();
    script.push(("200 OK", Some(STATUS)));
    script.push((
        "500 Internal Server Error",
        Some(r#"{"ok":false,"error":"wda_source_failed","outcome":"not_sent","retry_safe":true}"#),
    ));
    script.push(("200 OK", Some(r#"{"ok":true,"elements":[]}"#)));
    let (url, _requests) = mock_daemon(&script);

    let run = flow_run(&url, &flow, home.path(), None);

    assert_eq!(run.code, Some(1));
    let json = run.json.as_ref().expect("stdout is parseable JSON");
    assert_eq!(json["error"], "wda_source_failed", "{json}");
    assert_eq!(json["outcome"], "not_sent");
    assert_eq!(json["retry_safe"], true, "the daemon's own judgement is kept");
}

/// The request went out and the answer never came back. We do not know what
/// the phone did — and the one thing that must not happen is the command
/// reporting that the flow never ran, which would read as safe to retry.
#[test]
fn a_lost_response_is_reported_as_unknown_not_as_never_ran() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    let artifacts = home.path().join("runs");
    let mut script = compat_preamble();
    script.push(("200 OK", Some(STATUS)));
    script.push(("200 OK", None)); // connection closed mid-flight
    let (url, requests) = mock_daemon(&script);

    let run = flow_run(&url, &flow, home.path(), Some(&artifacts));

    assert_eq!(run.code, Some(1), "{}", run.stdout);
    let json = run
        .json
        .as_ref()
        .expect("a lost answer still prints a machine-readable result");
    assert_eq!(json["outcome"], "unknown", "{json}");
    assert_eq!(json["error"], "outcome_unknown");
    assert_eq!(
        json["retry_safe"], false,
        "we do not know what the phone did, so a replay is never safe"
    );
    assert_eq!(json["reason"], "transport_error");
    assert!(
        json["http_status"].is_null(),
        "no status was received, so none is invented: {json}"
    );
    assert!(json["failed_step"].is_null(), "no step is invented: {json}");
    assert!(
        json["applied_actions"].is_null(),
        "no count is invented: {json}"
    );

    // The evidence file exists and records the unknown outcome rather than
    // silently omitting the run that we cannot describe.
    let stored = read_only_evidence(&artifacts);
    assert_eq!(stored["result"]["outcome"], "unknown", "{stored}");
    assert_eq!(stored["result"]["retry_safe"], false);

    let sent: Vec<String> = requests.try_iter().collect();
    assert_eq!(
        sent.iter()
            .filter(|request| request.starts_with("POST /agent/actions"))
            .count(),
        1,
        "sent once, and never re-sent after the answer was lost: {sent:?}"
    );
}

/// A daemon that cannot be reached at all is a different fact: nothing was
/// sent, and saying so is safe.
#[test]
fn a_daemon_that_never_answers_the_connection_is_reported_as_not_sent() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    // Bind and immediately drop, so the port is closed: connect fails.
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };

    let run = flow_run(&format!("http://{dead}"), &flow, home.path(), None);

    assert_ne!(run.code, Some(0));
    assert!(
        run.stdout.is_empty() || run.json.is_none(),
        "nothing was dispatched, so there is no result to print: {}",
        run.stdout
    );
}

/// An unusable evidence directory is caught while the phone has not moved.
#[test]
fn an_unwritable_artifacts_directory_stops_before_anything_is_sent() {
    let home = tempfile::tempdir().unwrap();
    let flow = write_flow(home.path());
    let blocker = home.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let (url, requests) = mock_daemon(&compat_preamble());

    let run = flow_run(&url, &flow, home.path(), Some(&blocker.join("runs")));

    assert_ne!(run.code, Some(0));
    let sent: Vec<String> = requests.try_iter().collect();
    assert!(
        !sent.iter().any(|request| request.starts_with("POST /agent/actions")),
        "nothing may be sent when the evidence directory is unusable: {sent:?}"
    );
}
