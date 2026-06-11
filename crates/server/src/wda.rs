//! L2 element-tree control via WebDriverAgent (WDA).
//!
//! This is the "L2" layer (see the roadmap / `docs/wda-setup.html`): instead of
//! the L3 pixel path (vision → coords → a synthetic click on the host Mac's one
//! shared cursor), we drive iOS's own accessibility tree. WDA runs *on the
//! phone* (Appium's runner, default `http://<phone>:8100`) and synthesizes the
//! events itself — so there is no cursor contention, no coordinate drift, and
//! text goes in as a real string (bypassing the keycode / Pinyin-IME caveat).
//!
//! This module is the daemon-side HTTP client for WDA's (W3C-ish) API. It is
//! deliberately standalone and NOT yet wired into the agent routing — the
//! routing decision (when to prefer L2 over L3, how to map a normalized tap to
//! an element) is made once we can iterate against a live WDA. The request
//! shapes follow Appium WebDriverAgent; the response parsers are unit-tested
//! here so the wiring is the only thing left to validate on hardware.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// A WDA endpoint plus a lazily-created session id.
pub struct WdaClient {
    base: String, // e.g. "http://192.168.0.190:8100"
    http: reqwest::Client,
    session: Option<String>,
}

impl WdaClient {
    /// `base_url` is the WDA server root, e.g. `http://<phone-ip>:8100` (LAN) or
    /// `http://127.0.0.1:8100` when tunneled over USB with `iproxy 8100 8100`.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("build reqwest client for WDA")?;
        Ok(Self {
            base: base_url.into().trim_end_matches('/').to_string(),
            http,
            session: None,
        })
    }

    /// `GET /status` — health/liveness probe (no session required).
    /// Returns true when WDA answers with a ready state.
    pub async fn is_up(&self) -> bool {
        match self.http.get(format!("{}/status", self.base)).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }

    /// Ensure a session exists, creating one if needed. WDA accepts an empty
    /// capabilities match and returns a session id (mirrored at top level and
    /// under `value.sessionId` across versions — we accept either).
    pub async fn ensure_session(&mut self) -> Result<&str> {
        if self.session.is_none() {
            let body = serde_json::json!({
                "capabilities": { "alwaysMatch": {}, "firstMatch": [{}] }
            });
            let text = self
                .http
                .post(format!("{}/session", self.base))
                .json(&body)
                .send()
                .await
                .context("POST /session")?
                .text()
                .await
                .unwrap_or_default();
            self.session = Some(parse_session_id(&text)?);
        }
        Ok(self.session.as_deref().unwrap())
    }

    /// `GET /source?format=json` — the element tree as JSON. Returned verbatim
    /// (the agent reasons over it the way it reasons over a screenshot, but it's
    /// text, so an order of magnitude cheaper).
    pub async fn source(&mut self) -> Result<serde_json::Value> {
        let sid = self.ensure_session().await?.to_string();
        let v: Envelope<serde_json::Value> = self
            .http
            .get(format!("{}/session/{}/source?format=json", self.base, sid))
            .send()
            .await
            .context("GET /source")?
            .json()
            .await
            .context("parse /source")?;
        Ok(v.value)
    }

    /// Find one element. `using` is a WDA locator strategy — "accessibility id"
    /// (the iOS accessibility label, e.g. a button's title), "class chain",
    /// "predicate string", "name", or "xpath". Returns the element id.
    pub async fn find_element(&mut self, using: &str, value: &str) -> Result<String> {
        let sid = self.ensure_session().await?.to_string();
        let text = self
            .http
            .post(format!("{}/session/{}/element", self.base, sid))
            .json(&serde_json::json!({ "using": using, "value": value }))
            .send()
            .await
            .context("POST /element")?
            .text()
            .await
            .unwrap_or_default();
        parse_element_id(&text)
            .with_context(|| format!("no element for {using}={value}: {text}"))
    }

    /// Tap an element by id (`POST .../element/:id/click`). Lands on the element
    /// regardless of where it is or what's frontmost — no host cursor involved.
    pub async fn click_element(&mut self, element_id: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        self.http
            .post(format!(
                "{}/session/{}/element/{}/click",
                self.base, sid, element_id
            ))
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST element/click")?
            .error_for_status()
            .context("element/click status")?;
        Ok(())
    }

    /// Type a literal string into an element (`POST .../element/:id/value`).
    /// WDA sends it through the on-device text input, so **CJK goes in directly**
    /// — this is the whole reason L2 beats the L3 keycode path for text.
    pub async fn type_into(&mut self, element_id: &str, text: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        self.http
            .post(format!(
                "{}/session/{}/element/{}/value",
                self.base, sid, element_id
            ))
            // WDA accepts both `value: [chars]` (W3C) and `text: "..."`; send text.
            .json(&serde_json::json!({ "value": [text], "text": text }))
            .send()
            .await
            .context("POST element/value")?
            .error_for_status()
            .context("element/value status")?;
        Ok(())
    }

    /// Coordinate tap fallback via WDA's helper (`POST /wda/tap/0`, absolute
    /// points). Useful when there's no addressable element; still synthesized on
    /// the phone, so no host-cursor contention. Coords are WDA points (top-left
    /// origin), NOT our normalized [0,1] — convert via window size first.
    pub async fn tap_point(&mut self, x: f64, y: f64) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        self.http
            .post(format!("{}/session/{}/wda/tap/0", self.base, sid))
            .json(&serde_json::json!({ "x": x, "y": y }))
            .send()
            .await
            .context("POST /wda/tap")?
            .error_for_status()
            .context("/wda/tap status")?;
        Ok(())
    }
}

/// `{ "value": T, ... }` — WDA wraps successful payloads in a `value` envelope.
#[derive(Deserialize)]
struct Envelope<T> {
    value: T,
}

/// Pull the session id out of a `POST /session` response, accepting both the
/// top-level `sessionId` (older WDA) and `value.sessionId` (W3C) placements.
fn parse_session_id(body: &str) -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(body).with_context(|| format!("session resp not JSON: {body}"))?;
    if let Some(s) = v.get("sessionId").and_then(|s| s.as_str()) {
        return Ok(s.to_string());
    }
    if let Some(s) = v
        .get("value")
        .and_then(|val| val.get("sessionId"))
        .and_then(|s| s.as_str())
    {
        return Ok(s.to_string());
    }
    Err(anyhow!("no sessionId in WDA response: {body}"))
}

/// Pull an element id out of a find/`POST element` response. WDA returns it
/// under `value.ELEMENT` (legacy) and the W3C key
/// `element-6066-11e4-a52e-4f735466cecf`; accept either.
fn parse_element_id(body: &str) -> Result<String> {
    const W3C_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";
    let v: serde_json::Value =
        serde_json::from_str(body).with_context(|| format!("element resp not JSON: {body}"))?;
    let val = v.get("value").ok_or_else(|| anyhow!("no value: {body}"))?;
    for key in ["ELEMENT", W3C_KEY] {
        if let Some(s) = val.get(key).and_then(|s| s.as_str()) {
            return Ok(s.to_string());
        }
    }
    Err(anyhow!("no element id in WDA response: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_top_level() {
        let body = r#"{"sessionId":"ABC-123","value":{"capabilities":{}}}"#;
        assert_eq!(parse_session_id(body).unwrap(), "ABC-123");
    }

    #[test]
    fn session_id_w3c_nested() {
        let body = r#"{"value":{"sessionId":"XYZ-9","capabilities":{}}}"#;
        assert_eq!(parse_session_id(body).unwrap(), "XYZ-9");
    }

    #[test]
    fn session_id_missing_errs() {
        assert!(parse_session_id(r#"{"value":{}}"#).is_err());
    }

    #[test]
    fn element_id_legacy_key() {
        let body = r#"{"value":{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}}"#;
        assert_eq!(parse_element_id(body).unwrap(), "E1");
    }

    #[test]
    fn element_id_w3c_only() {
        let body = r#"{"value":{"element-6066-11e4-a52e-4f735466cecf":"W3"}}"#;
        assert_eq!(parse_element_id(body).unwrap(), "W3");
    }

    #[test]
    fn element_id_not_found_errs() {
        // A W3C "no such element" error payload has no element key.
        let body = r#"{"value":{"error":"no such element","message":"unable to find"}}"#;
        assert!(parse_element_id(body).is_err());
    }
}
