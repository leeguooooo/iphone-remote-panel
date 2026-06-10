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
}

/// Drive one viewer WebSocket session to completion.
///
/// Acquires the control lease for `session_id`, builds the PeerConnection, sends
/// the offer, and pumps signaling messages until the socket closes.
pub async fn run_session(mut socket: WebSocket, state: Arc<AppState>, session_id: String) {
    // Acquire the human control lease for this viewer.
    {
        let mut control = recover(state.control.lock());
        let lease = control.acquire(core::control::Holder::Human(session_id.clone()), now_secs());
        *recover(state.current_lease.lock()) = Some(lease);
    }

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

    // Tear down: release the lease and close the PC.
    {
        let mut control = recover(state.control.lock());
        if let Some(lease) = recover(state.current_lease.lock()).take() {
            control.release(&lease);
        }
    }
    let _ = pc.close().await;
    tracing::debug!("viewer session {session_id} ended");
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
        SignalMsg::Offer { .. } | SignalMsg::PhoneStatus { .. } => {
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
