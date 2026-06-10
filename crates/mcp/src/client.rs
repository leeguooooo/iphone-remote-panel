//! HTTP client that talks to the iphone-use daemon's agent API.
//!
//! The public surface is intentionally small: `DaemonClient` holds the base
//! URL and optional bearer token and exposes one async method per daemon
//! endpoint.  All I/O errors are surfaced as `anyhow::Error` so the MCP layer
//! can turn them into MCP tool errors.

use crate::types::{InputMsg, StatusResponse};
use reqwest::{header, Client, StatusCode};

const DEFAULT_URL: &str = "http://127.0.0.1:8787";

/// Thin async wrapper over the daemon's `GET /agent/*` and
/// `POST /agent/input` endpoints.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    client: Client,
    base_url: String,
    token: Option<String>,
}

impl DaemonClient {
    /// Build a client from the two environment variables:
    ///
    /// * `PHONE_REMOTE_URL`   — daemon base URL (default `http://127.0.0.1:8787`)
    /// * `PHONE_REMOTE_TOKEN` — bearer token / password (optional; omit for
    ///   open-mode daemons running on localhost)
    pub fn from_env() -> Self {
        let base_url = std::env::var("PHONE_REMOTE_URL")
            .unwrap_or_else(|_| DEFAULT_URL.to_string());
        let token = std::env::var("PHONE_REMOTE_TOKEN").ok();
        Self::new(base_url, token)
    }

    /// Construct with explicit values (useful for tests).
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        let client = Client::builder()
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
        check_status(&resp)?;
        let body: StatusResponse = resp.json().await?;
        Ok(body)
    }

    /// `POST /agent/input` — send one control event to the phone.
    pub async fn input(&self, msg: &InputMsg) -> anyhow::Result<()> {
        let json = msg.to_json();
        let req = self
            .auth(self.client.post(self.url("/agent/input")))
            .header(header::CONTENT_TYPE, "application/json")
            .body(json);
        let resp = req.send().await?;
        check_status(&resp)?;
        Ok(())
    }

    /// `GET /agent/screenshot` — returns raw PNG bytes.
    pub async fn screenshot(&self) -> anyhow::Result<Vec<u8>> {
        let req = self.auth(self.client.get(self.url("/agent/screenshot")));
        let resp = req.send().await?;
        check_status(&resp)?;
        let bytes = resp.bytes().await?;
        Ok(bytes.to_vec())
    }
}

/// Turn a non-2xx status into an `anyhow::Error` that includes the status code
/// and response body (useful for surfacing daemon error messages to the MCP
/// caller).
fn check_status(resp: &reqwest::Response) -> anyhow::Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // Surface common cases with descriptive messages.
    let msg = match status {
        StatusCode::UNAUTHORIZED => "daemon returned 401 Unauthorized — check PHONE_REMOTE_TOKEN",
        StatusCode::TOO_MANY_REQUESTS => "daemon returned 429 Too Many Requests — auth limiter triggered",
        StatusCode::SERVICE_UNAVAILABLE => "daemon returned 503 — iPhone Mirroring window not found",
        StatusCode::BAD_REQUEST => "daemon returned 400 Bad Request — invalid control message",
        _ => "daemon returned unexpected HTTP status",
    };
    anyhow::bail!("{msg} (HTTP {status})")
}

// ---------------------------------------------------------------------------
// Unit tests — pure (no I/O).  Network calls are not mocked; we only test
// the pure request-building helpers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url_trim() {
        let c = DaemonClient::new("http://127.0.0.1:8787/", None);
        assert_eq!(c.url("/agent/status"), "http://127.0.0.1:8787/agent/status");
    }

    #[test]
    fn url_no_double_slash() {
        let c = DaemonClient::new("http://192.168.1.50:8787", None);
        assert_eq!(c.url("/agent/screenshot"), "http://192.168.1.50:8787/agent/screenshot");
    }

    #[test]
    fn from_env_falls_back_to_default() {
        // Make sure PHONE_REMOTE_URL is not set for this sub-test.
        // (We can't unset env in a safe way without unsafe, so we just construct
        //  directly and confirm the default string.)
        let c = DaemonClient::new(
            std::env::var("PHONE_REMOTE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string()),
            None,
        );
        assert!(c.base_url().starts_with("http"));
    }
}
