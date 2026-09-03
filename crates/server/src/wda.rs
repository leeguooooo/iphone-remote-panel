//! L2 element-tree control via WebDriverAgent (WDA).
//!
//! This is the "L2" layer (see the roadmap / `docs/wda-setup.html`): instead of
//! the L3 pixel path (vision → coords → a synthetic click on the host Mac's one
//! shared cursor), we drive iOS's own accessibility tree. WDA runs *on the
//! phone* (Appium's runner, default `http://<phone>:8100`) and synthesizes the
//! events itself — so there is no cursor contention, no coordinate drift, and
//! text goes in as a real string (bypassing the keycode / Pinyin-IME caveat).
//!
//! This module is the daemon-side HTTP client for WDA's (W3C-ish) API. Direct
//! mode routes browser and agent input here and fails closed when the device
//! service is unavailable; the legacy Mac-side L3 path is a separate backend.
//! Request shapes follow Appium WebDriverAgent, and response parsers reject
//! both HTTP failures and W3C error envelopes.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

fn bounded_move_duration_ms(duration_ms: u64) -> u64 {
    duration_ms.clamp(80, 2_000)
}

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
            .connect_timeout(Duration::from_secs(3))
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

    /// Device screen lock state (`GET /wda/locked`). XCUITest cannot drive a
    /// locked phone, so this disambiguates a "ready but won't act" WDA (locked,
    /// recoverable by unlocking) from a genuinely wedged one.
    pub async fn locked(&mut self) -> Result<bool> {
        let sid = self.ensure_session().await?.to_string();
        let body = self
            .http
            .get(format!("{}/session/{}/wda/locked", self.base, sid))
            .send()
            .await
            .context("GET /wda/locked")?
            .error_for_status()
            .context("/wda/locked status")?
            .text()
            .await
            .context("parse /wda/locked")?;
        parse_wda_value(&body, "GET /wda/locked")?
            .as_bool()
            .ok_or_else(|| anyhow!("GET /wda/locked returned a non-boolean value: {body}"))
    }

    /// Probe WDA at the ACTION level, not just `/status`. `GET /status` lies:
    /// it keeps reporting `ready` even when every UI action fails Code=41 "Not
    /// authorized for performing UI testing actions" — the "zombie ready" state
    /// caused by a locked phone, sleep, or a severed testmanagerd/CoreDevice
    /// connection (e.g. after a WARP toggle). To catch that, additionally run a
    /// cheap real action (`wda/apps/list`, which needs the live test connection);
    /// only if THAT succeeds is the runner actually drivable.
    ///
    /// Do not use `wda/activeAppInfo` here. WDA implements that route by
    /// resolving the current application object, which can request a full
    /// accessibility snapshot. Large views such as WeChat's chat list can make
    /// that supposedly cheap health probe hang until the daemon marks an
    /// otherwise healthy runner unavailable. `wda/apps/list` asks
    /// testmanagerd for the active process list without traversing the
    /// foreground app's element tree, and is also safe on the Home screen.
    pub async fn probe_health(&mut self) -> WdaHealth {
        if !self.is_up().await {
            return WdaHealth::down();
        }
        let sid = match self.ensure_session().await {
            Ok(s) => s.to_string(),
            Err(_) => {
                return WdaHealth {
                    up: true,
                    actionable: false,
                    locked: None,
                }
            }
        };
        let locked = self.locked().await.ok();
        let active_apps_actionable = match self
            .http
            .get(format!("{}/session/{}/wda/apps/list", self.base, sid))
            .send()
            .await
        {
            Ok(response) => ensure_wda_success(response, "GET /wda/apps/list")
                .await
                .is_ok(),
            Err(_) => false,
        };
        // A live testmanagerd connection is necessary but not sufficient:
        // WDA can still answer apps/list while the iPhone is locked. Treat an
        // unreadable or positive lock state as non-actionable so callers never
        // inject a gesture into the lock screen after the phone sleeps.
        let actionable = locked == Some(false) && active_apps_actionable;
        if !actionable {
            // Drop the severed session so the next caller re-creates one.
            self.invalidate_session();
        }
        WdaHealth {
            up: true,
            actionable,
            locked,
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
                .error_for_status()
                .context("POST /session status")?
                .text()
                .await
                .context("POST /session body")?;
            self.session = Some(parse_session_id(&text)?);
            // Opt-in bounded-snapshot settings (issue #44): apps with an
            // enormous accessibility tree (hardware-reported with KakaoTalk)
            // can make WDA's hierarchy snapshot run so long that testmanagerd
            // kills the whole runner (`** BUILD INTERRUPTED **`) and the
            // device goes blocked. WDA's `snapshotMaxDepth` /
            // `customSnapshotTimeout` settings live in its process-global
            // configuration, so applying them once per session also bounds the
            // session-less `/source` reads behind `/agent/elements`. Off by
            // default — behavior is byte-identical unless the operator sets
            // the env vars. Best-effort: a failure to apply must not take
            // down session creation.
            if let Some(settings) = snapshot_settings_from_env() {
                let sid = self.session.as_deref().unwrap().to_string();
                let result = self
                    .http
                    .post(format!("{}/session/{}/appium/settings", self.base, sid))
                    .json(&serde_json::json!({ "settings": settings }))
                    .send()
                    .await;
                match result {
                    Ok(response) => {
                        if let Err(error) =
                            ensure_wda_success(response, "POST /appium/settings (snapshot)").await
                        {
                            tracing::warn!("apply WDA snapshot settings: {error:#}");
                        }
                    }
                    Err(error) => tracing::warn!("apply WDA snapshot settings: {error:#}"),
                }
            }
        }
        Ok(self.session.as_deref().unwrap())
    }

    /// Session-less `GET /source?format=json` — the active UI tree as JSON.
    ///
    /// WDA explicitly exposes this route without a session and resolves the
    /// current active application itself. That matters for system-owned remote
    /// views such as the document picker: an app-scoped session can go stale or
    /// stay bound to the presenting app while the visible picker is elsewhere.
    /// Source reads are read-only, so avoiding session creation also removes a
    /// source/session churn loop during relay recovery.
    pub async fn source(&mut self) -> Result<serde_json::Value> {
        let body = self
            .http
            .get(format!("{}/source?format=json", self.base))
            .send()
            .await
            .context("GET /source")?
            .error_for_status()
            .context("/source status")?
            .text()
            .await
            .context("parse /source")?;
        parse_wda_value(&body, "GET /source")
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
            .error_for_status()
            .context("POST /element status")?
            .text()
            .await
            .context("POST /element body")?;
        parse_element_id(&text).with_context(|| format!("no element for {using}={value}: {text}"))
    }

    /// Find ALL elements matching a locator (`POST .../elements`, plural), in
    /// document order. Returns their element ids. Used to address the Nth
    /// element of a kind (e.g. the 3 columns of a date PickerWheel) when none
    /// has a usable label.
    pub async fn find_elements(&mut self, using: &str, value: &str) -> Result<Vec<String>> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/elements", self.base, sid))
            .json(&serde_json::json!({ "using": using, "value": value }))
            .send()
            .await
            .context("POST /elements")?;
        let value = ensure_wda_success(response, "POST /elements").await?;
        let elements = value
            .as_array()
            .ok_or_else(|| anyhow!("POST /elements returned non-array value: {value}"))?;
        Ok(elements
            .iter()
            .filter_map(|e| {
                e.get("ELEMENT")
                    .or_else(|| e.get("element-6066-11e4-a52e-4f735466cecf"))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            })
            .collect())
    }

    /// Set a date/option PickerWheel to a target value (issue #23). A `scroll`
    /// gesture doesn't reach the wheel's recognizer; the reliable path is
    /// XCUITest's `adjustToPickerWheelValue:`, which WDA triggers when you POST
    /// a value to the wheel element. `column` selects which wheel (0-based, the
    /// order they appear — e.g. month=0, day=1, year=2). On-device, so no
    /// cursor/frontmost requirement.
    pub async fn set_picker(&mut self, column: usize, value: &str) -> Result<()> {
        let wheels = self
            .find_elements("class chain", "**/XCUIElementTypePickerWheel")
            .await?;
        let id = wheels.get(column).ok_or_else(|| {
            anyhow!(
                "picker column {column} out of range ({} wheel(s) on screen)",
                wheels.len()
            )
        })?;
        // POST a BARE value array (not type_into's {value,text} — the extra
        // `text` makes WDA keyboard-type instead of calling
        // adjustToPickerWheelValue, so the wheel reported ok but never moved).
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!(
                "{}/session/{}/element/{}/value",
                self.base, sid, id
            ))
            .json(&serde_json::json!({ "value": [value] }))
            .send()
            .await
            .context("POST pickerwheel value")?;
        ensure_wda_success(response, "POST pickerwheel value").await?;
        Ok(())
    }

    /// Tap an element by id (`POST .../element/:id/click`). Lands on the element
    /// regardless of where it is or what's frontmost — no host cursor involved.
    pub async fn click_element(&mut self, element_id: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!(
                "{}/session/{}/element/{}/click",
                self.base, sid, element_id
            ))
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST element/click")?;
        ensure_wda_success(response, "POST element/click").await?;
        Ok(())
    }

    /// Type a literal string into an element (`POST .../element/:id/value`).
    /// WDA sends it through the on-device text input, so **CJK goes in directly**
    /// — this is the whole reason L2 beats the L3 keycode path for text.
    pub async fn type_into(&mut self, element_id: &str, text: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!(
                "{}/session/{}/element/{}/value",
                self.base, sid, element_id
            ))
            // WDA accepts both `value: [chars]` (W3C) and `text: "..."`; send text.
            .json(&serde_json::json!({ "value": [text], "text": text }))
            .send()
            .await
            .context("POST element/value")?;
        ensure_wda_success(response, "POST element/value").await?;
        Ok(())
    }

    /// Clear a specific element's text (`POST .../element/:id/clear`). Unlike
    /// [`Self::clear_active`] this does not depend on keyboard focus, so a
    /// `set_value` action can replace a field's contents without tapping it
    /// first.
    pub async fn clear_element(&mut self, element_id: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!(
                "{}/session/{}/element/{}/clear",
                self.base, sid, element_id
            ))
            // A bodyless POST is rejected with 400 by current WDA builds
            // (hardware-hit on 9.15.3 during the set_value("") validation);
            // send the same empty JSON object every other element POST sends.
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST element/clear")?;
        ensure_wda_success(response, "POST element/clear").await?;
        Ok(())
    }

    /// Find ALL elements matching a locator UNDER an already-resolved element
    /// (`POST .../element/:id/elements`), in document order. Used to address a
    /// composite control's children (e.g. a Stepper's Increment/Decrement
    /// buttons) without widening the search to the whole tree.
    pub async fn find_elements_from(
        &mut self,
        element_id: &str,
        using: &str,
        value: &str,
    ) -> Result<Vec<String>> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!(
                "{}/session/{}/element/{}/elements",
                self.base, sid, element_id
            ))
            .json(&serde_json::json!({ "using": using, "value": value }))
            .send()
            .await
            .context("POST element/elements")?;
        let value = ensure_wda_success(response, "POST element/elements").await?;
        let elements = value
            .as_array()
            .ok_or_else(|| anyhow!("POST element/elements returned non-array value: {value}"))?;
        Ok(elements
            .iter()
            .filter_map(|e| {
                e.get("ELEMENT")
                    .or_else(|| e.get("element-6066-11e4-a52e-4f735466cecf"))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            })
            .collect())
    }

    /// POST one element-scoped `/wda/element/:id/<command>` gesture. Every
    /// call sends a JSON body (at least `{}`) because a bodyless POST is
    /// rejected with 400 by current WDA builds (see [`Self::clear_element`]).
    async fn post_element_gesture(
        &mut self,
        element_id: &str,
        command: &str,
        body: serde_json::Value,
    ) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let operation = format!("POST wda/element/{command}");
        let response = self
            .http
            .post(format!(
                "{}/session/{}/wda/element/{}/{}",
                self.base, sid, element_id, command
            ))
            .json(&body)
            .send()
            .await
            .with_context(|| operation.clone())?;
        ensure_wda_success(response, &operation).await?;
        Ok(())
    }

    /// Long-press an element (`POST .../wda/element/:id/touchAndHold`) — the
    /// element-scoped context-menu ("secondary action") gesture. WDA computes
    /// the geometry, so this is immune to stale source-tree rectangles.
    pub async fn touch_and_hold_element(
        &mut self,
        element_id: &str,
        duration_s: f64,
    ) -> Result<()> {
        self.post_element_gesture(
            element_id,
            "touchAndHold",
            serde_json::json!({ "duration": duration_s }),
        )
        .await
    }

    /// Double-tap an element (`POST .../wda/element/:id/doubleTap`).
    pub async fn double_tap_element(&mut self, element_id: &str) -> Result<()> {
        self.post_element_gesture(element_id, "doubleTap", serde_json::json!({}))
            .await
    }

    /// Two-finger tap an element (`POST .../wda/element/:id/twoFingerTap`).
    pub async fn two_finger_tap_element(&mut self, element_id: &str) -> Result<()> {
        self.post_element_gesture(element_id, "twoFingerTap", serde_json::json!({}))
            .await
    }

    /// Scroll an element into view (`POST .../wda/element/:id/scrollTo`).
    pub async fn scroll_element_to_visible(&mut self, element_id: &str) -> Result<()> {
        self.post_element_gesture(element_id, "scrollTo", serde_json::json!({}))
            .await
    }

    /// Pinch an element (`POST .../wda/element/:id/pinch`). `scale` above 1
    /// zooms in, below 1 zooms out; XCUITest wants `velocity`'s sign to match.
    pub async fn pinch_element(
        &mut self,
        element_id: &str,
        scale: f64,
        velocity: f64,
    ) -> Result<()> {
        self.post_element_gesture(
            element_id,
            "pinch",
            serde_json::json!({ "scale": scale, "velocity": velocity }),
        )
        .await
    }

    /// Rotate an element (`POST .../wda/element/:id/rotate`). `rotation` is in
    /// radians; XCUITest wants `velocity`'s sign to match the rotation's.
    pub async fn rotate_element(
        &mut self,
        element_id: &str,
        rotation: f64,
        velocity: f64,
    ) -> Result<()> {
        self.post_element_gesture(
            element_id,
            "rotate",
            serde_json::json!({ "rotation": rotation, "velocity": velocity }),
        )
        .await
    }

    /// Force-press an element (`POST .../wda/element/:id/forceTouch`). WDA
    /// treats `pressure` and `duration` as a pair, so a caller providing
    /// either gets both (defaults: pressure 1.0, duration 0.5 s); providing
    /// neither sends the plain default force press.
    pub async fn force_touch_element(
        &mut self,
        element_id: &str,
        pressure: Option<f64>,
        duration_s: Option<f64>,
    ) -> Result<()> {
        let body = if pressure.is_some() || duration_s.is_some() {
            serde_json::json!({
                "pressure": pressure.unwrap_or(1.0),
                "duration": duration_s.unwrap_or(0.5),
            })
        } else {
            serde_json::json!({})
        };
        self.post_element_gesture(element_id, "forceTouch", body)
            .await
    }

    /// Move a picker wheel one notch (`POST .../wda/pickerwheel/:id/select`).
    /// `order` is `"next"` (increment) or `"previous"` (decrement); `offset`
    /// is WDA's tap offset from the wheel's center (0.2 is its documented
    /// sweet spot for one-notch moves).
    pub async fn pickerwheel_select(
        &mut self,
        element_id: &str,
        order: &str,
        offset: f64,
    ) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!(
                "{}/session/{}/wda/pickerwheel/{}/select",
                self.base, sid, element_id
            ))
            .json(&serde_json::json!({ "order": order, "offset": offset }))
            .send()
            .await
            .context("POST wda/pickerwheel/select")?;
        ensure_wda_success(response, "POST wda/pickerwheel/select").await?;
        Ok(())
    }

    /// Adjust a wheel/slider by POSTing a BARE `{"value":[…]}` to
    /// `element/:id/value` — no `text` key, exactly like [`Self::set_picker`]:
    /// the extra `text` makes WDA keyboard-type instead of calling
    /// `adjustToPickerWheelValue:` / `adjustToNormalizedSliderPosition:`.
    pub async fn adjust_element_value(&mut self, element_id: &str, value: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!(
                "{}/session/{}/element/{}/value",
                self.base, sid, element_id
            ))
            .json(&serde_json::json!({ "value": [value] }))
            .send()
            .await
            .context("POST element/value (adjust)")?;
        ensure_wda_success(response, "POST element/value (adjust)").await?;
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
        let response = self
            .http
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
            .context("POST /actions")?;
        ensure_wda_success(response, "POST /actions").await?;
        Ok(())
    }

    /// Hold one touch at a coordinate for `duration_ms`, then release it.
    ///
    /// This is a real on-device long press.  The old browser dispatcher mapped
    /// `longpress` to [`Self::tap_point`], so context menus and edit affordances
    /// never appeared even though the UI claimed success.
    pub async fn longpress_point(&mut self, x: f64, y: f64, duration_ms: u64) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/actions", self.base, sid))
            .json(&serde_json::json!({
                "actions": [{
                    "type": "pointer",
                    "id": "finger1",
                    "parameters": { "pointerType": "touch" },
                    "actions": [
                        { "type": "pointerMove", "duration": 0, "x": x, "y": y },
                        { "type": "pointerDown", "button": 0 },
                        { "type": "pause", "duration": duration_ms.clamp(300, 2_000) },
                        { "type": "pointerUp", "button": 0 }
                    ]
                }]
            }))
            .send()
            .await
            .context("POST /actions (long press)")?;
        ensure_wda_success(response, "POST /actions (long press)").await?;
        Ok(())
    }

    /// Press, hold, drag, and release one on-device touch.
    pub async fn drag(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        hold_ms: u64,
        duration_ms: u64,
    ) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/actions", self.base, sid))
            .json(&serde_json::json!({
                "actions": [{
                    "type": "pointer",
                    "id": "finger1",
                    "parameters": { "pointerType": "touch" },
                    "actions": [
                        { "type": "pointerMove", "duration": 0, "x": x1, "y": y1 },
                        { "type": "pointerDown", "button": 0 },
                        { "type": "pause", "duration": hold_ms.clamp(0, 2_000) },
                        {
                            "type": "pointerMove",
                            "duration": bounded_move_duration_ms(duration_ms),
                            "x": x2,
                            "y": y2
                        },
                        { "type": "pointerUp", "button": 0 }
                    ]
                }]
            }))
            .send()
            .await
            .context("POST /actions (drag)")?;
        ensure_wda_success(response, "POST /actions (drag)").await?;
        Ok(())
    }

    /// Swipe/scroll gesture via the W3C Actions API — a single touch that
    /// presses at `(x1,y1)`, drags to `(x2,y2)` over `duration_ms`, and lifts.
    /// Synthesized on the phone like [`Self::tap_point`], so it works in agent
    /// mode regardless of whether the Mirroring window is frontmost (issue #27:
    /// `scroll` used to fall back to the L3/CGEvent path, which the OS drops
    /// when a human holds the Mac's foreground). Coords are WDA points
    /// (top-left origin), NOT normalized — convert via [`Self::window_size`].
    ///
    /// A short press-pause before the move makes XCUITest register a drag
    /// rather than a flick, so the content tracks the finger predictably.
    pub async fn swipe(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        duration_ms: u64,
    ) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/actions", self.base, sid))
            .json(&serde_json::json!({
                "actions": [{
                    "type": "pointer",
                    "id": "finger1",
                    "parameters": { "pointerType": "touch" },
                    "actions": [
                        { "type": "pointerMove", "duration": 0, "x": x1, "y": y1 },
                        { "type": "pointerDown", "button": 0 },
                        { "type": "pause", "duration": 80 },
                        {
                            "type": "pointerMove",
                            "duration": bounded_move_duration_ms(duration_ms),
                            "x": x2,
                            "y": y2
                        },
                        { "type": "pointerUp", "button": 0 }
                    ]
                }]
            }))
            .send()
            .await
            .context("POST /actions (swipe)")?;
        ensure_wda_success(response, "POST /actions (swipe)").await?;
        Ok(())
    }

    /// Press the Home button on-device (`POST /wda/pressButton` `{name:home}`).
    /// Works in agent mode regardless of the Mirroring window — the `shortcut`
    /// path routes through L3 and needs the mirror frontmost, so this is the
    /// reliable "go home".
    pub async fn press_home(&mut self) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/wda/pressButton", self.base, sid))
            .json(&serde_json::json!({ "name": "home" }))
            .send()
            .await
            .context("POST /wda/pressButton home")?;
        ensure_wda_success(response, "POST /wda/pressButton home").await?;
        Ok(())
    }

    /// Open Spotlight from SpringBoard through its accessibility element.
    ///
    /// A coordinate tap on the Search pill can be acknowledged by WDA without
    /// changing the screen. Resolve the localized accessibility element and
    /// click it instead, then verify that Spotlight's text field appeared
    /// before reporting success.
    pub async fn open_spotlight(&mut self) -> Result<()> {
        self.press_home().await?;
        tokio::time::sleep(Duration::from_millis(450)).await;

        let mut search_element = None;
        for label in ["搜索", "Search", "検索"] {
            let elements = self.find_elements("accessibility id", label).await?;
            match elements.as_slice() {
                [element] => {
                    search_element = Some(element.clone());
                    break;
                }
                [] => {}
                _ => return Err(anyhow!("Spotlight Search element is ambiguous for {label}")),
            }
        }
        let element =
            search_element.ok_or_else(|| anyhow!("Spotlight Search element not found"))?;
        self.click_element(&element).await?;
        tokio::time::sleep(Duration::from_millis(350)).await;

        let rows = self.elements().await?;
        let opened = rows.iter().any(|row| {
            row.kind == "TextField"
                && (row.label == "SpotlightSearchField"
                    || row
                        .placeholder
                        .as_deref()
                        .is_some_and(|value| matches!(value, "搜索" | "Search" | "検索")))
        });
        if !opened {
            return Err(anyhow!(
                "Spotlight Search click was acknowledged but no search field appeared"
            ));
        }
        Ok(())
    }

    /// Navigate back via the universal iOS edge-swipe-from-left gesture (works
    /// in almost every app, unlike a nav-bar back button whose label/position
    /// varies). A short swipe from the very left edge to ~55% width at mid
    /// height.
    pub async fn back(&mut self) -> Result<()> {
        let (sw, sh) = self.window_size().await?;
        let y = sh * 0.5;
        self.swipe(1.0, y, sw * 0.55, y, 250).await
    }

    /// Clear the currently-focused text field (`GET /element/active` →
    /// `POST /element/:id/clear`). Lets `text` REPLACE a field's contents
    /// instead of appending to stale text (the "ClaudeClaude" search-box bug).
    pub async fn clear_active(&mut self) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let body = self
            .http
            .get(format!("{}/session/{}/element/active", self.base, sid))
            .send()
            .await
            .context("GET /element/active")?
            .error_for_status()
            .context("/element/active status")?
            .text()
            .await
            .context("/element/active body")?;
        let id = parse_element_id(&body)?;
        let response = self
            .http
            .post(format!(
                "{}/session/{}/element/{}/clear",
                self.base, sid, id
            ))
            // Same bodyless-POST 400 as clear_element on current WDA builds.
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST /element/clear")?;
        ensure_wda_success(response, "POST /element/clear").await?;
        Ok(())
    }

    /// Tune WDA's built-in MJPEG screen stream (`POST /appium/settings`). The
    /// defaults cap at ~9 fps; bumping `mjpegServerFramerate` and shrinking the
    /// frame (`mjpegScalingFactor` %, `mjpegServerScreenshotQuality` %) lifts it
    /// to ~28 fps with a still-legible image. The stream itself is served on the
    /// device's MJPEG port (9100) — see `/agent/mjpeg`.
    pub async fn set_mjpeg_settings(
        &mut self,
        framerate: u32,
        scaling: u32,
        quality: u32,
    ) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/appium/settings", self.base, sid))
            .json(&serde_json::json!({ "settings": {
                "mjpegServerFramerate": framerate,
                "mjpegScalingFactor": scaling,
                "mjpegServerScreenshotQuality": quality,
            }}))
            .send()
            .await
            .context("POST /appium/settings (mjpeg)")?;
        ensure_wda_success(response, "POST /appium/settings (mjpeg)").await?;
        Ok(())
    }

    /// Window (screen) size in WDA points — needed to map our normalized
    /// `[0,1]` agent coordinates onto [`Self::tap_point`]'s absolute points.
    pub async fn window_size(&mut self) -> Result<(f64, f64)> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .get(format!("{}/session/{}/window/size", self.base, sid))
            .send()
            .await
            .context("GET /window/size")?;
        let value = ensure_wda_success(response, "GET /window/size").await?;
        let w = value.get("width").and_then(|x| x.as_f64());
        let h = value.get("height").and_then(|x| x.as_f64());
        match (w, h) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Ok((w, h)),
            _ => Err(anyhow!("bad window size: {value}")),
        }
    }

    /// Launch (or foreground) an app by bundle id (`POST /wda/apps/launch`).
    /// This bypasses the Home-Screen icon grid entirely — those icons report
    /// `rect [0,0,0,0]` and don't navigate on a label/coordinate tap (issue
    /// #18-A), so launching by bundle id is the only reliable way to open a
    /// system app. Examples: Settings = `com.apple.Preferences`, Photos =
    /// `com.apple.mobileslideshow`.
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/wda/apps/launch", self.base, sid))
            .json(&serde_json::json!({ "bundleId": bundle_id }))
            .send()
            .await
            .context("POST /wda/apps/launch")?;
        ensure_wda_success(response, "POST /wda/apps/launch").await?;
        Ok(())
    }

    /// Open a URL on the device via WDA's **sessionless** `POST /url`.
    ///
    /// appium-webdriveragent registers this route both with and without a
    /// session (`FBSessionCommands.m`), and the handler uses the modern
    /// system `open` path — no Safari detour. We deliberately use the
    /// sessionless form so a deep link (e.g. `shortcuts://run-shortcut?...`
    /// for the semantic intents channel) can fire even before a session is
    /// established. The JSON body is required: current WDA rejects bodyless
    /// POSTs with a 400, so the URL always rides in `{"url": ...}`.
    pub async fn open_url(&mut self, url: &str) -> Result<()> {
        // Session-scoped route: the sessionless `POST /url` the design
        // expected 404s on WDA 9.15.3 (hardware-verified); the W3C
        // `POST /session/:sid/url` opens the deep link fine.
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/url", self.base, sid))
            .json(&serde_json::json!({ "url": url }))
            .send()
            .await
            .context("POST /session/:sid/url")?;
        ensure_wda_success(response, "POST /session/:sid/url").await?;
        Ok(())
    }

    /// Type a string into whatever currently has keyboard focus
    /// (`POST /wda/keys`). The string goes in as Unicode — CJK lands cleanly
    /// even with a Pinyin keyboard active (hardware-validated).
    pub async fn keys(&mut self, text: &str) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/wda/keys", self.base, sid))
            .json(&serde_json::json!({ "value": [text] }))
            .send()
            .await
            .context("POST /wda/keys")?;
        ensure_wda_success(response, "POST /wda/keys").await?;
        Ok(())
    }

    /// Send one named WebDriver key through WDA.
    ///
    /// WebDriver represents non-text keys with Unicode code points in the
    /// private-use range.  Keeping the mapping here gives HTTP, MCP, and the web
    /// client one device-native implementation instead of falling back to Mac
    /// keyboard events.
    ///
    /// Dispatched through the W3C Actions API (`POST /session/<id>/actions`)
    /// with a **key input source**, NOT through [`Self::keys`]. `/wda/keys`
    /// bottoms out in `FBKeyboard typeText:`, which is literal text entry: it
    /// never interprets the private-use code points, so it *typed* them into the
    /// focused field while WDA still answered `{"ok":true}` (issue #42 —
    /// `return` left the search unsubmitted, `delete` made the text longer).
    /// Only the Actions key source translates `\u{E007}` &co. into real key
    /// events. Plain text keeps using [`Self::keys`], which is correct for it.
    pub async fn named_key(&mut self, name: &str) -> Result<()> {
        let value = match name {
            // `send`/`go`/`search` are the same physical Return key an agent
            // reaches for to submit a chat message ("发送"), a URL bar ("前往"),
            // or a search field — coordinate-tapping a third-party keyboard's
            // 发送 glyph ACKs but does not fire, so route them to the real key
            // event instead (issue #63).
            "return" | "enter" | "send" | "go" | "search" => "\u{E007}",
            "escape" => "\u{E00C}",
            "space" => "\u{E00D}",
            "tab" => "\u{E004}",
            "delete" | "backspace" => "\u{E003}",
            "left" => "\u{E012}",
            "up" => "\u{E013}",
            "right" => "\u{E014}",
            "down" => "\u{E015}",
            _ => anyhow::bail!("unsupported named key: {name}"),
        };
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/actions", self.base, sid))
            .json(&serde_json::json!({
                "actions": [{
                    "type": "key",
                    "id": "keyboard",
                    "actions": [
                        { "type": "keyDown", "value": value },
                        { "type": "keyUp",   "value": value }
                    ]
                }]
            }))
            .send()
            .await
            .context("POST /actions (key)")?;
        ensure_wda_success(response, "POST /actions").await?;
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
    /// If no keyboard is present, WDA treats the native dismiss request as a
    /// successful no-op (the desired postcondition already holds). Transport
    /// failures and W3C error envelopes are returned to the caller, so an HTTP
    /// ACK never claims that an unconfirmed click landed.
    pub async fn dismiss_keyboard(&mut self) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        const LABELS: &[&str] = &[
            "Hide keyboard",
            "隐藏键盘",
            "キーボードを閉じる",
            "閉じる",
            "完了",
            "Done",
            "收起键盘",
            "關閉鍵盤",
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
        if let Ok(eid) = self.find_element("predicate string", &predicate).await {
            // Do not retry or fall back after dispatching the click: if its
            // response is lost, replaying another dismiss action would violate
            // the at-most-once contract used by Direct control.
            return self
                .click_element(&eid)
                .await
                .context("dismiss keyboard accessory click");
        }
        // No accessory button (native keyboard or already gone) — ask WDA to
        // establish the same postcondition with its native dismiss endpoint.
        let response = self
            .http
            .post(format!(
                "{}/session/{}/wda/keyboard/dismiss",
                self.base, sid
            ))
            .json(&serde_json::json!({ "keyNames": ["Done", "完了", "return", "前往", "search"] }))
            .send()
            .await
            .context("POST /wda/keyboard/dismiss")?;
        ensure_wda_success(response, "POST /wda/keyboard/dismiss").await?;
        Ok(())
    }

    /// Current phone screen as PNG bytes (`GET /screenshot`, base64 in the
    /// envelope). Works with no Mirroring window at all — the capture happens
    /// on the phone — so it's the L2 fallback when the L3 capture is gone.
    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        // Session-less endpoint; no ensure_session needed.
        let response = self
            .http
            .get(format!("{}/screenshot", self.base))
            .send()
            .await
            .context("GET /screenshot")?;
        let value = ensure_wda_success(response, "GET /screenshot").await?;
        let encoded = value
            .as_str()
            .ok_or_else(|| anyhow!("GET /screenshot returned non-string value"))?;
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
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

    /// One element's frame in WDA points (`GET .../element/:id/rect`).
    /// Used to match a semantic-less source-tree row onto its live XCUIElement
    /// when nothing but geometry identifies it (e.g. a bare `UISwitch`).
    pub async fn element_rect(&mut self, element_id: &str) -> Result<[f64; 4]> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .get(format!(
                "{}/session/{}/element/{}/rect",
                self.base, sid, element_id
            ))
            .send()
            .await
            .context("GET element/rect")?;
        let value = ensure_wda_success(response, "GET element/rect").await?;
        let g = |k: &str| value.get(k).and_then(serde_json::Value::as_f64);
        match (g("x"), g("y"), g("width"), g("height")) {
            (Some(x), Some(y), Some(w), Some(h)) => Ok([x, y, w, h]),
            _ => Err(anyhow!("GET element/rect returned bad frame: {value}")),
        }
    }

    /// The system alert (`UIAlertController`) currently on screen, through
    /// WDA's native alert routes: `(text, button names)`; `None` when no alert
    /// is up. Alerts are the one UI layer the flattened `/source` tree handles
    /// badly (hardware-hit on stock Settings: the alert was absent from the
    /// tree, and an element click on its button was ACKed without effect), so
    /// agents get them as a first-class block instead.
    pub async fn alert_summary(&mut self) -> Result<Option<(String, Vec<String>)>> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .get(format!("{}/session/{}/alert/text", self.base, sid))
            .send()
            .await
            .context("GET /alert/text")?;
        let text = match ensure_wda_success(response, "GET /alert/text").await {
            Ok(value) => value.as_str().unwrap_or("").to_string(),
            // W3C maps "no such alert" to HTTP 404.
            Err(error) if wda_error_is_not_found(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let response = self
            .http
            .get(format!("{}/session/{}/wda/alert/buttons", self.base, sid))
            .send()
            .await
            .context("GET /wda/alert/buttons")?;
        let buttons = match ensure_wda_success(response, "GET /wda/alert/buttons").await {
            Ok(value) => value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        Ok(Some((text, buttons)))
    }

    /// Press one alert button by name (`POST /alert/accept {"name"}`), or the
    /// default accept button when `button` is `None`.
    pub async fn alert_accept(&mut self, button: Option<&str>) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let body = match button {
            Some(name) => serde_json::json!({ "name": name }),
            None => serde_json::json!({}),
        };
        let response = self
            .http
            .post(format!("{}/session/{}/alert/accept", self.base, sid))
            .json(&body)
            .send()
            .await
            .context("POST /alert/accept")?;
        ensure_wda_success(response, "POST /alert/accept").await?;
        Ok(())
    }

    /// Dismiss the current alert (`POST /alert/dismiss`, the cancel/default
    /// dismiss button).
    pub async fn alert_dismiss(&mut self) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/alert/dismiss", self.base, sid))
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST /alert/dismiss")?;
        ensure_wda_success(response, "POST /alert/dismiss").await?;
        Ok(())
    }

    /// Drop the cached session (e.g. after an error that suggests it went
    /// stale); the next call re-creates one via [`Self::ensure_session`].
    pub fn invalidate_session(&mut self) {
        self.session = None;
    }

    /// `POST /session/:id/wda/lock` — lock the phone's screen.
    ///
    /// This is a generic WDA primitive. Backend selection is persisted at
    /// daemon startup and this method must not be used as a runtime mode switch.
    pub async fn lock(&mut self) -> Result<()> {
        let sid = self.ensure_session().await?.to_string();
        let response = self
            .http
            .post(format!("{}/session/{}/wda/lock", self.base, sid))
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("POST wda/lock")?;
        ensure_wda_success(response, "POST wda/lock").await?;
        Ok(())
    }
}

/// Bounded-snapshot WDA settings from the environment (issue #44), applied
/// once per created session. `PHONE_REMOTE_WDA_SNAPSHOT_MAX_DEPTH` maps to
/// WDA's `snapshotMaxDepth` (tree levels; WDA's own default is 50) and
/// `PHONE_REMOTE_WDA_SNAPSHOT_TIMEOUT_S` to `customSnapshotTimeout` (seconds;
/// WDA's default 15). Unset or unparseable values are simply omitted, so the
/// default daemon applies no settings at all.
fn snapshot_settings_from_env() -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut settings = serde_json::Map::new();
    if let Some(depth) = std::env::var("PHONE_REMOTE_WDA_SNAPSHOT_MAX_DEPTH")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|depth| *depth > 0)
    {
        settings.insert("snapshotMaxDepth".to_string(), depth.into());
    }
    if let Some(timeout) = std::env::var("PHONE_REMOTE_WDA_SNAPSHOT_TIMEOUT_S")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|timeout| timeout.is_finite() && *timeout > 0.0)
    {
        settings.insert("customSnapshotTimeout".to_string(), timeout.into());
    }
    (!settings.is_empty()).then_some(settings)
}

/// One row of the flattened element tree.
#[derive(Debug, Default, serde::Serialize, PartialEq)]
pub struct ElementRow {
    /// Element type without the `XCUIElementType` prefix (e.g. `Button`).
    pub kind: String,
    /// User-facing accessibility label. When WDA has no label, this falls back
    /// to its name so existing label-based clients keep a useful target.
    pub label: String,
    /// Stable accessibility identifier (`rawIdentifier` in WDA JSON), when the
    /// application supplies one. This is a durable locator candidate; unlike a
    /// snapshot index or WDA element id it may be persisted in a flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Position + size in WDA points: `[x, y, w, h]`.
    pub rect: [f64; 4],
    /// Tree depth (purely presentational).
    pub depth: u32,
    /// Current value/state, when the element has one (issue #20): a Switch's
    /// `"0"`/`"1"`, a Slider's fraction, a PickerWheel's selected option, a
    /// TextField's current text. `None` for elements without a value (most
    /// Buttons/Cells) so the JSON stays lean. This is what makes pickers /
    /// switches / sliders drivable without vision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// WDA emits these flags for every source node. Keep the common healthy
    /// state sparse to avoid inflating every MCP response: `enabled:false` and
    /// `visible:false` are emitted only for exceptional nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// Positive accessibility/focus state is useful for strict locators; false
    /// is the common state and is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    /// Text-input placeholder when WDA exposes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Derived non-default affordances (`PHONE_REMOTE_ELEMENTS_AFFORDANCES=1`
    /// only): the named `perform` actions this element supports beyond the
    /// universal tap/longpress family, from `type` + accessibility traits +
    /// min/max — e.g. `["increment","decrement","adjust"]`. Emitted only for
    /// kinds the daemon can actually drive, so plain Buttons/Cells/StaticText
    /// emit nothing and the default JSON stays byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    /// From the `Selected` accessibility trait (tab bars, segmented controls,
    /// filter chips); only `true` is emitted. Affordances flag only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    /// Slider/Stepper range, parsed from WDA's `minValue`/`maxValue` strings.
    /// Affordances flag only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Verbatim accessibility trait names (`PHONE_REMOTE_ELEMENTS_TRAITS=1`
    /// only, for debugging/forward-compat) — most values duplicate `kind`, so
    /// this is not part of the default or affordances payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traits: Option<Vec<String>>,
}

fn wda_bool(node: &serde_json::Value, key: &str) -> Option<bool> {
    match node.get(key)? {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::Number(value) => value.as_i64().and_then(|value| match value {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }),
        serde_json::Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "0" | "false" => Some(false),
            "1" | "true" => Some(true),
            _ => None,
        },
        _ => None,
    }
}

fn non_empty_string(node: &serde_json::Value, key: &str) -> Option<String> {
    node.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Element kinds an agent can act on directly. Shared between the tree
/// flattener (row inclusion) and the `/agent/elements` `ax_stats` block
/// (`n_interactive`), so the two can never drift apart.
pub const INTERACTIVE_KINDS: [&str; 11] = [
    "Button",
    "Cell",
    "TextField",
    "SecureTextField",
    "SearchField",
    "Switch",
    "Slider",
    "TextView",
    "PickerWheel",
    "Picker",
    "Stepper",
];

/// A finite number WDA emits either natively or as an `NSNumber.stringValue`
/// string (`minValue`/`maxValue` arrive as strings on v9.15.3).
fn wda_number(node: &serde_json::Value, key: &str) -> Option<f64> {
    match node.get(key)? {
        serde_json::Value::Number(value) => value.as_f64(),
        serde_json::Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

/// The node's `UIAccessibilityTraits` names, from WDA's comma-separated
/// `traits` string (e.g. `"Button, Selected"`).
fn node_traits(node: &serde_json::Value) -> Vec<String> {
    node.get("traits")
        .and_then(serde_json::Value::as_str)
        .map(|traits| {
            traits
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Derive the sparse non-default `perform` affordances for one element.
///
/// Only kinds the daemon can actually drive are listed (a bare `Adjustable`
/// trait on an `Other` row has no reachable WDA increment path, so emitting
/// it would advertise an action `perform` must refuse). Universal gestures
/// (tap, longpress/menu, double_tap, swipes) are deliberately NOT listed per
/// row — `perform` still accepts them everywhere.
fn derived_actions(
    kind: &str,
    traits: &[String],
    min: Option<f64>,
    max: Option<f64>,
) -> Option<Vec<String>> {
    let actions: &[&str] = match kind {
        "PickerWheel" | "Slider" => &["increment", "decrement", "adjust"],
        "Stepper" => &["increment", "decrement"],
        "Switch" => &["toggle"],
        // iOS 17+ marks on/off buttons that are not tree-level Switches.
        _ if traits.iter().any(|name| name == "ToggleButton") => &["toggle"],
        // A min/max pair on an otherwise untyped control is Stepper-like.
        _ if min.is_some() && max.is_some() => &["increment", "decrement"],
        _ => return None,
    };
    Some(actions.iter().map(|action| action.to_string()).collect())
}

/// Opt-in extras for [`flatten_tree`], read from the environment once per
/// flatten (mirroring the `snapshot_settings_from_env` opt-in pattern). Both
/// default to off, which keeps the emitted JSON byte-identical.
#[derive(Debug, Clone, Copy, Default)]
struct FlattenOptions {
    /// `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1`: emit sparse `actions`,
    /// `selected`, `min`, and `max` derived from traits + min/max values.
    affordances: bool,
    /// `PHONE_REMOTE_ELEMENTS_TRAITS=1`: emit the verbatim `traits` names.
    raw_traits: bool,
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value.trim() == "1")
}

fn flatten_options_from_env() -> FlattenOptions {
    FlattenOptions {
        affordances: env_flag("PHONE_REMOTE_ELEMENTS_AFFORDANCES"),
        raw_traits: env_flag("PHONE_REMOTE_ELEMENTS_TRAITS"),
    }
}

/// Recursively flatten a WDA `/source?format=json` tree, keeping only rows an
/// agent can act on or learn from: anything with a non-empty label, or an
/// interactive type. Order is document order (roughly top-to-bottom).
fn flatten_tree(node: &serde_json::Value, depth: u32, out: &mut Vec<ElementRow>) {
    flatten_tree_with(node, depth, out, flatten_options_from_env());
}

fn flatten_tree_with(
    node: &serde_json::Value,
    depth: u32,
    out: &mut Vec<ElementRow>,
    options: FlattenOptions,
) {
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
    if !label.is_empty() || INTERACTIVE_KINDS.contains(&kind.as_str()) {
        let r = node.get("rect").cloned().unwrap_or_default();
        let g = |k: &str| r.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        // Current value/state (issue #20). WDA reports `value` as a string
        // ("1"/"0" for a Switch), a number (Slider fraction), or a bool —
        // normalize to a non-empty string, else None.
        let value = node.get("value").and_then(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        });
        let enabled = wda_bool(node, "isEnabled").filter(|value| !value);
        let visible = wda_bool(node, "isVisible").filter(|value| !value);
        let accessible = wda_bool(node, "isAccessible").filter(|value| *value);
        let focused = wda_bool(node, "isFocused").filter(|value| *value);
        // Affordance extras (both env-gated; the default daemon parses none of
        // this and every field stays None → byte-identical serialized rows).
        let traits = if options.affordances || options.raw_traits {
            node_traits(node)
        } else {
            Vec::new()
        };
        let (actions, selected, min, max) = if options.affordances {
            let min = wda_number(node, "minValue");
            let max = wda_number(node, "maxValue");
            (
                derived_actions(&kind, &traits, min, max),
                traits.iter().any(|name| name == "Selected").then_some(true),
                min,
                max,
            )
        } else {
            (None, None, None, None)
        };
        let traits = (options.raw_traits && !traits.is_empty()).then_some(traits);
        out.push(ElementRow {
            kind,
            label,
            identifier: non_empty_string(node, "rawIdentifier"),
            rect: [g("x"), g("y"), g("width"), g("height")],
            depth,
            value,
            enabled,
            visible,
            accessible,
            focused,
            placeholder: non_empty_string(node, "placeholderValue"),
            actions,
            selected,
            min,
            max,
            traits,
        });
    }
    if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
        for c in children {
            flatten_tree_with(c, depth + 1, out, options);
        }
    }
}

/// Action-level WDA health (see [`WdaClient::probe_health`]). Distinguishes a
/// runner that merely answers `/status` from one that can actually act.
#[derive(Debug, Clone, Copy)]
pub struct WdaHealth {
    /// `/status` answered 2xx (runner process + HTTP server alive).
    pub up: bool,
    /// A real UI-test action succeeded — the testmanagerd connection is live.
    /// `up && !actionable` is the "zombie ready" state (locked phone / revoked
    /// automation / wedged tunnel) that `/status` alone cannot see.
    pub actionable: bool,
    /// Device lock state, when it could be read.
    pub locked: Option<bool>,
}

impl WdaHealth {
    /// Nothing reachable — WDA not configured or `/status` down.
    pub fn down() -> Self {
        Self {
            up: false,
            actionable: false,
            locked: None,
        }
    }
}

/// WDA answers 404 for W3C "no such alert" / "no such element" conditions;
/// `ensure_wda_success` surfaces that as an HTTP-status error.
pub fn wda_error_is_not_found(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("404 Not Found")
}

/// Require both HTTP success and a successful W3C `value` envelope.
///
/// WDA commonly reports command failures as HTTP 200 with
/// `{"value":{"error":...}}`. Every action that can produce a user-facing ACK
/// must consume and validate the body rather than trusting the status code.
async fn ensure_wda_success(
    response: reqwest::Response,
    operation: &str,
) -> Result<serde_json::Value> {
    let body = response
        .error_for_status()
        .with_context(|| format!("{operation} HTTP status"))?
        .text()
        .await
        .with_context(|| format!("{operation} response body"))?;
    parse_wda_value(&body, operation)
}

/// Parse a W3C response value while preserving error semantics. WDA sometimes
/// returns a JSON error envelope that still has a decodable `value`; treating
/// that object as a successful source tree or `false` lock state is unsafe.
fn parse_wda_value(body: &str, operation: &str) -> Result<serde_json::Value> {
    let root: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("{operation} response is not JSON: {body}"))?;
    let value = root
        .get("value")
        .ok_or_else(|| anyhow!("{operation} response has no value: {body}"))?;
    if let Some(code) = value.get("error").and_then(serde_json::Value::as_str) {
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no message");
        return Err(anyhow!("{operation} failed ({code}): {message}"));
    }
    Ok(value.clone())
}

/// Pull the session id out of a `POST /session` response, accepting both the
/// top-level `sessionId` (older WDA) and `value.sessionId` (W3C) placements.
fn parse_session_id(body: &str) -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(body).with_context(|| format!("session resp not JSON: {body}"))?;
    if let Some(code) = v
        .get("value")
        .and_then(|value| value.get("error"))
        .and_then(serde_json::Value::as_str)
    {
        let message = v
            .get("value")
            .and_then(|value| value.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no message");
        return Err(anyhow!("POST /session failed ({code}): {message}"));
    }
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
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn touch_move_duration_is_bounded() {
        assert_eq!(bounded_move_duration_ms(0), 80);
        assert_eq!(bounded_move_duration_ms(80), 80);
        assert_eq!(bounded_move_duration_ms(450), 450);
        assert_eq!(bounded_move_duration_ms(2_000), 2_000);
        assert_eq!(bounded_move_duration_ms(u64::MAX), 2_000);
    }

    fn mock_wda(
        requests: usize,
        responder: impl Fn(&str) -> String + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 8_192];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let body = responder(&request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}"), task)
    }

    fn block<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

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
    fn session_id_rejects_w3c_error_envelope() {
        let error = parse_session_id(
            r#"{"value":{"error":"session not created","message":"automation unavailable"}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("session not created"), "{error}");
        assert!(error.contains("automation unavailable"), "{error}");
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
    fn wda_value_rejects_w3c_error_envelope() {
        let body = r#"{"value":{"error":"invalid element state","message":"device is locked"}}"#;
        let error = parse_wda_value(body, "GET /source")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid element state"));
        assert!(error.contains("device is locked"));
    }

    #[test]
    fn wda_value_returns_success_payload() {
        let body = r#"{"value":{"type":"XCUIElementTypeApplication","children":[]}}"#;
        let value = parse_wda_value(body, "GET /source").unwrap();
        assert_eq!(
            value.get("type").and_then(serde_json::Value::as_str),
            Some("XCUIElementTypeApplication")
        );
    }

    #[test]
    fn source_uses_sessionless_active_application_endpoint() {
        let (base, server) = mock_wda(1, |request| {
            assert!(request.starts_with("GET /source?format=json "), "{request}");
            assert!(!request.contains("/session/"), "{request}");
            r#"{"value":{"type":"XCUIElementTypeApplication","label":"Files","children":[]}}"#
                .to_string()
        });
        let mut client = WdaClient::new(base).unwrap();
        client.session = Some("STALE-APP-SCOPED-SESSION".to_string());

        let source = block(client.source()).unwrap();
        server.join().unwrap();

        assert_eq!(
            source.get("label").and_then(serde_json::Value::as_str),
            Some("Files")
        );
        assert_eq!(client.session.as_deref(), Some("STALE-APP-SCOPED-SESSION"));
    }

    #[test]
    fn action_rejects_http_200_w3c_error_envelope() {
        let (base, server) = mock_wda(1, |_| {
            r#"{"value":{"error":"invalid session id","message":"response lost"}}"#.to_string()
        });
        let mut client = WdaClient::new(base).unwrap();
        client.session = Some("SESSION".to_string());

        let error = block(client.tap_point(10.0, 20.0)).unwrap_err().to_string();
        server.join().unwrap();

        assert!(error.contains("invalid session id"), "{error}");
        assert!(error.contains("response lost"), "{error}");
    }

    #[test]
    fn named_key_uses_actions_key_source_not_wda_keys() {
        // issue #42: /wda/keys goes through `FBKeyboard typeText:`, which typed
        // the private-use code point into the field instead of pressing the key.
        // Only a key input source on /actions produces a real key event.
        let (base, server) = mock_wda(1, |request| {
            assert!(
                request.starts_with("POST /session/SESSION/actions "),
                "{request}"
            );
            assert!(!request.contains("/wda/keys"), "{request}");
            assert!(request.contains(r#""type":"key""#), "{request}");
            assert!(request.contains(r#""id":"keyboard""#), "{request}");
            assert!(request.contains(r#""keyDown""#), "{request}");
            assert!(request.contains(r#""keyUp""#), "{request}");
            // U+E007 (WebDriver "Enter") travels as raw UTF-8 in the JSON body.
            assert!(request.contains('\u{E007}'), "{request}");
            r#"{"value":null}"#.to_string()
        });
        let mut client = WdaClient::new(base).unwrap();
        client.session = Some("SESSION".to_string());

        let sent = block(client.named_key("return"));
        server.join().unwrap();
        sent.unwrap();
    }

    #[test]
    fn named_key_send_go_search_alias_the_return_key() {
        // issue #63: coordinate-tapping a third-party keyboard's 发送 / 前往 key
        // ACKs but does not submit; `send`/`go`/`search` must map to the real
        // Return key event (U+E007) so a chat/search actually fires.
        for name in ["send", "go", "search"] {
            let (base, server) = mock_wda(1, |request| {
                assert!(request.contains(r#""type":"key""#), "{request}");
                assert!(request.contains('\u{E007}'), "{request}");
                r#"{"value":null}"#.to_string()
            });
            let mut client = WdaClient::new(base).unwrap();
            client.session = Some("SESSION".to_string());
            let sent = block(client.named_key(name));
            server.join().unwrap();
            sent.unwrap_or_else(|error| panic!("{name}: {error}"));
        }
    }

    #[test]
    fn named_key_rejects_unsupported_name() {
        let mut client = WdaClient::new("http://127.0.0.1:1".to_string()).unwrap();
        client.session = Some("SESSION".to_string());

        let error = block(client.named_key("f13")).unwrap_err().to_string();
        assert!(error.contains("unsupported named key: f13"), "{error}");
    }

    #[test]
    fn health_rejects_active_apps_http_200_error_envelope() {
        let (base, server) = mock_wda(3, |request| {
            if request.contains("/wda/locked") {
                r#"{"value":false}"#.to_string()
            } else if request.contains("/wda/apps/list") {
                r#"{"value":{"error":"invalid element state","message":"device is locked"}}"#
                    .to_string()
            } else {
                r#"{"value":{"ready":true}}"#.to_string()
            }
        });
        let mut client = WdaClient::new(base).unwrap();
        client.session = Some("SESSION".to_string());

        let health = block(client.probe_health());
        server.join().unwrap();

        assert!(health.up);
        assert!(!health.actionable);
        assert_eq!(health.locked, Some(false));
    }

    #[test]
    fn health_rejects_locked_device_even_when_active_apps_succeeds() {
        let (base, server) = mock_wda(3, |request| {
            if request.contains("/wda/locked") {
                r#"{"value":true}"#.to_string()
            } else if request.contains("/wda/apps/list") {
                r#"{"value":[{"pid":123,"bundleId":"com.apple.springboard"}]}"#.to_string()
            } else {
                r#"{"value":{"ready":true}}"#.to_string()
            }
        });
        let mut client = WdaClient::new(base).unwrap();
        client.session = Some("SESSION".to_string());

        let health = block(client.probe_health());
        server.join().unwrap();

        assert!(health.up);
        assert!(!health.actionable);
        assert_eq!(health.locked, Some(true));
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
                   "rawIdentifier":"new-note-button",
                   "isEnabled":"1","isVisible":"1","isAccessible":"1","isFocused":"0",
                   "rect":{"x":369,"y":885,"width":38,"height":38}},
                  {"type":"XCUIElementTypeStaticText","label":"你好世界",
                   "isEnabled":true,"isVisible":0,
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
        assert_eq!(rows[0].identifier.as_deref(), Some("new-note-button"));
        assert_eq!(rows[0].rect, [369.0, 885.0, 38.0, 38.0]);
        assert_eq!(rows[0].enabled, None);
        assert_eq!(rows[0].visible, None);
        assert_eq!(rows[0].accessible, Some(true));
        assert_eq!(rows[0].focused, None);
        assert_eq!(rows[1].label, "你好世界");
        assert_eq!(rows[1].visible, Some(false));
        // Plain button has no value
        assert_eq!(rows[0].value, None);
    }

    #[test]
    fn flatten_captures_value_for_switch_slider_picker() {
        // issue #20: switches/sliders/pickers must carry their value/state so an
        // agent can drive them without vision. Includes a value-bearing element
        // with NO label (a Switch) to prove INTERACTIVE inclusion + value.
        let tree: serde_json::Value = serde_json::from_str(
            r#"{
              "type":"XCUIElementTypeApplication","label":"",
              "children":[
                {"type":"XCUIElementTypeSwitch","label":"Wi-Fi","value":"1",
                 "rect":{"x":300,"y":100,"width":51,"height":31}},
                {"type":"XCUIElementTypeSwitch","label":"","value":0,
                 "rect":{"x":300,"y":160,"width":51,"height":31}},
                {"type":"XCUIElementTypeSlider","label":"亮度","value":"45%",
                 "rect":{"x":20,"y":200,"width":380,"height":30}},
                {"type":"XCUIElementTypePickerWheel","label":"","value":"March",
                 "rect":{"x":40,"y":300,"width":120,"height":200}}
              ]
            }"#,
        )
        .unwrap();
        let mut rows = Vec::new();
        flatten_tree(&tree, 0, &mut rows);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].value.as_deref(), Some("1")); // string switch
        assert_eq!(rows[1].kind, "Switch"); // unlabeled but interactive → kept
        assert_eq!(rows[1].value.as_deref(), Some("0")); // numeric → "0"
        assert_eq!(rows[2].value.as_deref(), Some("45%")); // slider
        assert_eq!(rows[3].value.as_deref(), Some("March")); // picker wheel
    }

    #[test]
    fn flatten_keeps_sparse_locator_state_and_placeholder() {
        let tree = serde_json::json!({
            "type": "XCUIElementTypeApplication",
            "children": [{
                "type": "XCUIElementTypeTextField",
                "label": "搜索",
                "rawIdentifier": "search-field",
                "placeholderValue": "搜索笔记",
                "isEnabled": "0",
                "isVisible": "false",
                "isAccessible": 1,
                "isFocused": true,
                "rect": {"x": 20, "y": 40, "width": 300, "height": 44}
            }]
        });
        let mut rows = Vec::new();
        flatten_tree(&tree, 0, &mut rows);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.identifier.as_deref(), Some("search-field"));
        assert_eq!(row.placeholder.as_deref(), Some("搜索笔记"));
        assert_eq!(row.enabled, Some(false));
        assert_eq!(row.visible, Some(false));
        assert_eq!(row.accessible, Some(true));
        assert_eq!(row.focused, Some(true));

        let json = serde_json::to_value(row).unwrap();
        assert_eq!(json["identifier"], "search-field");
        assert_eq!(json["placeholder"], "搜索笔记");
        assert_eq!(json["enabled"], false);
        assert_eq!(json["visible"], false);
        assert_eq!(json["accessible"], true);
        assert_eq!(json["focused"], true);
    }

    #[test]
    fn flatten_omits_common_locator_state_from_json() {
        let tree = serde_json::json!({
            "type": "XCUIElementTypeButton",
            "label": "发布",
            "isEnabled": "1",
            "isVisible": true,
            "isAccessible": "0",
            "isFocused": 0,
            "rect": {"x": 300, "y": 40, "width": 60, "height": 44}
        });
        let mut rows = Vec::new();
        flatten_tree(&tree, 0, &mut rows);

        let json = serde_json::to_value(&rows[0]).unwrap();
        assert!(json.get("enabled").is_none());
        assert!(json.get("visible").is_none());
        assert!(json.get("accessible").is_none());
        assert!(json.get("focused").is_none());
    }

    /// A tree that carries `traits`/`minValue`/`maxValue`; the affordance
    /// tests below all parse this one fixture.
    fn affordance_tree() -> serde_json::Value {
        serde_json::json!({
            "type": "XCUIElementTypeApplication",
            "children": [
                {"type": "XCUIElementTypeSwitch", "label": "Wi-Fi", "value": "1",
                 "traits": "Button",
                 "rect": {"x": 300, "y": 100, "width": 51, "height": 31}},
                {"type": "XCUIElementTypeSlider", "label": "亮度", "value": "45%",
                 "traits": "Adjustable", "minValue": "0", "maxValue": "1",
                 "rect": {"x": 20, "y": 200, "width": 380, "height": 30}},
                {"type": "XCUIElementTypePickerWheel", "label": "", "value": "三月",
                 "traits": "Adjustable",
                 "rect": {"x": 40, "y": 300, "width": 120, "height": 200}},
                {"type": "XCUIElementTypeStepper", "label": "份数",
                 "traits": "Adjustable", "minValue": "1", "maxValue": "10",
                 "rect": {"x": 40, "y": 520, "width": 94, "height": 32}},
                {"type": "XCUIElementTypeButton", "label": "浏览",
                 "traits": "Button, Selected",
                 "rect": {"x": 0, "y": 900, "width": 110, "height": 48}},
                {"type": "XCUIElementTypeButton", "label": "静音",
                 "traits": "ToggleButton",
                 "rect": {"x": 120, "y": 900, "width": 110, "height": 48}},
                {"type": "XCUIElementTypeOther", "label": "自定义调节",
                 "traits": "Adjustable",
                 "rect": {"x": 20, "y": 600, "width": 400, "height": 44}}
            ]
        })
    }

    #[test]
    fn flatten_default_is_byte_identical_with_traits_present() {
        // With the affordance flags unset (the default in this test process),
        // a tree carrying traits/minValue/maxValue must serialize exactly like
        // the same tree without them — existing snapshot-token hashes and
        // clients see no difference.
        let with_extras = affordance_tree();
        let mut without_extras = with_extras.clone();
        for child in without_extras["children"].as_array_mut().unwrap() {
            let child = child.as_object_mut().unwrap();
            child.remove("traits");
            child.remove("minValue");
            child.remove("maxValue");
        }

        let mut rows = Vec::new();
        flatten_tree(&with_extras, 0, &mut rows);
        let mut plain_rows = Vec::new();
        flatten_tree(&without_extras, 0, &mut plain_rows);

        assert_eq!(rows, plain_rows);
        assert_eq!(
            serde_json::to_string(&rows).unwrap(),
            serde_json::to_string(&plain_rows).unwrap()
        );
        assert!(!serde_json::to_string(&rows).unwrap().contains("actions"));
    }

    #[test]
    fn flatten_with_affordances_derives_actions_selected_and_range() {
        let mut rows = Vec::new();
        flatten_tree_with(
            &affordance_tree(),
            0,
            &mut rows,
            FlattenOptions {
                affordances: true,
                raw_traits: false,
            },
        );
        assert_eq!(rows.len(), 7);

        let by_kind = |kind: &str| rows.iter().find(|row| row.kind == kind).unwrap();
        assert_eq!(
            by_kind("Switch").actions.as_deref(),
            Some(&["toggle".to_string()][..])
        );
        let slider = by_kind("Slider");
        assert_eq!(
            slider.actions.as_deref(),
            Some(
                &[
                    "increment".to_string(),
                    "decrement".to_string(),
                    "adjust".to_string()
                ][..]
            )
        );
        assert_eq!(slider.min, Some(0.0));
        assert_eq!(slider.max, Some(1.0));
        assert_eq!(
            by_kind("PickerWheel").actions.as_deref(),
            Some(
                &[
                    "increment".to_string(),
                    "decrement".to_string(),
                    "adjust".to_string()
                ][..]
            )
        );
        let stepper = by_kind("Stepper");
        assert_eq!(
            stepper.actions.as_deref(),
            Some(&["increment".to_string(), "decrement".to_string()][..])
        );
        assert_eq!(stepper.min, Some(1.0));
        assert_eq!(stepper.max, Some(10.0));

        // Selected trait → state, not an action; the plain tab Button gets
        // no actions list at all.
        let tab = rows.iter().find(|row| row.label == "浏览").unwrap();
        assert_eq!(tab.selected, Some(true));
        assert_eq!(tab.actions, None);

        // ToggleButton trait on a Button → toggle.
        let mute = rows.iter().find(|row| row.label == "静音").unwrap();
        assert_eq!(mute.actions.as_deref(), Some(&["toggle".to_string()][..]));
        assert_eq!(mute.selected, None);

        // Bare Adjustable on an untyped row has no reachable WDA increment
        // path — advertise nothing rather than an action perform must refuse.
        let custom = by_kind("Other");
        assert_eq!(custom.actions, None);

        // Raw traits stay behind their own flag.
        assert!(rows.iter().all(|row| row.traits.is_none()));
    }

    #[test]
    fn flatten_with_raw_traits_emits_verbatim_names() {
        let mut rows = Vec::new();
        flatten_tree_with(
            &affordance_tree(),
            0,
            &mut rows,
            FlattenOptions {
                affordances: false,
                raw_traits: true,
            },
        );
        let tab = rows.iter().find(|row| row.label == "浏览").unwrap();
        assert_eq!(
            tab.traits.as_deref(),
            Some(&["Button".to_string(), "Selected".to_string()][..])
        );
        // Traits alone never derive actions/selected/min/max.
        assert!(rows.iter().all(|row| row.actions.is_none()
            && row.selected.is_none()
            && row.min.is_none()
            && row.max.is_none()));
        let switch = rows.iter().find(|row| row.kind == "Switch").unwrap();
        assert_eq!(switch.traits.as_deref(), Some(&["Button".to_string()][..]));
    }

    #[test]
    fn wda_number_parses_nsnumber_strings_and_numbers() {
        let node = serde_json::json!({
            "minValue": "0.25",
            "maxValue": 10,
            "bad": "wide open",
            "inf": "inf"
        });
        assert_eq!(wda_number(&node, "minValue"), Some(0.25));
        assert_eq!(wda_number(&node, "maxValue"), Some(10.0));
        assert_eq!(wda_number(&node, "bad"), None);
        assert_eq!(wda_number(&node, "inf"), None);
        assert_eq!(wda_number(&node, "missing"), None);
    }
}
