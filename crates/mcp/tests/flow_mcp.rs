//! `phone_flow_run` through the REAL MCP stdio server: a child process, a real
//! JSON-RPC `tools/call`, and a real tool result.
//!
//! An agent never calls `run_command`; it calls this tool. Proving the CLI
//! reports a failure well says nothing about what the agent sees, and the two
//! used to differ — the MCP path reverse-parsed JSON out of an error string.
//! Driving the actual protocol is the only way to test what an agent gets.
//!
//! Everything here is child-scoped: the flow lives in a temporary registry
//! reached through the child's own environment, so this test cannot disturb
//! (or be disturbed by) anything else in the suite.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

/// A response the script deliberately never sends: the connection closes with
/// the request already on the wire.
const LOST: &str = "\u{0}LOST";

/// A scripted daemon that always terminates: it stops after its script, and
/// stops early if the client goes away. A mock that can block forever turns
/// every mistake in the test into a hung suite.
fn mock_daemon(responses: Vec<(&'static str, &'static str)>) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        for (status, body) in responses {
            let mut stream = loop {
                if std::time::Instant::now() > deadline {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0_u8; 8_192];
            let Ok(read) = stream.read(&mut buffer) else {
                continue;
            };
            let _ = tx.send(String::from_utf8_lossy(&buffer[..read]).to_string());
            if body == LOST {
                continue; // the request was sent; the answer never comes
            }
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

/// A temporary flow registry holding one installable flow at `test/search`.
fn temp_registry(home: &Path) -> String {
    let store = home.join("flows");
    std::fs::create_dir_all(store.join("test")).unwrap();
    std::fs::write(store.join("test").join("search.json"), FLOW).unwrap();
    store.to_string_lossy().to_string()
}

struct McpServer {
    child: Child,
    next_id: u64,
}

impl McpServer {
    fn start(url: &str, store: &str, home: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_iphone-use-mcp"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("PHONE_REMOTE_URL", url)
            .env("PHONE_REMOTE_TOKEN", "test-token")
            .env("IPHONE_USE_FLOWS_DIR", store)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the MCP server starts");
        Self { child, next_id: 1 }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();

        let stdout = self.child.stdout.as_mut().expect("stdout");
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("the server answers");
            assert!(read > 0, "the server closed stdout while answering {method}");
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return value;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","method":method})).unwrap();
        stdin.flush().unwrap();
    }

    fn initialize(&mut self) {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "flow-mcp-test", "version": "0"}
            }),
        );
        self.notify("notifications/initialized");
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The agent-facing path on a real failure: the tool must hand back the
/// daemon's structured result — including what the phone actually did and the
/// diagnosis — not prose with the JSON melted into it.
#[test]
fn phone_flow_run_hands_an_agent_the_structured_failure_and_its_diagnosis() {
    let home = tempfile::tempdir().unwrap();
    let store = temp_registry(home.path());
    // A real failure: one step applied, then the expectation never met. The
    // daemon's own contract — an applied action makes a replay unsafe.
    let failure = r#"{"ok":false,"error":"expectation_timeout","failed_step":1,
        "completed":1,"applied_actions":1,"outcome":"not_sent","retry_safe":false,
        "observation":{"read":true,"sparse":false,"missing_present":[0]}}"#;
    let (url, requests) = mock_daemon(vec![
        // compat pre-flight: no apps, and no udid to fall back on
        ("200 OK", "{}"),
        ("200 OK", r#"{"ok":true}"#),
        // the run's own pre-flight
        ("200 OK", r#"{"ok":true,"backend":"direct","drivable":true,"version":"0.6.4"}"#),
        ("409 Conflict", failure),
        // the single diagnostic read
        ("200 OK", r#"{"ok":true,"snapshot":"S","elements":[{"index":0,"kind":"TextField","label":"搜索"}]}"#),
    ]);

    let mut server = McpServer::start(&url, &store, home.path());
    server.initialize();
    let response = server.request(
        "tools/call",
        serde_json::json!({
            "name": "phone_flow_run",
            "arguments": {"id": "test/search", "force": true}
        }),
    );

    let result = &response["result"];
    assert_eq!(result["isError"], true, "a failed flow is a tool error: {response}");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("the tool returns text content");
    let summary: serde_json::Value =
        serde_json::from_str(text).expect("that text is the structured result, not prose");

    assert_eq!(summary["flow"], "test/search");
    assert_eq!(summary["result"]["error"], "expectation_timeout");
    assert_eq!(summary["result"]["failed_step"], 1, "the 0-based index survives");
    assert_eq!(
        summary["result"]["applied_actions"], 1,
        "what the phone actually did reaches the agent: {summary}"
    );
    assert_eq!(summary["result"]["retry_safe"], false);
    assert_eq!(summary["result"]["http_status"], 409);
    assert_eq!(summary["result"]["diagnosis"]["observable"], true, "{summary}");
    assert_eq!(summary["result"]["diagnosis"]["failed_step"], 1);

    let sent: Vec<String> = requests.try_iter().collect();
    assert_eq!(
        sent.iter()
            .filter(|request| request.starts_with("POST /agent/actions"))
            .count(),
        1,
        "exactly one mutation: {sent:?}"
    );
    assert_eq!(
        sent.iter()
            .filter(|request| request.starts_with("GET /agent/elements"))
            .count(),
        1,
        "exactly one diagnostic read: {sent:?}"
    );
    // The pre-flight status is reused rather than re-fetched after the failure.
    assert_eq!(
        sent.iter()
            .filter(|request| request.starts_with("GET /agent/status"))
            .count(),
        2,
        "one compat fallback probe and one run pre-flight, no third: {sent:?}"
    );
}

/// The same rule at the agent's own entry point: an answer that never arrived
/// is an UNKNOWN outcome, not a flow that never ran. An agent told "it did not
/// run" would reasonably try again — on a phone that may have already acted.
#[test]
fn a_lost_answer_reaches_the_agent_as_unknown_and_not_retry_safe() {
    let home = tempfile::tempdir().unwrap();
    let store = temp_registry(home.path());
    let (url, requests) = mock_daemon(vec![
        ("200 OK", "{}"),
        ("200 OK", r#"{"ok":true}"#),
        ("200 OK", r#"{"ok":true,"backend":"direct","drivable":true,"version":"0.6.4"}"#),
        ("200 OK", LOST),
    ]);

    let mut server = McpServer::start(&url, &store, home.path());
    server.initialize();
    let response = server.request(
        "tools/call",
        serde_json::json!({
            "name": "phone_flow_run",
            "arguments": {"id": "test/search", "force": true}
        }),
    );

    let result = &response["result"];
    assert_eq!(result["isError"], true, "{response}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    let summary: serde_json::Value =
        serde_json::from_str(text).expect("structured result, not prose");

    assert_eq!(summary["result"]["outcome"], "unknown", "{summary}");
    assert_eq!(summary["result"]["error"], "outcome_unknown");
    assert_eq!(summary["result"]["retry_safe"], false);
    assert_eq!(summary["result"]["reason"], "transport_error");
    assert!(summary["result"]["failed_step"].is_null(), "{summary}");
    assert!(summary["result"]["applied_actions"].is_null(), "{summary}");
    assert!(
        !text.contains("did not run"),
        "an unknown outcome must not be described as never having run: {text}"
    );

    let sent: Vec<String> = requests.try_iter().collect();
    assert_eq!(
        sent.iter()
            .filter(|request| request.starts_with("POST /agent/actions"))
            .count(),
        1,
        "{sent:?}"
    );
}

/// The same rule at the agent's entry point: a parseable body with no verdict
/// must not be handed to an agent as though it were one.
#[test]
fn a_body_without_a_verdict_reaches_the_agent_as_unknown() {
    let home = tempfile::tempdir().unwrap();
    let store = temp_registry(home.path());
    let (url, _requests) = mock_daemon(vec![
        ("200 OK", "{}"),
        ("200 OK", r#"{"ok":true}"#),
        ("200 OK", r#"{"ok":true,"backend":"direct","drivable":true,"version":"0.6.4"}"#),
        ("500 Internal Server Error", r#"{"unrelated":1}"#),
    ]);

    let mut server = McpServer::start(&url, &store, home.path());
    server.initialize();
    let response = server.request(
        "tools/call",
        serde_json::json!({
            "name": "phone_flow_run",
            "arguments": {"id": "test/search", "force": true}
        }),
    );

    let result = &response["result"];
    assert_eq!(result["isError"], true, "{response}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    let summary: serde_json::Value =
        serde_json::from_str(text).expect("structured result, not prose");

    assert_eq!(summary["result"]["outcome"], "unknown", "{summary}");
    assert_eq!(summary["result"]["retry_safe"], false);
    assert_eq!(summary["result"]["reason"], "no_verdict_in_response");
    assert!(summary["result"]["unrelated"].is_null(), "{summary}");
}

/// The batch entry point must carry a FAILURE's evidence, not a summary of it.
///
/// A 24-step batch with per-step observations is comfortably larger than any
/// preview budget, and the fields that matter most — the failing step's
/// observation — sit at the END. Flattening that into bounded error prose is
/// how a caller loses exactly the part that says whether anyone could see the
/// screen.
#[test]
fn a_failing_batch_keeps_its_evidence_over_stdio() {
    let home = tempfile::tempdir().unwrap();
    let store = temp_registry(home.path());

    // A realistic failure body: big, with the decisive evidence last.
    let mut steps = String::new();
    for index in 0..24 {
        steps.push_str(&format!(
            r#"{{"index":{index},"kind":"action","ok":true,"filler":"{}"}},"#,
            "x".repeat(400)
        ));
    }
    let failure = format!(
        r#"{{"ok":false,"error":"expectation_timeout","failed_step":24,"completed":24,
            "applied_actions":24,"outcome":"not_sent","retry_safe":false,
            "steps":[{steps}{{"index":24,"kind":"wait_for","ok":false}}],
            "observation":{{"read":false,"reads":0,"hint":"no readable element tree was obtained"}}}}"#
    );
    assert!(failure.len() > 8 * 1024, "the body must exceed any preview budget");
    let failure: &'static str = Box::leak(failure.into_boxed_str());

    let (url, requests) = mock_daemon(vec![("409 Conflict", failure)]);
    let mut server = McpServer::start(&url, &store, home.path());
    server.initialize();
    let response = server.request(
        "tools/call",
        serde_json::json!({
            "name": "phone_run_steps",
            "arguments": {"steps": [{"kind": "shortcut", "name": "home"}]}
        }),
    );

    let result = &response["result"];
    assert_eq!(result["isError"], true, "{response}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["failed_step"], 24, "{structured}");
    assert_eq!(structured["applied_actions"], 24);
    assert_eq!(structured["retry_safe"], false);
    // The tail survived: this is the whole point.
    assert_eq!(
        structured["observation"]["read"], false,
        "the failing step's observation is at the end of a large body: {structured}"
    );
    assert_eq!(structured["steps"].as_array().map(Vec::len), Some(25));

    let sent: Vec<String> = requests.try_iter().collect();
    assert_eq!(
        sent.iter()
            .filter(|request| request.starts_with("POST /agent/actions"))
            .count(),
        1,
        "{sent:?}"
    );
}

/// A batch whose answer is lost is unknown — and sent exactly once.
#[test]
fn a_dropped_batch_answer_is_unknown_and_never_resent() {
    let home = tempfile::tempdir().unwrap();
    let store = temp_registry(home.path());
    let (url, requests) = mock_daemon(vec![("200 OK", LOST)]);

    let mut server = McpServer::start(&url, &store, home.path());
    server.initialize();
    let response = server.request(
        "tools/call",
        serde_json::json!({
            "name": "phone_run_steps",
            "arguments": {"steps": [{"kind": "shortcut", "name": "home"}]}
        }),
    );

    let result = &response["result"];
    assert_eq!(result["isError"], true, "{response}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{structured}");
    assert_eq!(structured["retry_safe"], false);
    assert_eq!(structured["reason"], "transport_error");

    let sent: Vec<String> = requests.try_iter().collect();
    assert_eq!(
        sent.iter()
            .filter(|request| request.starts_with("POST /agent/actions"))
            .count(),
        1,
        "sent once, never resent after the answer was lost: {sent:?}"
    );
}

/// A Mirror-era plain-text `ok` acknowledges ONE action. It carries no
/// per-step outcome, so it can never stand in for a batch result.
#[test]
fn a_legacy_text_ack_is_not_a_batch_success() {
    let home = tempfile::tempdir().unwrap();
    let store = temp_registry(home.path());
    let (url, _requests) = mock_daemon(vec![("200 OK", "ok")]);

    let mut server = McpServer::start(&url, &store, home.path());
    server.initialize();
    let response = server.request(
        "tools/call",
        serde_json::json!({
            "name": "phone_run_steps",
            "arguments": {"steps": [{"kind": "shortcut", "name": "home"}]}
        }),
    );

    let result = &response["result"];
    assert_eq!(result["isError"], true, "a batch needs a batch result: {response}");
    assert_eq!(result["structuredContent"]["outcome"], "unknown");
    assert_eq!(result["structuredContent"]["retry_safe"], false);
}
