//! Cloudflare TURN — mint short-lived ICE credentials from a TURN key.
//!
//! Cross-network clients (iPhone on cellular, away from the Mac's LAN) need a
//! TURN relay; STUN alone can't punch through symmetric NAT. Cloudflare's TURN
//! service issues **ephemeral** credentials: the daemon holds a long-lived TURN
//! *key* (an id + API token) and calls Cloudflare to mint a short-TTL
//! username/credential, which it serves to clients via `/turn-creds` and uses
//! for its own PeerConnection.
//!
//! The key id + token are read from the environment (set by `install.sh` into
//! the LaunchAgent, or exported manually) — never hard-coded:
//!   * `PHONE_REMOTE_CF_TURN_KEY_ID`
//!   * `PHONE_REMOTE_CF_TURN_API_TOKEN`
//!   * `PHONE_REMOTE_CF_TURN_TTL_SECS` (optional; default 86400, clamped 600..=172800)
//!
//! When these are absent the daemon falls back to STUN + any static
//! `PHONE_REMOTE_TURN_*` env, so this module is purely additive.

use anyhow::{Context, Result};
use serde::Deserialize;
use webrtc::ice_transport::ice_server::RTCIceServer;

/// Cloudflare Realtime TURN credential-generation API base.
const CF_TURN_KEYS_API: &str = "https://rtc.live.cloudflare.com/v1/turn/keys";

const DEFAULT_TTL_SECS: u64 = 86_400; // 24h
const MIN_TTL_SECS: u64 = 600; // 10 min
const MAX_TTL_SECS: u64 = 172_800; // 48h (Cloudflare's ceiling)

/// Cloudflare TURN configuration read from the environment.
#[derive(Clone)]
pub struct CfTurnConfig {
    key_id: String,
    api_token: String,
    /// Requested credential lifetime, in seconds.
    pub ttl_secs: u64,
}

impl CfTurnConfig {
    /// Read config from the process environment, or `None` if not configured.
    ///
    /// Requires both `PHONE_REMOTE_CF_TURN_KEY_ID` and
    /// `PHONE_REMOTE_CF_TURN_API_TOKEN` to be non-empty.
    pub fn from_env() -> Option<Self> {
        Self::from_getter(|k| std::env::var(k).ok())
    }

    /// Testable core of [`from_env`] — `get` resolves an env var by name.
    pub fn from_getter(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let nonempty = |k: &str| {
            get(k)
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let key_id = nonempty("PHONE_REMOTE_CF_TURN_KEY_ID")?;
        let api_token = nonempty("PHONE_REMOTE_CF_TURN_API_TOKEN")?;
        let ttl_secs = nonempty("PHONE_REMOTE_CF_TURN_TTL_SECS")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TTL_SECS)
            .clamp(MIN_TTL_SECS, MAX_TTL_SECS);
        Some(Self {
            key_id,
            api_token,
            ttl_secs,
        })
    }

    /// Seconds to wait before re-minting: half the TTL, so a credential never
    /// reaches expiry while in use (floored so a tiny TTL still refreshes).
    pub fn refresh_after_secs(&self) -> u64 {
        (self.ttl_secs / 2).max(MIN_TTL_SECS / 2)
    }
}

// Cloudflare's response shape: { "iceServers": { "urls": [...], "username", "credential" } }
#[derive(Deserialize)]
struct CfResponse {
    #[serde(rename = "iceServers")]
    ice_servers: CfIceServers,
}

#[derive(Deserialize)]
struct CfIceServers {
    urls: Vec<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    credential: String,
}

/// Parse a Cloudflare credential-generate response body into an [`RTCIceServer`].
///
/// Pulled out so it is unit-testable without a live HTTP call.
fn parse_response(body: &str) -> Result<RTCIceServer> {
    let parsed: CfResponse = serde_json::from_str(body)
        .with_context(|| format!("parse Cloudflare TURN response: {body}"))?;
    if parsed.ice_servers.urls.is_empty() {
        anyhow::bail!("Cloudflare TURN response had no urls: {body}");
    }
    Ok(RTCIceServer {
        urls: parsed.ice_servers.urls,
        username: parsed.ice_servers.username,
        credential: parsed.ice_servers.credential,
    })
}

/// Mint one ephemeral TURN [`RTCIceServer`] from Cloudflare.
///
/// Cloudflare returns a single object carrying several relay URLs plus one
/// username/credential pair valid for `ttl_secs`.
pub async fn mint(cfg: &CfTurnConfig) -> Result<RTCIceServer> {
    let url = format!("{CF_TURN_KEYS_API}/{}/credentials/generate", cfg.key_id);
    let client = reqwest::Client::builder()
        .build()
        .context("build reqwest client")?;
    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_token)
        .json(&serde_json::json!({ "ttl": cfg.ttl_secs }))
        .send()
        .await
        .context("Cloudflare TURN request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Cloudflare TURN returned {status}: {body}");
    }
    parse_response(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter<'a>(map: &'a HashMap<&'a str, &'a str>) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| map.get(k).map(|s| s.to_string())
    }

    #[test]
    fn from_env_requires_both_id_and_token() {
        let only_id = HashMap::from([("PHONE_REMOTE_CF_TURN_KEY_ID", "kid")]);
        assert!(CfTurnConfig::from_getter(getter(&only_id)).is_none());

        let only_tok = HashMap::from([("PHONE_REMOTE_CF_TURN_API_TOKEN", "tok")]);
        assert!(CfTurnConfig::from_getter(getter(&only_tok)).is_none());

        let both = HashMap::from([
            ("PHONE_REMOTE_CF_TURN_KEY_ID", "kid"),
            ("PHONE_REMOTE_CF_TURN_API_TOKEN", "tok"),
        ]);
        let cfg = CfTurnConfig::from_getter(getter(&both)).expect("both present");
        assert_eq!(cfg.ttl_secs, DEFAULT_TTL_SECS);
    }

    #[test]
    fn blank_values_are_treated_as_absent() {
        let blank = HashMap::from([
            ("PHONE_REMOTE_CF_TURN_KEY_ID", "   "),
            ("PHONE_REMOTE_CF_TURN_API_TOKEN", "tok"),
        ]);
        assert!(CfTurnConfig::from_getter(getter(&blank)).is_none());
    }

    #[test]
    fn ttl_is_clamped() {
        let too_big = HashMap::from([
            ("PHONE_REMOTE_CF_TURN_KEY_ID", "kid"),
            ("PHONE_REMOTE_CF_TURN_API_TOKEN", "tok"),
            ("PHONE_REMOTE_CF_TURN_TTL_SECS", "999999999"),
        ]);
        assert_eq!(
            CfTurnConfig::from_getter(getter(&too_big))
                .unwrap()
                .ttl_secs,
            MAX_TTL_SECS
        );

        let too_small = HashMap::from([
            ("PHONE_REMOTE_CF_TURN_KEY_ID", "kid"),
            ("PHONE_REMOTE_CF_TURN_API_TOKEN", "tok"),
            ("PHONE_REMOTE_CF_TURN_TTL_SECS", "1"),
        ]);
        assert_eq!(
            CfTurnConfig::from_getter(getter(&too_small))
                .unwrap()
                .ttl_secs,
            MIN_TTL_SECS
        );
    }

    #[test]
    fn refresh_is_half_ttl() {
        let cfg = CfTurnConfig {
            key_id: "k".into(),
            api_token: "t".into(),
            ttl_secs: 86_400,
        };
        assert_eq!(cfg.refresh_after_secs(), 43_200);
    }

    #[test]
    fn parse_response_extracts_urls_user_credential() {
        let body = r#"{"iceServers":{"urls":["turn:turn.cloudflare.com:3478?transport=udp","turns:turn.cloudflare.com:5349?transport=tcp"],"username":"abc","credential":"xyz"}}"#;
        let s = parse_response(body).unwrap();
        assert_eq!(s.urls.len(), 2);
        assert_eq!(s.username, "abc");
        assert_eq!(s.credential, "xyz");
    }

    #[test]
    fn parse_response_rejects_empty_urls() {
        let body = r#"{"iceServers":{"urls":[],"username":"a","credential":"b"}}"#;
        assert!(parse_response(body).is_err());
    }
}
