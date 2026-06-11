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

    /// Coordinate tap via the W3C Actions API (`POST /session/<id>/actions`, a
    /// touch down-up at the point). Useful when there's no addressable element;
    /// still synthesized on the phone, so no host-cursor contention. Coords are
    /// WDA points (top-left origin), NOT our normalized [0,1] — convert via
    /// window size first.
    ///
    /// The older `/wda/tap/0` helper 404s on current WDA builds (verified on
    /// 14.1.1 / iOS 27): the route was element-scoped (`/wda/tap/<element>`) and
    /// element `0` no longer resolves. `/actions` is the W3C-standard path and is
    /// present across builds, so a coordinate tap never silently falls through to
    /// the Mirroring (L3) injector — which drops the event when the phone is in
    /// hand and the Mirroring window isn't frontmost.
    pub async fn tap_point(&mut self, x: f64, y: f64) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        self.http
            .post(format!("{}/session/{}/actions", self.base, sid))
            .json(&serde_json::json!({
                "actions": [{
                    "type": "pointer",
                    "id": "finger1",
                    "parameters": { "pointerType": "touch" },
                    "actions": [
                        { "type": "pointerMove", "duration": 0, "x": x, "y": y },
                        { "type": "pointerDown", "button": 0 },
                        { "type": "pointerUp", "button": 0 }
                    ]
                }]
            }))
            .send()
            .await
            .context("POST /actions")?
            .error_for_status()
            .context("/actions status")?;
        Ok(())
    }

    /// Window (screen) size in WDA points — needed to map our normalized
    /// `[0,1]` agent coordinates onto [`Self::tap_point`]'s absolute points.
    pub async fn window_size(&mut self) -> Result<(f64, f64)> {
        let sid = self.ensure_session().await?.to_string();
        let v: Envelope<serde_json::Value> = self
            .http
            .get(format!("{}/session/{}/window/size", self.base, sid))
            .send()
            .await
            .context("GET /window/size")?
            .json()
            .await
            .context("parse /window/size")?;
        let w = v.value.get("width").and_then(|x| x.as_f64());
        let h = v.value.get("height").and_then(|x| x.as_f64());
        match (w, h) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Ok((w, h)),
            _ => Err(anyhow!("bad window size: {}", v.value)),
        }
    }

    /// Type a string into whatever currently has keyboard focus
    /// (`POST /wda/keys`). The string goes in as Unicode — CJK lands cleanly
    /// even with a Pinyin keyboard active (hardware-validated).
    pub async fn keys(&mut self, text: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        self.http
            .post(format!("{}/session/{}/wda/keys", self.base, sid))
            .json(&serde_json::json!({ "value": [text] }))
            .send()
            .await
            .context("POST /wda/keys")?
            .error_for_status()
            .context("/wda/keys status")?;
        Ok(())
    }

    /// Dismiss the on-screen keyboard so it stops covering a web page's own
    /// submit/next buttons.
    ///
    /// For a web keyboard the dismiss affordance is the **form-assistant-bar
    /// button** ("Hide keyboard" / CN "隐藏键盘"), which lives *outside* the
    /// `XCUIElementTypeKeyboard` subtree — so WDA's own `POST
    /// /wda/keyboard/dismiss` (it only taps keyboard *keys*) is a no-op here.
    /// Hardware-verified: tapping that accessory button is what actually works.
    /// So we find a Button whose name/label is one of the locale-specific
    /// dismiss labels and tap it; if none is present we fall back to the native
    /// `/wda/keyboard/dismiss` (covers system keyboards that do have a Done key).
    /// Best-effort throughout — no keyboard up is success, not an error.
    pub async fn dismiss_keyboard(&mut self) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        const LABELS: &[&str] = &[
            "Hide keyboard", "隐藏键盘", "キーボードを閉じる", "閉じる", "完了",
            "Done", "收起键盘", "關閉鍵盤",
        ];
        // NSPredicate: match the accessory dismiss button by name OR label.
        let quoted = LABELS
            .iter()
            .map(|l| format!("'{}'", l.replace('\'', "\\'")))
            .collect::<Vec<_>>()
            .join(", ");
        let predicate = format!(
            "type == 'XCUIElementTypeButton' AND (name IN {{{quoted}}} OR label IN {{{quoted}}})"
        );
        if let Ok(resp) = self
            .http
            .post(format!("{}/session/{}/element", self.base, sid))
            .json(&serde_json::json!({ "using": "predicate string", "value": predicate }))
            .send()
            .await
        {
            if let Ok(env) = resp.json::<Envelope<serde_json::Value>>().await {
                if let Some(eid) = env
                    .value
                    .get("ELEMENT")
                    .or_else(|| env.value.get("element-6066-11e4-a52e-4f735466cecf"))
                    .and_then(|v| v.as_str())
                {
                    let _ = self
                        .http
                        .post(format!("{}/session/{}/element/{}/click", self.base, sid, eid))
                        .json(&serde_json::json!({}))
                        .send()
                        .await;
                    return Ok(());
                }
            }
        }
        // No accessory button (native keyboard or already gone) — try the
        // built-in dismiss with a key list, then give up silently.
        let _ = self
            .http
            .post(format!("{}/session/{}/wda/keyboard/dismiss", self.base, sid))
            .json(&serde_json::json!({ "keyNames": ["Done", "完了", "return", "前往", "search"] }))
            .send()
            .await;
        Ok(())
    }

    /// Current phone screen as PNG bytes (`GET /screenshot`, base64 in the
    /// envelope). Works with no Mirroring window at all — the capture happens
    /// on the phone — so it's the L2 fallback when the L3 capture is gone.
    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        // Session-less endpoint; no ensure_session needed.
        let v: Envelope<String> = self
            .http
            .get(format!("{}/screenshot", self.base))
            .send()
            .await
            .context("GET /screenshot")?
            .json()
            .await
            .context("parse /screenshot")?;
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(v.value.trim())
            .context("decode screenshot base64")
    }

    /// The element tree flattened to agent-friendly rows (type, label, rect).
    /// An agent reasons over this the way it reasons over a screenshot, but
    /// it's text — an order of magnitude cheaper, and it carries the labels
    /// needed for [`Self::find_element`]/[`Self::click_element`].
    pub async fn elements(&mut self) -> Result<Vec<ElementRow>> {
        let tree = self.source().await?;
        let mut rows = Vec::new();
        flatten_tree(&tree, 0, &mut rows);
        Ok(rows)
    }

    /// Find an element by its visible label and tap it. Tries the
    /// "accessibility id" strategy first (exact accessibility label), then a
    /// predicate on `name`/`label`. This is the primary L2 action: no
    /// coordinates, no host cursor, immune to layout drift.
    pub async fn click_label(&mut self, label: &str) -> Result<()> {
        // Escape single quotes for the NSPredicate string literal.
        let esc = label.replace('\'', "\\'");
        let attempts = [
            ("accessibility id", label.to_string()),
            (
                "predicate string",
                format!("name == '{esc}' OR label == '{esc}' OR value == '{esc}'"),
            ),
        ];
        let mut last_err = None;
        for (using, value) in attempts {
            match self.find_element(using, &value).await {
                Ok(eid) => return self.click_element(&eid).await,
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("element not found: {label}")))
    }

    /// Drop the cached session (e.g. after an error that suggests it went
    /// stale); the next call re-creates one via [`Self::ensure_session`].
    pub fn invalidate_session(&mut self) {
        self.session = None;
    }

    /// `POST /session/:id/wda/lock` — lock the phone's screen. Used by the
    /// agent→mirror mode switch: iPhone Mirroring can only connect to a
    /// LOCKED phone, so locking right before the runner is stopped makes the
    /// reconnect deterministic (hardware-verified).
    pub async fn lock(&mut self) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        self.http
            .post(format!("{}/session/{}/wda/lock", self.base, sid))
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST wda/lock")?
            .error_for_status()
            .context("wda/lock status")?;
        Ok(())
    }
}

/// One row of the flattened element tree.
#[derive(Debug, serde::Serialize, PartialEq)]
pub struct ElementRow {
    /// Element type without the `XCUIElementType` prefix (e.g. `Button`).
    pub kind: String,
    /// The accessibility label/name — what `find_element("accessibility id", …)`
    /// matches on. Empty-label rows are skipped during flattening.
    pub label: String,
    /// Position + size in WDA points: `[x, y, w, h]`.
    pub rect: [f64; 4],
    /// Tree depth (purely presentational).
    pub depth: u32,
}

/// Recursively flatten a WDA `/source?format=json` tree, keeping only rows an
/// agent can act on or learn from: anything with a non-empty label, or an
/// interactive type. Order is document order (roughly top-to-bottom).
fn flatten_tree(node: &serde_json::Value, depth: u32, out: &mut Vec<ElementRow>) {
    const INTERACTIVE: [&str; 8] = [
        "Button", "Cell", "TextField", "SecureTextField", "SearchField", "Switch", "Slider",
        "TextView",
    ];
    let kind = node
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_start_matches("XCUIElementType")
        .to_string();
    let label = node
        .get("label")
        .and_then(|l| l.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| node.get("name").and_then(|l| l.as_str()))
        .unwrap_or("")
        .to_string();
    if !label.is_empty() || INTERACTIVE.contains(&kind.as_str()) {
        let r = node.get("rect").cloned().unwrap_or_default();
        let g = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        out.push(ElementRow {
            kind,
            label,
            rect: [g("x"), g("y"), g("width"), g("height")],
            depth,
        });
    }
    if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
        for c in children {
            flatten_tree(c, depth + 1, out);
        }
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

    #[test]
    fn flatten_keeps_labels_and_interactive_skips_noise() {
        let tree: serde_json::Value = serde_json::from_str(
            r#"{
              "type":"XCUIElementTypeApplication","label":"",
              "rect":{"x":0,"y":0,"width":440,"height":956},
              "children":[
                {"type":"XCUIElementTypeOther","label":"","children":[
                  {"type":"XCUIElementTypeButton","label":"新备忘录",
                   "rect":{"x":369,"y":885,"width":38,"height":38}},
                  {"type":"XCUIElementTypeStaticText","label":"你好世界",
                   "rect":{"x":10,"y":20,"width":100,"height":20}},
                  {"type":"XCUIElementTypeImage","label":""}
                ]}
              ]
            }"#,
        )
        .unwrap();
        let mut rows = Vec::new();
        flatten_tree(&tree, 0, &mut rows);
        // unlabeled Application/Other/Image dropped; Button + labeled text kept
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "Button");
        assert_eq!(rows[0].label, "新备忘录");
        assert_eq!(rows[0].rect, [369.0, 885.0, 38.0, 38.0]);
        assert_eq!(rows[1].label, "你好世界");
    }
}
