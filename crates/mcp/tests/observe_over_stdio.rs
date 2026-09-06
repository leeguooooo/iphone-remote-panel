//! Real MCP calls over stdio against a scripted daemon.
//!
//! In-process tests can call a tool function and inspect the value it
//! returns, but they never serialise it. `structured_content` only matters if
//! it survives the JSON-RPC encoding an actual client reads, so these tests
//! spawn the built binary, speak the protocol to it, and assert on the wire
//! format. The daemon URL is passed through the child's environment, so
//! nothing here touches process-global state in the test binary.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A daemon that answers every request with one canned response and can
/// always be stopped.
///
/// A blocking `accept` loop with no stop signal turns one mis-written test
/// into a suite that hangs forever, so this uses a non-blocking accept, a stop
/// flag, a hard deadline, and a join in `Drop`.
struct ScriptedDaemon {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedDaemon {
    fn start(status: &'static str, body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !thread_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        let mut buffer = [0_u8; 8_192];
                        let _ = stream.read(&mut buffer);
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            url: format!("http://{address}"),
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The binary under test, built alongside this integration test.
fn binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("iphone-use-mcp");
    assert!(path.exists(), "built binary missing at {}", path.display());
    path
}

struct McpChild {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl McpChild {
    fn start(daemon_url: &str) -> Self {
        let mut child = Command::new(binary())
            .env("PHONE_REMOTE_URL", daemon_url)
            .env_remove("PHONE_REMOTE_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the MCP server");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = Self { child, reader };
        session.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "observe-test", "version": "0" }
            }
        }));
        let _ = session.read_reply(1);
        session.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        session
    }

    fn send(&mut self, value: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{value}").expect("write request");
        stdin.flush().expect("flush");
    }

    /// Read until the reply with this id arrives (notifications are skipped).
    fn read_reply(&mut self, id: u64) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let mut line = String::new();
            if self.reader.read_line(&mut line).expect("read stdout") == 0 {
                panic!("MCP server closed stdout before replying to {id}");
            }
            if line.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return value;
            }
        }
        panic!("no reply to request {id} within the deadline");
    }

    fn call_tool(&mut self, id: u64, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }));
        self.read_reply(id)
    }
}

impl Drop for McpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The observation has to reach the client whole. A 64 KiB body is well past
/// any display-sized cap, and everything a caller acts on sits at its end.
#[test]
fn an_observed_tap_puts_the_whole_observation_on_the_wire() {
    let filler = "x".repeat(64 * 1024);
    let body = format!(
        r#"{{"ok":true,"transport":"wda","tree":"{filler}","snapshot":"snap-1","settle":{{"settled":true,"reason":"stable","captures":2}},"delta":{{"added":["搜索"]}}}}"#
    );
    let daemon = ScriptedDaemon::start("200 OK", body.into_bytes());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(
        2,
        "phone_tap",
        serde_json::json!({ "x": 0.5, "y": 0.5, "observe": true }),
    );

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert!(
        structured.is_object(),
        "no structuredContent on the wire: {reply}"
    );
    assert_eq!(structured["settle"]["reason"], "stable");
    assert_eq!(structured["settle"]["captures"], 2);
    assert_eq!(structured["delta"]["added"][0], "搜索");
    assert_eq!(structured["snapshot"], "snap-1");
}

/// Omitting `observe` must behave exactly as before: the short result callers
/// already parse, and no structured payload appearing out of nowhere.
#[test]
fn an_unobserved_tap_is_unchanged_on_the_wire() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true,"transport":"wda"}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    assert_eq!(result["content"][0]["text"], "ok");
}

/// A 2xx the client cannot read is not a completed action, and the reply must
/// say so in a form a program can branch on.
#[test]
fn an_unreadable_success_reaches_the_client_as_unknown() {
    let daemon = ScriptedDaemon::start("200 OK", b"<html>proxy error</html>".to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{reply}");
    assert_eq!(structured["retry_safe"], false, "{reply}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("Do NOT resend"), "{text}");
}

/// A refusal keeps the fields a caller decides on.
#[test]
fn a_refusal_keeps_its_structure_on_the_wire() {
    let daemon = ScriptedDaemon::start(
        "409 Conflict",
        br#"{"ok":false,"error":"phone_owned","outcome":"not_sent","retry_safe":true}"#.to_vec(),
    );
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    assert_eq!(result["structuredContent"]["error"], "phone_owned");
    assert_eq!(result["structuredContent"]["outcome"], "not_sent");
    assert_eq!(result["structuredContent"]["retry_safe"], true);
}

/// A daemon that accepts the connection and then dies mid-response. The
/// request may already have reached the phone, so the only honest answer is
/// unknown — and above all not "not sent".
#[test]
fn a_broken_connection_is_unknown_not_a_safe_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let killer = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0_u8; 4_096];
            let _ = stream.read(&mut buffer);
            // Promise a body, then hang up without sending it.
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 4096

{",
            );
        }
    });
    let mut mcp = McpChild::start(&format!("http://{address}"));

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));
    let _ = killer.join();

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{reply}");
    assert_eq!(
        structured["retry_safe"], false,
        "a broken connection authorised a resend: {reply}"
    );
    assert_ne!(
        structured["outcome"], "not_sent",
        "a broken connection claimed nothing was sent: {reply}"
    );
}

/// A 500 with an HTML error page says nothing about the phone either.
#[test]
fn a_non_json_server_error_is_unknown_not_not_sent() {
    let daemon = ScriptedDaemon::start(
        "500 Internal Server Error",
        b"<html><body>gateway exploded</body></html>".to_vec(),
    );
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "unknown", "{reply}");
    assert_eq!(structured["retry_safe"], false, "{reply}");
}

/// The Mirror backend answers a dispatched CGEvent with `200 ok` in plain
/// text. That must stay a success — the strict-JSON rule would otherwise
/// report every legitimate Mirror action as an uncertain outcome — but it must
/// be labelled as an acknowledgement, not as a verified result.
#[test]
fn the_legacy_plain_text_ack_is_success_but_not_verified() {
    let daemon = ScriptedDaemon::start("200 OK", b"ok".to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "a legacy ack was reported as failure: {reply}");
    assert_eq!(result["content"][0]["text"], "ok");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "acknowledged", "{reply}");
    assert_eq!(
        structured["verified"], false,
        "an unverified ack claimed verification: {reply}"
    );
    assert_eq!(structured["protocol"], "legacy_text_ack");
}

/// Asking that backend to observe must not fabricate an observation, and must
/// not turn the missing observation into a reason to send the action twice.
#[test]
fn a_legacy_ack_under_observe_reports_no_observation_rather_than_inventing_one() {
    let daemon = ScriptedDaemon::start("200 OK", b"ok".to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(
        2,
        "phone_tap",
        serde_json::json!({ "x": 0.5, "y": 0.5, "observe": true }),
    );

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["observation"], "unavailable", "{reply}");
    assert_eq!(structured["verified"], false);
    assert!(
        structured.get("settle").is_none() && structured.get("delta").is_none(),
        "an observation was invented for a backend that cannot produce one: {reply}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("do NOT resend"), "{text}");
}

/// The recognition is exact. A 2xx whose body merely CONTAINS "ok", or is any
/// other plain text, stays unknown — otherwise an error page mentioning the
/// word would be read as a dispatched action.
#[test]
fn only_the_exact_legacy_ack_is_accepted() {
    for body in [
        b"ok then".to_vec(),
        b"not ok".to_vec(),
        b"<html>ok</html>".to_vec(),
    ] {
        let daemon = ScriptedDaemon::start("200 OK", body.clone());
        let mut mcp = McpChild::start(&daemon.url);

        let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

        let result = &reply["result"];
        let shape = String::from_utf8_lossy(&body).to_string();
        assert_eq!(
            result["isError"], true,
            "{shape:?} was accepted as a legacy ack: {reply}"
        );
        assert_eq!(result["structuredContent"]["outcome"], "unknown", "{shape:?}");
    }
}

/// Parsing is not the same as being told. A body that happens to be valid
/// JSON but is not the daemon saying `ok:false` leaves the outcome unknown —
/// including the shapes that cannot even be attached as structured content.
#[test]
fn a_parseable_but_meaningless_failure_is_still_unknown() {
    for body in [
        br#"[]"#.to_vec(),
        br#"42"#.to_vec(),
        br#"{"unrelated":1}"#.to_vec(),
    ] {
        let daemon = ScriptedDaemon::start("500 Internal Server Error", body.clone());
        let mut mcp = McpChild::start(&daemon.url);

        let reply = mcp.call_tool(2, "phone_tap", serde_json::json!({ "x": 0.5, "y": 0.5 }));

        let result = &reply["result"];
        let shape = String::from_utf8_lossy(&body).to_string();
        assert_eq!(result["isError"], true, "{shape}: {reply}");
        let structured = &result["structuredContent"];
        assert_eq!(
            structured["outcome"], "unknown",
            "{shape} was read as a refusal: {reply}"
        );
        assert_eq!(
            structured["retry_safe"], false,
            "{shape} authorised a resend: {reply}"
        );
    }
}

/// The one case that IS provably safe to retry: refused locally, before
/// anything was sent.
#[test]
fn a_missing_snapshot_is_refused_locally_and_is_retry_safe() {
    let daemon = ScriptedDaemon::start("200 OK", br#"{"ok":true}"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(
        2,
        "phone_tap_element",
        serde_json::json!({ "element": 0, "snapshot": "" }),
    );

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["outcome"], "not_sent", "{reply}");
    assert_eq!(structured["retry_safe"], true, "{reply}");
}

/// Structured content is an object in MCP. A body that parsed into an array
/// must not be attached, and must not be handed back as capability data.
#[test]
fn a_non_object_body_is_not_offered_as_structured_content() {
    let daemon = ScriptedDaemon::start("200 OK", br#"[1,2,3]"#.to_vec());
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_capabilities", serde_json::json!({}));

    let result = &reply["result"];
    assert_eq!(result["isError"], true, "{reply}");
    assert!(
        result.get("structuredContent").is_none()
            || result["structuredContent"].is_null(),
        "a JSON array was offered as structured content: {reply}"
    );
}

/// Discovery is read-only and structured.
#[test]
fn capabilities_reach_the_client_as_structured_content() {
    let daemon = ScriptedDaemon::start(
        "200 OK",
        br#"{"ok":true,"backend":"direct","scope":"ui_control","supported":{"element_tree":true},"available":{"ok":false,"blocked_by":"released"}}"#.to_vec(),
    );
    let mut mcp = McpChild::start(&daemon.url);

    let reply = mcp.call_tool(2, "phone_capabilities", serde_json::json!({}));

    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["supported"]["element_tree"], true);
    assert_eq!(structured["available"]["blocked_by"], "released");
    assert_eq!(structured["scope"], "ui_control");
}
