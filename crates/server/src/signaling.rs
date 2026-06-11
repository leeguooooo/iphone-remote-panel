//! `/ws` signaling state machine — the daemon is the **offerer**.
//!
//! Handshake (mirrors `web/index.html`):
//!   1. On connect the daemon builds a [`RTCPeerConnection`]
//!      ([`crate::webrtc::build_viewer_pc`]), `create_offer` +
//!      `set_local_description`, and sends `{type:"offer", sdp}`.
//!   2. The client replies `{type:"answer", sdp}` → `set_remote_description`.
//!   3. Trickle ICE both ways via `{type:"ice", candidate}`.
//!   4. The client may send `{type:"restart"}` → `create_offer({iceRestart})` and
//!      re-send as `offer`.
//!
//! One viewer per WebSocket. The viewer holds a `Human(session_id)` control lease
//! for the life of the socket; input injection is gated on it
//! ([`crate::input_bridge`]).

use std::sync::Arc;

/// Recover a poisoned mutex guard instead of panicking. A panic inside a lock
/// holder would otherwise permanently disable the control-lease subsystem; the
/// data is a small state struct that stays consistent, so unwrapping the poison
/// error is safe here.
#[inline]
fn recover<T>(r: std::sync::LockResult<T>) -> T {
    r.unwrap_or_else(std::sync::PoisonError::into_inner)
}

use axum::extract::ws::{Message, WebSocket};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::http::AppState;

/// A signaling message on the `/ws` channel (both directions).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SignalMsg {
    /// Daemon → client: the SDP offer.
    Offer { sdp: String },
    /// Client → daemon: the SDP answer.
    Answer { sdp: String },
    /// Either direction: a trickled ICE candidate (browser candidate JSON shape).
    Ice {
        candidate: serde_json::Value,
    },
    /// Client → daemon: request an ICE restart (re-offer).
    Restart,
    /// Daemon → client: whether the phone's Mirroring window is present.
    ///
    /// Pushed on connect and on transitions. Needed because the static-screen
    /// keepalive keeps re-encoding the last frame when the window vanishes, so
    /// the client cannot infer "phone offline" from frame flow alone.
    #[serde(rename = "phone_status")]
    PhoneStatus { present: bool },
    /// Daemon → client: this viewer's place in the single-viewer queue (issue #8).
    ///
    /// `state` is `"queued"` when another viewer holds the live session — the
    /// client should show a "session in use" overlay and wait (no offer comes
    /// yet) — or `"active"` the moment this viewer is promoted (an offer follows
    /// immediately). `ahead` is how many viewers are in front (0 when active).
    #[serde(rename = "session_status")]
    SessionStatus { state: String, ahead: usize },
}

/// Single-active-viewer arbitration for `/ws` (issue #8: "queue + notify").
///
/// One viewer streams at a time; the rest wait in FIFO order and are told
/// `session_status: queued`. When the active viewer disconnects the front of
/// the queue is promoted and its [`Notify`] is woken so its session can build a
/// PeerConnection and send the offer. This avoids two PeerConnections racing
/// over the one capture pipeline (the previously-undefined behavior).
///
/// [`Notify`]: tokio::sync::Notify
#[derive(Default)]
pub struct ViewerRegistry {
    /// Session id of the viewer currently allowed to stream (`None` = idle).
    active: Option<String>,
    /// Waiting viewers in arrival order, each with the notifier that promotes it.
    queue: std::collections::VecDeque<Waiter>,
}

struct Waiter {
    id: String,
    notify: Arc<tokio::sync::Notify>,
}

/// Outcome of [`ViewerRegistry::join`].
#[derive(Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    /// No one was streaming — this viewer is active immediately.
    Active,
    /// Someone else is active — this viewer waits; `ahead` are in front of it.
    Queued { ahead: usize },
}

impl ViewerRegistry {
    /// Register `id`. Becomes active if the session is idle, else is queued
    /// (its `notify` is stored so [`Self::leave`] can promote it later).
    pub fn join(&mut self, id: String, notify: Arc<tokio::sync::Notify>) -> JoinOutcome {
        if self.active.is_none() {
            self.active = Some(id);
            JoinOutcome::Active
        } else {
            let ahead = self.queue.len() + 1; // +1 for the active viewer ahead
            self.queue.push_back(Waiter { id, notify });
            JoinOutcome::Queued { ahead }
        }
    }

    /// Remove `id`. If it was the active viewer, promote the front of the queue
    /// and return its notifier (the caller wakes it). Removing a still-queued
    /// viewer just drops it from the line and returns `None`.
    pub fn leave(&mut self, id: &str) -> Option<Arc<tokio::sync::Notify>> {
        if self.active.as_deref() == Some(id) {
            self.active = None;
            if let Some(next) = self.queue.pop_front() {
                self.active = Some(next.id);
                return Some(next.notify);
            }
            return None;
        }
        // Not active — drop it from the queue if present.
        if let Some(pos) = self.queue.iter().position(|w| w.id == id) {
            self.queue.remove(pos);
        }
        None
    }

    /// Total connected viewers (active + waiting). Surfaced as `viewer_count`.
    pub fn count(&self) -> usize {
        self.active.is_some() as usize + self.queue.len()
    }

    /// 1-based position of `id` in the queue (number of viewers ahead), or
    /// `None` if it isn't waiting. Used to refresh a viewer's `ahead` after the
    /// line shrinks.
    pub fn ahead_of(&self, id: &str) -> Option<usize> {
        self.queue.iter().position(|w| w.id == id).map(|i| i + 1)
    }
}

/// Drive one viewer WebSocket session to completion.
///
/// Acquires the control lease for `session_id`, builds the PeerConnection, sends
/// the offer, and pumps signaling messages until the socket closes.
pub async fn run_session(mut socket: WebSocket, state: Arc<AppState>, session_id: String) {
    // Single-active-viewer policy (issue #8: queue + notify). Join the registry;
    // if another viewer is live we wait in line instead of opening a second
    // PeerConnection that would race over the one capture pipeline.
    let promote = Arc::new(tokio::sync::Notify::new());
    let outcome = {
        let mut reg = recover(state.viewers.lock());
        reg.join(session_id.clone(), promote.clone())
    };
    if let JoinOutcome::Queued { ahead } = outcome {
        let _ = send_json(
            &mut socket,
            &SignalMsg::SessionStatus { state: "queued".into(), ahead },
        )
        .await;
        // Wait until promoted (active viewer left → our Notify fires) or the
        // client gives up (socket closes). Returns false on disconnect.
        if !wait_for_promotion(&mut socket, &promote).await {
            leave_and_promote(&state, &session_id);
            return;
        }
        // Promoted: an offer follows immediately; tell the client to drop the
        // "session in use" overlay.
        let _ = send_json(
            &mut socket,
            &SignalMsg::SessionStatus { state: "active".into(), ahead: 0 },
        )
        .await;
    }

    // --- We are now the single active viewer. ---
    // Acquire the human control lease. Keep OUR lease handle locally so teardown
    // only releases/clears the shared slot if WE still hold it (an agent's input
    // lease can supersede ours mid-session; a blind take() would rip theirs out).
    let my_lease = {
        let mut control = recover(state.control.lock());
        let lease = control.acquire(core::control::Holder::Human(session_id.clone()), now_secs());
        *recover(state.current_lease.lock()) = Some(lease.clone());
        lease
    };

    // Outbound queue: PC callbacks (ICE candidates) and the offer push here.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMsg>();

    let pc = match build_pc(&state, &out_tx).await {
        Ok(pc) => pc,
        Err(e) => {
            tracing::error!("failed to build peer connection: {e}");
            return;
        }
    };

    // Send the initial offer.
    if let Err(e) = send_offer(&pc, &out_tx, false).await {
        tracing::error!("failed to create/send offer: {e}");
        return;
    }

    // Phone-presence push: tell the client the current window state up front,
    // then again on every transition (polled below). See SignalMsg::PhoneStatus.
    let mut phone_present = state.pipeline.phone_present();
    let _ = out_tx.send(SignalMsg::PhoneStatus { present: phone_present });
    let mut presence_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    presence_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Pump: forward outbound signaling to the socket, and handle inbound messages.
    loop {
        tokio::select! {
            // Phone-presence poll → push on transition.
            _ = presence_tick.tick() => {
                let present = state.pipeline.phone_present();
                if present != phone_present {
                    phone_present = present;
                    let _ = out_tx.send(SignalMsg::PhoneStatus { present });
                }
            }
            // Outbound signaling (offer / ICE candidates) → socket.
            maybe_out = out_rx.recv() => {
                match maybe_out {
                    Some(msg) => {
                        if let Ok(json) = serde_json::to_string(&msg) {
                            if socket.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    None => break,
                }
            }
            // Inbound from the client.
            maybe_in = socket.recv() => {
                match maybe_in {
                    Some(Ok(Message::Text(text))) => {
                        if !handle_inbound(&pc, &out_tx, &text).await {
                            // fatal protocol error — keep the socket open but log.
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ignore binary/ping/pong
                    Some(Err(_)) => break,
                }
            }
        }
    }

    // Tear down: release OUR lease only if we are still the current holder.
    // If an agent superseded us, the slot holds THEIR lease — taking/clearing it
    // would silently gate out their input.
    {
        let mut control = recover(state.control.lock());
        if control.is_current(&my_lease) {
            control.release(&my_lease);
            *recover(state.current_lease.lock()) = None;
        }
    }
    let _ = pc.close().await;
    // Hand the live slot to the next viewer in line (issue #8).
    leave_and_promote(&state, &session_id);
    tracing::debug!("viewer session {session_id} ended");
}

/// Serialize and send one signaling message straight to the socket (used outside
/// the main pump, for queue-status pushes). Best-effort.
async fn send_json(socket: &mut WebSocket, msg: &SignalMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => socket.send(Message::Text(json)).await.is_ok(),
        Err(_) => false,
    }
}

/// Block a queued viewer until it is promoted (`promote` fires) or its socket
/// closes. Returns `true` if promoted, `false` if the client disconnected while
/// waiting. Inbound frames from a waiting client are ignored (no PC yet).
async fn wait_for_promotion(socket: &mut WebSocket, promote: &Arc<tokio::sync::Notify>) -> bool {
    loop {
        tokio::select! {
            _ = promote.notified() => return true,
            maybe_in = socket.recv() => match maybe_in {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return false,
                _ => continue, // ignore anything a waiting client sends
            }
        }
    }
}

/// Remove this viewer from the registry; if it was active, wake the newly
/// promoted front-of-queue viewer so its session can start streaming.
fn leave_and_promote(state: &Arc<AppState>, session_id: &str) {
    let next = {
        let mut reg = recover(state.viewers.lock());
        reg.leave(session_id)
    };
    if let Some(notify) = next {
        notify.notify_one();
    }
}

/// Build the PeerConnection and wire its ICE-candidate callback to `out_tx`.
async fn build_pc(
    state: &Arc<AppState>,
    out_tx: &mpsc::UnboundedSender<SignalMsg>,
) -> anyhow::Result<Arc<RTCPeerConnection>> {
    let ice_servers = state.ice.load().servers.clone();
    let pc = crate::webrtc::build_viewer_pc(
        ice_servers,
        state.pipeline.clone(),
        state.injector.clone(),
    )
    .await?;

    // Trickle ICE: forward each gathered candidate to the client.
    let tx = out_tx.clone();
    pc.on_ice_candidate(Box::new(move |cand| {
        let tx = tx.clone();
        Box::pin(async move {
            if let Some(c) = cand {
                if let Ok(init) = c.to_json() {
                    if let Ok(value) = serde_json::to_value(&init) {
                        let _ = tx.send(SignalMsg::Ice { candidate: value });
                    }
                }
            }
        })
    }));

    Ok(pc)
}

/// Create an offer (optionally with ICE restart), set it as local description,
/// and enqueue it for the client.
async fn send_offer(
    pc: &Arc<RTCPeerConnection>,
    out_tx: &mpsc::UnboundedSender<SignalMsg>,
    ice_restart: bool,
) -> anyhow::Result<()> {
    use webrtc::peer_connection::offer_answer_options::RTCOfferOptions;
    let opts = ice_restart.then_some(RTCOfferOptions {
        ice_restart: true,
        ..Default::default()
    });
    let offer = pc.create_offer(opts).await?;
    pc.set_local_description(offer.clone()).await?;
    let _ = out_tx.send(SignalMsg::Offer { sdp: offer.sdp });
    Ok(())
}

/// Handle one inbound signaling message. Returns `false` on a recoverable error.
async fn handle_inbound(
    pc: &Arc<RTCPeerConnection>,
    out_tx: &mpsc::UnboundedSender<SignalMsg>,
    text: &str,
) -> bool {
    let msg: SignalMsg = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("unparseable signaling message: {e}");
            return false;
        }
    };
    match msg {
        SignalMsg::Answer { sdp } => {
            match RTCSessionDescription::answer(sdp) {
                Ok(answer) => {
                    if let Err(e) = pc.set_remote_description(answer).await {
                        tracing::warn!("set_remote_description(answer) failed: {e}");
                        return false;
                    }
                }
                Err(e) => {
                    tracing::warn!("parse answer failed: {e}");
                    return false;
                }
            }
            true
        }
        SignalMsg::Ice { candidate } => {
            // The client sends the browser RTCIceCandidate JSON shape; map it onto
            // RTCIceCandidateInit (candidate / sdpMid / sdpMLineIndex / ...).
            match serde_json::from_value::<RTCIceCandidateInit>(candidate) {
                Ok(init) => {
                    if let Err(e) = pc.add_ice_candidate(init).await {
                        tracing::debug!("add_ice_candidate failed: {e}");
                    }
                    true
                }
                Err(e) => {
                    tracing::debug!("bad ICE candidate: {e}");
                    false
                }
            }
        }
        SignalMsg::Restart => {
            if let Err(e) = send_offer(pc, out_tx, true).await {
                tracing::warn!("ICE restart offer failed: {e}");
                return false;
            }
            true
        }
        SignalMsg::Offer { .. } | SignalMsg::PhoneStatus { .. } | SignalMsg::SessionStatus { .. } => {
            // Daemon-to-client-only messages; inbound copies are protocol errors.
            tracing::debug!("unexpected inbound daemon-only message; ignoring");
            false
        }
    }
}

/// Unix time in seconds (for the control lease clock).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_serializes_with_type_tag() {
        let msg = SignalMsg::Offer {
            sdp: "v=0...".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"offer""#), "got {json}");
        assert!(json.contains(r#""sdp":"v=0...""#));
    }

    #[test]
    fn answer_parses_from_client_shape() {
        let json = r#"{"type":"answer","sdp":"v=0..."}"#;
        let msg: SignalMsg = serde_json::from_str(json).unwrap();
        match msg {
            SignalMsg::Answer { sdp } => assert_eq!(sdp, "v=0..."),
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn ice_parses_candidate_object() {
        let json =
            r#"{"type":"ice","candidate":{"candidate":"candidate:1 1 udp ...","sdpMid":"0","sdpMLineIndex":0}}"#;
        let msg: SignalMsg = serde_json::from_str(json).unwrap();
        match msg {
            SignalMsg::Ice { candidate } => {
                assert_eq!(candidate["sdpMid"], "0");
            }
            other => panic!("expected Ice, got {other:?}"),
        }
    }

    #[test]
    fn restart_parses() {
        let msg: SignalMsg = serde_json::from_str(r#"{"type":"restart"}"#).unwrap();
        assert!(matches!(msg, SignalMsg::Restart));
    }

    #[test]
    fn phone_status_serializes_with_snake_case_tag() {
        let json = serde_json::to_string(&SignalMsg::PhoneStatus { present: false }).unwrap();
        assert_eq!(json, r#"{"type":"phone_status","present":false}"#);
    }

    #[test]
    fn session_status_serializes_with_snake_case_tag() {
        let json = serde_json::to_string(&SignalMsg::SessionStatus {
            state: "queued".into(),
            ahead: 2,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"session_status","state":"queued","ahead":2}"#);
    }

    fn notify() -> Arc<tokio::sync::Notify> {
        Arc::new(tokio::sync::Notify::new())
    }

    #[test]
    fn registry_first_viewer_is_active_rest_queue() {
        let mut reg = ViewerRegistry::default();
        assert_eq!(reg.join("a".into(), notify()), JoinOutcome::Active);
        assert_eq!(reg.join("b".into(), notify()), JoinOutcome::Queued { ahead: 1 });
        assert_eq!(reg.join("c".into(), notify()), JoinOutcome::Queued { ahead: 2 });
        assert_eq!(reg.count(), 3);
        assert_eq!(reg.ahead_of("b"), Some(1));
        assert_eq!(reg.ahead_of("c"), Some(2));
        assert_eq!(reg.ahead_of("a"), None); // active, not queued
    }

    #[test]
    fn registry_active_leaving_promotes_front_of_queue() {
        let mut reg = ViewerRegistry::default();
        reg.join("a".into(), notify());
        reg.join("b".into(), notify());
        reg.join("c".into(), notify());
        // Active 'a' leaves → 'b' promoted, its notifier returned.
        assert!(reg.leave("a").is_some());
        assert_eq!(reg.count(), 2);
        assert_eq!(reg.ahead_of("c"), Some(1)); // 'c' moved up
        // 'b' is now active; its departure promotes 'c'.
        assert!(reg.leave("b").is_some());
        assert_eq!(reg.ahead_of("c"), None); // 'c' is active now
        // Last viewer leaves → nobody to promote.
        assert!(reg.leave("c").is_none());
        assert_eq!(reg.count(), 0);
    }

    #[test]
    fn registry_queued_viewer_leaving_does_not_promote() {
        let mut reg = ViewerRegistry::default();
        reg.join("a".into(), notify());
        reg.join("b".into(), notify());
        reg.join("c".into(), notify());
        // A waiting viewer bails — no promotion (active 'a' keeps streaming).
        assert!(reg.leave("b").is_none());
        assert_eq!(reg.count(), 2);
        assert_eq!(reg.ahead_of("c"), Some(1)); // 'c' moved up from 2 to 1
        // 'a' (still active) leaves → 'c' promoted.
        assert!(reg.leave("a").is_some());
        assert_eq!(reg.ahead_of("c"), None);
    }

    #[test]
    fn ice_candidate_init_deserializes_from_browser_json() {
        // Verify the browser candidate JSON maps onto RTCIceCandidateInit.
        let value: serde_json::Value = serde_json::from_str(
            r#"{"candidate":"candidate:1 1 udp 2122260223 192.168.1.5 51000 typ host","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":"abcd"}"#,
        )
        .unwrap();
        let init: RTCIceCandidateInit = serde_json::from_value(value).unwrap();
        assert!(init.candidate.contains("typ host"));
    }
}
