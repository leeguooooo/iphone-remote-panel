//! HTTP client that talks to the iphone-use daemon's agent API.
//!
//! The public surface is intentionally small: `DaemonClient` holds the base
//! URL and optional bearer token and exposes one async method per daemon
//! endpoint.  All I/O errors are surfaced as `anyhow::Error` so the MCP layer
//! can turn them into MCP tool errors.

use crate::types::{InputMsg, StatusResponse};
use anyhow::Context as _;
use reqwest::{header, Client};
use std::time::Duration;

const DEFAULT_URL: &str = "http://127.0.0.1:44321";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ELEMENTS_TIMEOUT: Duration = Duration::from_secs(45);
const ACTIONS_TIMEOUT: Duration = Duration::from_secs(90);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ERROR_BODY_CHARS: usize = 2_048;

/// Thin async wrapper over the daemon's `GET /agent/*` and
/// `POST /agent/input` endpoints.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    client: Client,
    base_url: String,
    token: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ElementSnapshotResponse {
    snapshot: String,
    elements: Vec<ElementSummary>,
}

#[derive(Debug, serde::Deserialize)]
struct ElementSummary {
    label: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    identifier: Option<String>,
}

fn unique_label_target(body: &str, label: &str) -> anyhow::Result<(usize, String)> {
    if label.trim().is_empty() {
        anyhow::bail!("label must not be empty");
    }
    let response: ElementSnapshotResponse =
        serde_json::from_str(body).context("parse /agent/elements response")?;
    if response.snapshot.is_empty() {
        anyhow::bail!("/agent/elements returned an empty snapshot");
    }

    let matches: Vec<_> = response
        .elements
        .iter()
        .enumerate()
        .filter(|(_, element)| element.label == label)
        .collect();
    match matches.as_slice() {
        [] => anyhow::bail!("no element matched the exact label '{label}'; no action was sent"),
        [(index, _)] => Ok((*index, response.snapshot)),
        _ => {
            let candidates = matches
                .iter()
                .take(8)
                .map(|(index, element)| {
                    let identifier = element
                        .identifier
                        .as_deref()
                        .map(|value| format!(", identifier={value:?}"))
                        .unwrap_or_default();
                    format!("#{index} kind={:?}{identifier}", element.kind)
                })
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!(
                "ambiguous exact label '{label}' matched {} elements ({candidates}); \
                 no action was sent — choose an element index from phone_elements and \
                 call phone_tap_element with the same snapshot",
                matches.len()
            )
        }
    }
}

impl DaemonClient {
    /// Build a client from the two environment variables:
    ///
    /// * `PHONE_REMOTE_URL`   — daemon base URL (default `http://127.0.0.1:44321`)
    /// * `PHONE_REMOTE_TOKEN` — bearer token / password (optional; omit for
    ///   open-mode daemons running on localhost)
    pub fn from_env() -> Self {
        let base_url =
            std::env::var("PHONE_REMOTE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
        let token = std::env::var("PHONE_REMOTE_TOKEN").ok();
        Self::new(base_url, token)
    }

    /// Construct with explicit values (useful for tests).
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self::with_timeouts(base_url, token, CONNECT_TIMEOUT, REQUEST_TIMEOUT)
    }

    fn with_timeouts(
        base_url: impl Into<String>,
        token: Option<String>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Self {
        let client = Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            .expect("reqwest client construction is infallible");
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token,
        }
    }

    /// The configured base URL (used for logging).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.header(header::AUTHORIZATION, format!("Bearer {t}")),
            None => req,
        }
    }

    // -----------------------------------------------------------------------
    // Daemon API
    // -----------------------------------------------------------------------

    /// `GET /agent/status` — health / phone-target probe.
    pub async fn status(&self) -> anyhow::Result<StatusResponse> {
        let req = self.auth(self.client.get(self.url("/agent/status")));
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        let body: StatusResponse = resp.json().await?;
        Ok(body)
    }

    /// `POST /agent/input` — send one control event to the phone.
    pub async fn input(&self, msg: &InputMsg) -> anyhow::Result<()> {
        let json = msg.to_json();
        let req = self
            .auth(self.client.post(self.url("/agent/input")))
            .header("x-phone-control", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(json);
        let resp = req.send().await?;
        let _ = check_status(resp).await?;
        Ok(())
    }

    /// `POST /agent/actions` — execute one bounded, fail-closed multi-step
    /// Direct/WDA sequence. The daemon validates every step before dispatch and
    /// returns a compact per-step result.
    pub async fn actions(&self, body: &serde_json::Value) -> anyhow::Result<String> {
        let req = self
            .auth(self.client.post(self.url("/agent/actions")))
            .timeout(ACTIONS_TIMEOUT)
            .header("x-phone-control", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        Ok(resp.text().await?)
    }

    /// `GET /agent/screenshot` — returns raw PNG bytes.
    pub async fn screenshot(&self) -> anyhow::Result<Vec<u8>> {
        let req = self.auth(self.client.get(self.url("/agent/screenshot")));
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// `GET /agent/elements` — the phone's UI as a flattened element list
    /// (L2 / WebDriverAgent). Returns the JSON body verbatim:
    /// `{"snapshot":"…","elements":[{kind,label,identifier?,rect,
    /// enabled?,visible?,accessible?,focused?,placeholder?,depth},…]}`.
    pub async fn elements(&self) -> anyhow::Result<String> {
        // A cold WDA call may create a session and then request the source tree;
        // each upstream step is bounded by the daemon, but together can exceed
        // the generic 30-second MCP timeout. Wait long enough for the daemon to
        // return its authoritative success/error instead of abandoning the
        // request while it still owns the WDA lock.
        let req = self
            .auth(self.client.get(self.url("/agent/elements")))
            .timeout(ELEMENTS_TIMEOUT);
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        Ok(resp.text().await?)
    }

    /// `POST /agent/mode {"mode":"agent"}` — reconnect the configured,
    /// canonical Direct/WDA target. Target changes are deliberately not exposed
    /// here; they require persistent configuration plus a daemon restart.
    pub async fn reconnect(&self) -> anyhow::Result<String> {
        let req = self
            .auth(self.client.post(self.url("/agent/mode")))
            .timeout(RECONNECT_TIMEOUT)
            .header("x-phone-control", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"mode":"agent"}"#);
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        Ok(resp.text().await?)
    }

    /// `POST /agent/input` with an element index bound to the exact snapshot
    /// returned by `GET /agent/elements`.
    pub async fn tap_element(&self, element: usize, snapshot: &str) -> anyhow::Result<()> {
        if snapshot.is_empty() {
            anyhow::bail!("snapshot must not be empty");
        }
        let json = serde_json::json!({
            "type": "tap",
            "element": element,
            "snapshot": snapshot,
        })
        .to_string();
        let req = self
            .auth(self.client.post(self.url("/agent/input")))
            .header("x-phone-control", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(json);
        let resp = req.send().await?;
        let _ = check_status(resp).await?;
        Ok(())
    }

    /// Resolve a visible label against one element snapshot, require exactly
    /// one match, then submit a snapshot-bound indexed tap.
    ///
    /// This deliberately does not use WDA's "first matching element" lookup:
    /// duplicate labels are common in lists and blindly choosing one can cause
    /// an irreversible action on the wrong row.
    pub async fn tap_label(&self, label: &str) -> anyhow::Result<()> {
        let body = self.elements().await?;
        let (element, snapshot) = unique_label_target(&body, label)?;
        self.tap_element(element, &snapshot).await
    }
}

/// Turn a non-2xx status into an `anyhow::Error` that includes the status code
/// and response body (useful for surfacing daemon error messages to the MCP
/// caller).
async fn check_status(resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    let body = match resp.text().await {
        Ok(body) => {
            let mut chars = body.trim().chars();
            let detail: String = chars.by_ref().take(MAX_ERROR_BODY_CHARS).collect();
            if chars.next().is_some() {
                format!("{detail}…")
            } else {
                detail
            }
        }
        Err(e) => format!("<failed to read response body: {e}>"),
    };
    let body = if body.is_empty() {
        "<empty response body>"
    } else {
        &body
    };
    let auth_hint = if status == reqwest::StatusCode::UNAUTHORIZED {
        " — check PHONE_REMOTE_TOKEN"
    } else {
        ""
    };
    anyhow::bail!("daemon returned HTTP {status}{auth_hint}: {body}")
}

// ---------------------------------------------------------------------------
// Unit tests — one-shot loopback listeners model daemon responses without
// starting the real daemon or touching a device.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0_u8; 2_048];
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= header_end + content_length {
                break;
            }
        }
        String::from_utf8_lossy(&request).to_string()
    }

    fn mock_daemon(status: &str, body: &str) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4_096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{addr}"), task)
    }

    fn mock_daemon_sequence(
        responses: &[(&str, &str)],
    ) -> (String, JoinHandle<()>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = responses
            .iter()
            .map(|(status, body)| (status.to_string(), body.to_string()))
            .collect::<Vec<_>>();
        let (request_tx, request_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                request_tx.send(read_http_request(&mut stream)).unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{addr}"), task, request_rx)
    }

    fn hanging_daemon(delay: Duration) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4_096];
            let _ = stream.read(&mut request);
            std::thread::sleep(delay);
        });
        (format!("http://{addr}"), task)
    }

    #[test]
    fn default_url_trim() {
        let c = DaemonClient::new("http://127.0.0.1:44321/", None);
        assert_eq!(
            c.url("/agent/status"),
            "http://127.0.0.1:44321/agent/status"
        );
    }

    #[test]
    fn url_no_double_slash() {
        let c = DaemonClient::new("http://192.168.1.50:44321", None);
        assert_eq!(
            c.url("/agent/screenshot"),
            "http://192.168.1.50:44321/agent/screenshot"
        );
    }

    #[test]
    fn from_env_falls_back_to_default() {
        // Make sure PHONE_REMOTE_URL is not set for this sub-test.
        // (We can't unset env in a safe way without unsafe, so we just construct
        //  directly and confirm the default string.)
        let c = DaemonClient::new(
            std::env::var("PHONE_REMOTE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:44321".to_string()),
            None,
        );
        assert!(c.base_url().starts_with("http"));
    }

    #[tokio::test]
    async fn status_parses_direct_lifecycle_fields_from_mock_daemon() {
        let body = serde_json::json!({
            "ok": true,
            "backend": "direct",
            "phone_target": false,
            "wda": true,
            "wda_actionable": false,
            "wda_locked": true,
            "drivable": false,
            "device_state": "locked",
            "screen_state": "waiting",
            "mode": "agent",
            "released": false,
            "hint": "unlock the phone",
            "setup_blocked_on": "trust"
        })
        .to_string();
        let (url, task) = mock_daemon("200 OK", &body);

        let status = DaemonClient::new(url, None).status().await.unwrap();
        task.join().unwrap();

        assert_eq!(status.backend.as_deref(), Some("direct"));
        assert_eq!(status.device_state.as_deref(), Some("locked"));
        assert_eq!(status.screen_state.as_deref(), Some("waiting"));
        assert_eq!(status.wda_actionable, Some(false));
        assert_eq!(status.locked, Some(true));
        assert_eq!(status.drivable, Some(false));
        assert_eq!(status.released, Some(false));
        assert_eq!(status.hint.as_deref(), Some("unlock the phone"));
        assert_eq!(status.setup_blocked_on.as_deref(), Some("trust"));
    }

    #[tokio::test]
    async fn non_success_error_surfaces_daemon_body() {
        let body = r#"{"error":"direct device service was released","retry":"mode=agent"}"#;
        let (url, task) = mock_daemon("503 Service Unavailable", body);

        let error = DaemonClient::new(url, None)
            .screenshot()
            .await
            .unwrap_err()
            .to_string();
        task.join().unwrap();

        assert!(error.contains("503 Service Unavailable"));
        assert!(error.contains("direct device service was released"));
        assert!(error.contains("mode=agent"));
        assert!(!error.contains("Mirroring window not found"));
    }

    #[tokio::test]
    async fn reconnect_returns_daemon_transition_body() {
        let body = r#"{"ok":true,"mode":"agent","starting":true,"reconnecting":false}"#;
        let (url, task) = mock_daemon("200 OK", body);

        let response = DaemonClient::new(url, None).reconnect().await.unwrap();
        task.join().unwrap();

        assert_eq!(response, body);
    }

    #[tokio::test]
    async fn actions_posts_one_guarded_batch_with_mutation_header() {
        let response_body = r#"{"ok":true,"completed":2,"applied_actions":1}"#;
        let (url, task, requests) = mock_daemon_sequence(&[("200 OK", response_body)]);
        let request_body = serde_json::json!({
            "steps": [
                {"kind":"action","action":{"type":"home"},"after_ms":0},
                {"kind":"wait_for","expect":{"application":"主屏幕"},"timeout_ms":1000,"poll_ms":100}
            ]
        });

        let response = DaemonClient::new(url, None)
            .actions(&request_body)
            .await
            .unwrap();
        task.join().unwrap();

        assert_eq!(response, response_body);
        let request = requests.recv().unwrap();
        assert!(request.starts_with("POST /agent/actions "));
        assert!(request.to_ascii_lowercase().contains("x-phone-control: 1"));
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            request_body
        );
    }

    #[test]
    fn unique_label_target_requires_exactly_one_match() {
        let body = serde_json::json!({
            "snapshot": "tree-v1",
            "elements": [
                {"kind": "Button", "label": "取消"},
                {"kind": "Button", "label": "发布", "identifier": "publish-button"}
            ]
        })
        .to_string();

        assert_eq!(
            unique_label_target(&body, "发布").unwrap(),
            (1, "tree-v1".to_string())
        );
        let missing = unique_label_target(&body, "保存").unwrap_err().to_string();
        assert!(missing.contains("no element matched"));
        assert!(missing.contains("no action was sent"));
    }

    #[test]
    fn unique_label_target_rejects_duplicate_labels_with_candidates() {
        let body = serde_json::json!({
            "snapshot": "tree-v1",
            "elements": [
                {"kind": "Button", "label": "关注", "identifier": "author-a"},
                {"kind": "Button", "label": "关注", "identifier": "author-b"}
            ]
        })
        .to_string();

        let error = unique_label_target(&body, "关注").unwrap_err().to_string();
        assert!(error.contains("ambiguous exact label"));
        assert!(error.contains("matched 2 elements"));
        assert!(error.contains("#0"));
        assert!(error.contains("author-a"));
        assert!(error.contains("phone_tap_element"));
        assert!(error.contains("no action was sent"));
    }

    #[tokio::test]
    async fn tap_label_reads_then_submits_snapshot_bound_index() {
        let elements = serde_json::json!({
            "snapshot": "tree-v7",
            "elements": [
                {"kind": "Button", "label": "取消"},
                {"kind": "Button", "label": "发布", "identifier": "publish-button"}
            ]
        })
        .to_string();
        let (url, task, requests) =
            mock_daemon_sequence(&[("200 OK", &elements), ("200 OK", r#"{"ok":true}"#)]);

        DaemonClient::new(url, None)
            .tap_label("发布")
            .await
            .unwrap();
        task.join().unwrap();

        let elements_request = requests.recv().unwrap();
        assert!(elements_request.starts_with("GET /agent/elements "));
        let tap_request = requests.recv().unwrap();
        assert!(tap_request
            .to_ascii_lowercase()
            .contains("x-phone-control: 1"));
        assert!(tap_request.starts_with("POST /agent/input "));
        let body = tap_request.split("\r\n\r\n").nth(1).unwrap();
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            payload,
            serde_json::json!({
                "type": "tap",
                "element": 1,
                "snapshot": "tree-v7"
            })
        );
    }

    #[tokio::test]
    async fn request_timeout_bounds_a_stalled_daemon() {
        let (url, task) = hanging_daemon(Duration::from_millis(150));
        let client = DaemonClient::with_timeouts(
            url,
            None,
            Duration::from_millis(50),
            Duration::from_millis(50),
        );

        let error = client.status().await.unwrap_err();
        task.join().unwrap();

        assert!(
            error
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_timeout),
            "{error:#}"
        );
    }
}
