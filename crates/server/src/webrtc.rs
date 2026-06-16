//! Per-viewer WebRTC PeerConnection: H.264 video track fed from the pipeline,
//! PLI→keyframe, and `control`/`move` data channels routed to input injection.
//!
//! The daemon is the **offerer** (see [`crate::signaling`]). This module builds
//! the PeerConnection, adds the video track + two data channels, wires the feed
//! and PLI loops, and returns the connection so the signaling layer can drive the
//! offer/answer/ICE handshake.
//!
//! Reuses the webrtc-rs patterns proven in `crates/spikes/src/bin/s0b2_webrtc.rs`
//! (TrackLocalStaticSample H264 feed, RTCP PLI reader → force IDR) but inverts the
//! negotiation direction (daemon offers; the browser answers, recvonly).

use std::sync::Arc;

use anyhow::Result;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use core::encode::VideoPipeline;

use crate::input_bridge::{decode_control, decode_move, InputInjector};

/// The H.264 fmtp the web client requires (Safari constrained-baseline marker
/// `42e01f`, packetization-mode 1). MUST match `web/index.html`'s answer codec.
pub const H264_FMTP: &str =
    "profile-level-id=42e01f;packetization-mode=1;level-asymmetry-allowed=1";

/// Build a fresh PeerConnection wired for one viewer.
///
/// * Adds an H.264 [`TrackLocalStaticSample`] and spawns the pipeline→track feed
///   task (one keepalive write per frame) plus the RTCP PLI reader (→ force IDR).
/// * Creates the `control` (ordered) and `move` (unordered, maxRetransmits=0)
///   data channels and routes their messages through `injector`.
/// * Forces a keyframe on connect (`on_peer_connection_state_change` → Connected).
///
/// Returns the `Arc<RTCPeerConnection>`; the caller ([`crate::signaling`]) does
/// `create_offer` / `set_local_description` and the ICE/answer exchange.
pub async fn build_viewer_pc(
    ice_servers: Vec<RTCIceServer>,
    pipeline: Arc<dyn VideoPipeline>,
    injector: InputInjector,
    wda: Option<Arc<tokio::sync::Mutex<crate::wda::WdaClient>>>,
) -> Result<Arc<RTCPeerConnection>> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs()?;

    let registry = Registry::new();
    let registry = register_default_interceptors(registry, &mut media_engine)?;

    let mut setting_engine = webrtc::api::setting_engine::SettingEngine::default();

    // Disable mDNS candidate obfuscation. webrtc-rs defaults to advertising LAN host
    // candidates as random `<uuid>.local` mDNS names instead of raw IPs. That needs
    // multicast to resolve — which silently dies behind a VPN (Cloudflare WARP, etc.)
    // with "No route to host", so the phone never learns the Mac's real LAN IP and the
    // only surviving candidate is a useless VPN-egress srflx → ICE fails the instant the
    // page opens. This is a self-hosted LAN-first tool: advertise the real IP directly.
    setting_engine
        .set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);

    // Only gather candidates on physical interfaces. VPN tunnels (utun*/ipsec*/ppp*) add
    // dead POINTOPOINT candidates (a WARP host has 10+ utun NICs) that bloat the SDP, slow
    // gathering, and never pair with a LAN phone. Keep en*/bridge* (Wi-Fi/Ethernet) only.
    setting_engine.set_interface_filter(Box::new(|name: &str| {
        !(name.starts_with("utun") || name.starts_with("ipsec") || name.starts_with("ppp"))
    }));

    // Drop IPv6 link-local (`fe80::/10`) candidates: webrtc-rs fails to bind them
    // ("Can't assign requested address", os error 49), which spams the log and can
    // stall candidate pairing. LAN (IPv4), STUN and TURN don't need them.
    setting_engine.set_ip_filter(Box::new(|ip: std::net::IpAddr| match ip {
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
        std::net::IpAddr::V4(_) => true,
    }));

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();

    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let pc = Arc::new(api.new_peer_connection(config).await?);

    // ── H.264 video track ──────────────────────────────────────────────────
    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            sdp_fmtp_line: H264_FMTP.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "iphone-mirror".to_owned(),
    ));

    let rtp_sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // PLI reader: force a fresh IDR on PictureLossIndication; drain other RTCP.
    {
        let pipeline = pipeline.clone();
        tokio::spawn(async move {
            while let Ok((packets, _)) = rtp_sender.read_rtcp().await {
                for p in &packets {
                    if p.as_any().downcast_ref::<PictureLossIndication>().is_some() {
                        pipeline.request_keyframe();
                    }
                }
            }
        });
    }

    // Feed: subscribe to the encoded broadcast and write each access unit as a
    // Sample. On Lagged, request a keyframe so the viewer can re-sync.
    {
        let pipeline = pipeline.clone();
        let track = Arc::clone(&track);
        tokio::spawn(async move {
            feed_loop(pipeline, track).await;
        });
    }

    // Force a keyframe as soon as the connection reaches Connected so the viewer
    // gets a decodable entry point without waiting for the next GOP.
    {
        let pipeline = pipeline.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            tracing::debug!("peer connection state: {state}");
            if state == webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Connected {
                pipeline.request_keyframe();
            }
            Box::pin(async {})
        }));
    }

    // ── Data channels: control (ordered) + move (unordered, lossy) ─────────
    let control_ch = pc
        .create_data_channel(
            "control",
            Some(RTCDataChannelInit {
                ordered: Some(true),
                ..Default::default()
            }),
        )
        .await?;
    wire_control_channel(control_ch, wda, injector.clone());

    let move_ch = pc
        .create_data_channel(
            "move",
            Some(RTCDataChannelInit {
                ordered: Some(false),
                max_retransmits: Some(0),
                ..Default::default()
            }),
        )
        .await?;
    wire_move_channel(move_ch, injector);

    // Request a keyframe right away so the very first frames after handshake are
    // decodable (covers viewer-join per the encode contract).
    pipeline.request_keyframe();

    Ok(pc)
}

/// Pipeline→track feed loop. Subscribes to the encoded H.264 broadcast and writes
/// every access unit to the track. On `Lagged`, requests a keyframe and keeps
/// going; on `Closed`, exits.
async fn feed_loop(pipeline: Arc<dyn VideoPipeline>, track: Arc<TrackLocalStaticSample>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = pipeline.subscribe();
    // Per-frame duration at 30 fps; the encode layer's keepalive guarantees a
    // steady cadence even on a static screen.
    let frame_dur = std::time::Duration::from_millis(1000 / 30);
    loop {
        match rx.recv().await {
            Ok(frame) => {
                let sample = Sample {
                    data: frame.data,
                    duration: frame_dur,
                    ..Default::default()
                };
                if let Err(e) = track.write_sample(&sample).await {
                    tracing::debug!("feed: write_sample ended: {e}");
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => {
                tracing::debug!("feed: lagged {n} frames, requesting keyframe");
                pipeline.request_keyframe();
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// Route control-channel JSON messages. In agent mode (WDA up) the browser
/// drives the phone ON-DEVICE via WDA — correct device, no Mac focus steal.
/// Falls back to the L3 (mirror) injector when WDA is absent or can't handle the
/// event (the injector drives whatever the Mac mirrors and yanks it frontmost).
fn wire_control_channel(
    ch: Arc<RTCDataChannel>,
    wda: Option<Arc<tokio::sync::Mutex<crate::wda::WdaClient>>>,
    injector: InputInjector,
) {
    ch.on_message(Box::new(move |msg| {
        let wda = wda.clone();
        let injector = injector.clone();
        Box::pin(async move {
            // Control channel is JSON text.
            if let Ok(text) = std::str::from_utf8(&msg.data) {
                if let Some(w) = &wda {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                        if crate::http::wda_control_from_json(w, &v).await {
                            return;
                        }
                    }
                }
                if let Some(ev) = decode_control(text) {
                    injector.send(ev);
                }
            }
        })
    }));
}

/// Route move-channel binary packets (5-byte) to the injector.
fn wire_move_channel(ch: Arc<RTCDataChannel>, injector: InputInjector) {
    ch.on_message(Box::new(move |msg| {
        let injector = injector.clone();
        Box::pin(async move {
            if let Some(ev) = decode_move(&msg.data) {
                injector.send(ev);
            }
        })
    }));
}
