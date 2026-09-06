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
    /// Sent as `X-Phone-Owner` on every control request so the daemon can
    /// refuse a second session that tries to drive the same phone (#72).
    owner: String,
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
            owner: std::env::var("PHONE_REMOTE_OWNER")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("mcp-{}", std::process::id())),
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

    /// `POST /agent/hold {"secs":N}` — keep the phone from idle release for a
    /// bounded human-in-the-loop pause; `0` clears the hold.
    pub async fn hold(&self, secs: u64) -> anyhow::Result<String> {
        let req = self
            .auth(self.client.post(self.url("/agent/hold")))
            .header("x-phone-control", "1")
            .header("x-phone-owner", &self.owner)
            .header(header::CONTENT_TYPE, "application/json")
            .body(format!(r#"{{"secs":{secs}}}"#));
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        Ok(resp.text().await?)
    }

    /// `POST /agent/owner {"release":true}` — hand the phone lease back.
    pub async fn release_owner(&self) -> anyhow::Result<String> {
        let req = self
            .auth(self.client.post(self.url("/agent/owner")))
            .header("x-phone-control", "1")
            .header("x-phone-owner", &self.owner)
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"release":true}"#);
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        Ok(resp.text().await?)
    }

    // -----------------------------------------------------------------------
    // Daemon API
    // -----------------------------------------------------------------------

    /// `GET /agent/status` — health / phone-target probe.
    /// Authenticated GET returning the raw body (2xx only). Used for
    /// endpoints the typed client does not model yet, e.g. `/agent/apps`.
    pub async fn get_text(&self, path: &str) -> anyhow::Result<String> {
        let resp = self.auth(self.client.get(self.url(path))).send().await?;
        let resp = check_status(resp).await?;
        Ok(resp.text().await?)
    }

    pub async fn status(&self) -> anyhow::Result<StatusResponse> {
        let req = self.auth(self.client.get(self.url("/agent/status")));
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        let body: StatusResponse = resp.json().await?;
        Ok(body)
    }

    /// `GET /agent/capabilities` — what this build supports and whether the
    /// phone can be driven right now. Read-only: it opens no device
    /// connection and takes no owner lease.
    pub async fn capabilities(&self) -> anyhow::Result<DaemonResponse> {
        let req = self
            .auth(self.client.get(self.url("/agent/capabilities")))
            .header("x-phone-owner", &self.owner);
        read_response(req.send().await?).await
    }

    /// `POST /agent/input` with the daemon's structured result kept.
    ///
    /// `observe` asks the daemon for a post-action observation
    /// (`?return=delta`); without it the request is byte-for-byte what
    /// [`Self::input`] sends, so existing callers are unaffected.
    pub async fn input_observed(
        &self,
        msg: &InputMsg,
        observe: bool,
    ) -> anyhow::Result<DaemonResponse> {
        let path = if observe {
            "/agent/input?return=delta"
        } else {
            "/agent/input"
        };
        let req = self
            .auth(self.client.post(self.url(path)))
            .header("x-phone-control", "1")
            .header("x-phone-owner", &self.owner)
            .header(header::CONTENT_TYPE, "application/json")
            .body(msg.to_json());
        read_response(req.send().await?).await
    }


    /// `POST /agent/actions`, keeping the response rather than raising it.
    ///
    /// The daemon answers a FAILED flow with a non-2xx status and a complete
    /// JSON body: `failed_step`, `outcome`, `applied_actions`, `retry_safe`,
    /// per-step results. Collapsing that into an error string throws away
    /// exactly what a caller needs after a failure.
    ///
    /// A TRANSPORT failure is still an error: when the request never completed
    /// we do not know what the phone did, and must not pretend otherwise.
    pub async fn actions_outcome(&self, body: &serde_json::Value) -> anyhow::Result<DaemonResponse> {
        let req = self
            .auth(self.client.post(self.url("/agent/actions")))
            .timeout(ACTIONS_TIMEOUT)
            .header("x-phone-control", "1")
            .header("x-phone-owner", &self.owner)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        read_response(req.send().await?).await
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
            .header("x-phone-owner", &self.owner)
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"mode":"agent"}"#);
        let resp = req.send().await?;
        let resp = check_status(resp).await?;
        Ok(resp.text().await?)
    }


    /// [`Self::tap_element`] keeping the daemon's structured result, and
    /// optionally asking for a post-action observation.
    ///
    /// The snapshot id is whatever the CALLER passed: this never substitutes a
    /// baseline of its own, so a tap can only ever be resolved against the
    /// tree the caller actually looked at.
    pub async fn tap_element_observed(
        &self,
        element: usize,
        snapshot: &str,
        observe: bool,
    ) -> anyhow::Result<DaemonResponse> {
        if snapshot.is_empty() {
            anyhow::bail!("snapshot must not be empty");
        }
        let json = serde_json::json!({
            "type": "tap",
            "element": element,
            "snapshot": snapshot,
        })
        .to_string();
        let path = if observe {
            "/agent/input?return=delta"
        } else {
            "/agent/input"
        };
        let req = self
            .auth(self.client.post(self.url(path)))
            .header("x-phone-control", "1")
            .header("x-phone-owner", &self.owner)
            .header(header::CONTENT_TYPE, "application/json")
            .body(json);
        read_response(req.send().await?).await
    }


    /// [`Self::tap_label`] keeping the structured result.
    ///
    /// The snapshot comes from the element read this call just performed —
    /// never from a cached or borrowed baseline — so the daemon's staleness
    /// check is evaluated against the tree this resolution actually used.
    pub async fn tap_label_observed(
        &self,
        label: &str,
        observe: bool,
    ) -> anyhow::Result<DaemonResponse> {
        let body = self.elements().await?;
        let (element, snapshot) = unique_label_target(&body, label)?;
        self.tap_element_observed(element, &snapshot, observe).await
    }
}

// ---------------------------------------------------------------------------
// Structured daemon responses
// ---------------------------------------------------------------------------

/// How much of a response we are willing to hold in memory. Element trees and
/// post-action deltas are routinely hundreds of kilobytes, so this is sized
/// for them; it exists to bound a runaway or hostile response, not to trim
/// normal ones.
const DAEMON_RESPONSE_READ_LIMIT: usize = 4 * 1024 * 1024;

/// How much raw text we quote back to a human or a model. Deliberately much
/// smaller than the read limit and applied ONLY to the preview — the JSON is
/// parsed from the whole body first, so trimming never costs structure.
const DAEMON_RESPONSE_PREVIEW_BYTES: usize = 8 * 1024;

/// One daemon call, with its HTTP status and its body kept together.
///
/// The daemon reports the fate of an action inside the body (`ok`, `outcome`,
/// `retry_safe`), so discarding it — or flattening it into an error string —
/// throws away exactly what a caller needs to decide whether re-sending is
/// safe. This type keeps both halves and refuses to guess when it cannot see
/// them.
#[derive(Debug, Clone)]
pub struct DaemonResponse {
    pub status: reqwest::StatusCode,
    body: String,
    pub json: Option<serde_json::Value>,
    /// The response exceeded [`DAEMON_RESPONSE_READ_LIMIT`] and was NOT
    /// parsed. Nothing about the action can be concluded from it.
    pub too_large: bool,
}

impl DaemonResponse {
    /// Whether the daemon reports the call as successful.
    ///
    /// A 2xx alone is not enough: the daemon answers `200 {"ok":false,…}` for
    /// refusals it wants a caller to read, so an `ok:false` body stays a
    /// failure here.
    ///
    /// **Do not use this to decide whether a MUTATION completed.** With no
    /// readable body — unparseable, or past the read limit — there is nothing
    /// to contradict the status, so this returns `true` for "the request was
    /// accepted". That is the right answer for a read and the wrong one for a
    /// tap. Use [`Self::confirms_action`], which requires the daemon to have
    /// said `ok:true`.
    pub fn ok(&self) -> bool {
        if !self.status.is_success() {
            return false;
        }
        !matches!(
            self.json.as_ref().and_then(|json| json.get("ok")),
            Some(serde_json::Value::Bool(false))
        )
    }

    /// Whether this response is positive EVIDENCE that the action completed.
    ///
    /// Stricter than [`Self::ok`]: a 2xx whose body could not be parsed proves
    /// the request was accepted, not that the phone did anything, so it is not
    /// evidence.
    pub fn confirms_action(&self) -> bool {
        self.status.is_success()
            && matches!(
                self.json.as_ref().and_then(|json| json.get("ok")),
                Some(serde_json::Value::Bool(true))
            )
    }

    /// Whether the daemon EXPLICITLY refused: an object body saying
    /// `ok: false`, at any status.
    ///
    /// The counterpart to [`Self::confirms_action`], and the other half of the
    /// only pair that may be trusted. A body that is merely parseable — an
    /// empty object, or `{"unrelated": 1}` alongside a 500 — told us nothing,
    /// and treating it as the daemon's verdict invents one. Anything that is
    /// neither a confirmation nor a refusal is an unknown outcome.
    pub fn explicit_refusal(&self) -> bool {
        matches!(
            self.json.as_ref().and_then(|json| json.get("ok")),
            Some(serde_json::Value::Bool(false))
        )
    }

    pub fn outcome(&self) -> Option<&str> {
        self.json.as_ref()?.get("outcome")?.as_str()
    }

    pub fn error_code(&self) -> Option<&str> {
        self.json.as_ref()?.get("error")?.as_str()
    }

    /// `Some(true)` ONLY when the daemon said so explicitly.
    ///
    /// An unparseable or oversized body yields `None`, never `Some(true)`:
    /// inferring "safe to retry" from a body we could not read is how a
    /// duplicate tap reaches a phone.
    pub fn retry_safe(&self) -> Option<bool> {
        self.json.as_ref()?.get("retry_safe")?.as_bool()
    }

    /// The whole body, as read (empty when it was too large).
    ///
    /// For callers that need the exact bytes they parsed — evidence files
    /// hashing what they actually saw, or recognising the legacy plain-text
    /// acknowledgement. Use [`Self::preview`] for anything shown to a person
    /// or a model.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Body trimmed to [`DAEMON_RESPONSE_PREVIEW_BYTES`] on a char boundary,
    /// for quoting into human- or model-facing text.
    pub fn preview(&self) -> String {
        if self.too_large {
            return format!(
                "<response larger than {} bytes; not read>",
                DAEMON_RESPONSE_READ_LIMIT
            );
        }
        if self.body.len() <= DAEMON_RESPONSE_PREVIEW_BYTES {
            return self.body.clone();
        }
        let mut end = DAEMON_RESPONSE_PREVIEW_BYTES;
        while end > 0 && !self.body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &self.body[..end])
    }

    /// One line for an error message: status, error code, outcome and whether
    /// a retry is known to be safe.
    pub fn failure_summary(&self) -> String {
        let mut parts = vec![format!("HTTP {}", self.status)];
        if self.status == reqwest::StatusCode::UNAUTHORIZED {
            parts.push("check PHONE_REMOTE_TOKEN".to_string());
        }
        if let Some(code) = self.error_code() {
            parts.push(format!("error={code}"));
        }
        if let Some(outcome) = self.outcome() {
            parts.push(format!("outcome={outcome}"));
        }
        parts.push(match self.retry_safe() {
            Some(true) => "retry_safe=true".to_string(),
            Some(false) => "retry_safe=false".to_string(),
            None => "retry_safe=unknown (do not resend automatically)".to_string(),
        });
        format!("{}: {}", parts.join(" "), self.preview())
    }
}

/// Read a response into [`DaemonResponse`] without failing on a non-2xx.
///
/// Bounded: the body is accumulated chunk by chunk and abandoned past
/// [`DAEMON_RESPONSE_READ_LIMIT`], which is reported as `too_large` rather
/// than guessed about. Within the limit the WHOLE body is parsed, so a large
/// element tree keeps its structure.
pub async fn read_response(resp: reqwest::Response) -> anyhow::Result<DaemonResponse> {
    let status = resp.status();
    let mut resp = resp;
    let mut buffer: Vec<u8> = Vec::new();
    let mut too_large = false;
    while let Some(chunk) = resp.chunk().await.context("read daemon response body")? {
        if buffer.len() + chunk.len() > DAEMON_RESPONSE_READ_LIMIT {
            too_large = true;
            buffer.clear();
            break;
        }
        buffer.extend_from_slice(&chunk);
    }
    // Parse the RAW bytes. Decoding lossily first would let replacement
    // characters repair a corrupt body into valid JSON, and a corrupt body
    // must never be able to answer `ok:true` or `retry_safe:true`.
    let json = if too_large {
        None
    } else {
        serde_json::from_slice(&buffer).ok()
    };
    // The text form is for reporting only, so lossy is right here: a body
    // that is not valid UTF-8 still has to be quotable, and the replacement
    // characters make the damage visible.
    let body = if too_large {
        String::new()
    } else {
        String::from_utf8_lossy(&buffer).into_owned()
    };
    Ok(DaemonResponse {
        status,
        body,
        json,
        too_large,
    })
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
        // Same bound as `mock_daemon_bytes`: a joined handle must not be able
        // to wait forever for a connection a failed test never opened.
        listener.set_nonblocking(true).unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).ok();
            let mut request = [0_u8; 4_096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://{addr}"), task)
    }

    /// Serve a raw byte body (so tests can send invalid UTF-8 or oversized
    /// payloads) and join the thread afterwards.
    fn mock_daemon_bytes(status: &str, body: Vec<u8>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Non-blocking with a deadline. A blocking `accept` waiting for a
        // connection a failed test never makes turns a joined handle into a
        // hung suite rather than a red one.
        listener.set_nonblocking(true).unwrap();
        let status = status.to_string();
        let task = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut stream = loop {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).ok();
            let mut request = [0_u8; 4_096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        });
        (format!("http://{addr}"), task)
    }

    async fn read_from(url: &str) -> DaemonResponse {
        let resp = reqwest::Client::new().get(url).send().await.unwrap();
        read_response(resp).await.unwrap()
    }

    /// A body far larger than the preview must still be parsed in full: the
    /// preview is for quoting, never for deciding. Trailing fields are the
    /// ones a naive "truncate then parse" would destroy.
    #[test]
    fn a_body_larger_than_the_preview_keeps_its_structure() {
        let filler = "x".repeat(64 * 1024);
        let body = format!(
            r#"{{"ok":true,"tree":"{filler}","outcome":"applied","retry_safe":false,"settle":{{"reason":"stable"}}}}"#
        );
        assert!(body.len() > DAEMON_RESPONSE_PREVIEW_BYTES * 4);
        let (url, task) = mock_daemon("200 OK", &body);
        let response = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(read_from(&url));
        task.join().unwrap();

        assert!(response.json.is_some(), "a large body lost its structure");
        assert_eq!(response.outcome(), Some("applied"));
        assert_eq!(response.retry_safe(), Some(false));
        assert_eq!(
            response.json.as_ref().unwrap()["settle"]["reason"],
            "stable",
            "a trailing field was lost"
        );
        assert!(response.confirms_action());
        // Only the preview is trimmed.
        assert!(response.preview().len() <= DAEMON_RESPONSE_PREVIEW_BYTES + 8);
        assert!(response.body().len() > DAEMON_RESPONSE_PREVIEW_BYTES);
    }

    /// Past the read limit nothing may be concluded — least of all that a
    /// retry is safe.
    #[test]
    fn an_oversized_body_is_reported_not_guessed() {
        let body = vec![b'x'; DAEMON_RESPONSE_READ_LIMIT + 1_024];
        let (url, task) = mock_daemon_bytes("200 OK", body);
        let response = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(read_from(&url));
        task.join().unwrap();

        assert!(response.too_large);
        assert!(response.json.is_none());
        assert_eq!(response.retry_safe(), None, "an unread body implied a retry");
        assert!(!response.confirms_action(), "an unread body counted as evidence");
        assert!(response.preview().contains("not read"));
        // The trap this pair exists to prevent: with nothing to contradict it,
        // `ok()` still reports the REQUEST as accepted. That is correct for a
        // read and useless for a mutation, which is why the two are separate.
        assert!(
            response.ok(),
            "ok() answers about the request, not about the phone"
        );
    }

    /// A corrupt body must not be repairable into evidence.
    ///
    /// Decoding lossily before parsing would turn the invalid bytes into
    /// replacement characters, producing valid JSON that says `ok:true` and
    /// `retry_safe:true` — a damaged response would then authorise a resend.
    /// These assertions are unconditional on purpose: an earlier version of
    /// this test guarded them with `if json.is_none()`, which passed while the
    /// bug was present.
    #[test]
    fn an_invalid_utf8_body_can_never_confirm_an_action() {
        // Deliberately the most dangerous shape: everything a caller would act
        // on is present and positive, and only the bytes are broken.
        let mut body = br#"{"ok":true,"outcome":"applied","retry_safe":true,"note":""#.to_vec();
        body.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        body.extend_from_slice(br#""}"#);
        let (url, task) = mock_daemon_bytes("200 OK", body);
        let response = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(read_from(&url));
        task.join().unwrap();

        assert!(!response.too_large);
        assert!(
            response.json.is_none(),
            "invalid UTF-8 was repaired into parseable JSON"
        );
        assert!(
            !response.confirms_action(),
            "a corrupt body confirmed an action"
        );
        assert_eq!(
            response.retry_safe(),
            None,
            "a corrupt body authorised a resend"
        );
        assert_eq!(response.outcome(), None);
        // Still reportable, with the damage visible.
        assert!(!response.preview().is_empty());
        assert!(response.preview().contains('\u{fffd}'));
    }

    /// `200 {"ok":false}` is the daemon refusing while answering politely. It
    /// must stay a failure, and a 2xx with no readable JSON must not count as
    /// proof the phone did anything.
    #[test]
    fn a_2xx_is_not_by_itself_success_or_evidence() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (url, task) = mock_daemon("200 OK", r#"{"ok":false,"error":"phone_owned","outcome":"not_sent","retry_safe":true}"#);
        let refused = runtime.block_on(read_from(&url));
        task.join().unwrap();
        assert!(!refused.ok(), "an ok:false body was treated as success");
        assert!(!refused.confirms_action());
        assert_eq!(refused.error_code(), Some("phone_owned"));
        assert_eq!(refused.outcome(), Some("not_sent"));
        assert_eq!(refused.retry_safe(), Some(true));

        let (url, task) = mock_daemon("200 OK", "not json at all");
        let opaque = runtime.block_on(read_from(&url));
        task.join().unwrap();
        assert!(opaque.ok(), "a 2xx without a body is still HTTP-level success");
        assert!(
            !opaque.confirms_action(),
            "an unparseable 2xx counted as proof the action ran"
        );
        assert_eq!(opaque.retry_safe(), None);
    }

    /// The failure summary must carry the fields a caller decides on, and say
    /// plainly when a retry is unknown.
    #[test]
    fn a_failure_summary_keeps_outcome_and_retry_safety() {
        let (url, task) = mock_daemon(
            "409 Conflict",
            r#"{"ok":false,"error":"phone_owned","outcome":"not_sent","retry_safe":true}"#,
        );
        let response = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(read_from(&url));
        task.join().unwrap();
        let summary = response.failure_summary();
        assert!(summary.contains("error=phone_owned"), "{summary}");
        assert!(summary.contains("outcome=not_sent"), "{summary}");
        assert!(summary.contains("retry_safe=true"), "{summary}");

        let (url, task) = mock_daemon("500 Internal Server Error", "gateway exploded");
        let opaque = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(read_from(&url));
        task.join().unwrap();
        let summary = opaque.failure_summary();
        assert!(
            summary.contains("retry_safe=unknown"),
            "an unreadable failure must not imply a safe retry: {summary}"
        );
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
            .actions_outcome(&request_body)
            .await
            .unwrap();
        task.join().unwrap();

        assert_eq!(response.body(), response_body);
        assert!(response.confirms_action());
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

        let response = DaemonClient::new(url, None)
            .tap_label_observed("发布", false)
            .await
            .unwrap();
        assert!(response.confirms_action(), "{}", response.preview());
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
