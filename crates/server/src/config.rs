//! Environment-variable configuration surface, mirroring v1's
//! `phone_remote_server.py` defaults exactly so that nothing regresses.
//!
//! Call [`Config::from_env`] in production code, or [`Config::from_map`]
//! (passing a closure) in tests to avoid touching the real process
//! environment.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Resolved configuration for the phone-remote server.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Device transport used by the daemon.
    ///
    /// `Direct` is the default: video and input come from services running on the
    /// iPhone, so the daemon does not need iPhone Mirroring, Screen Recording, or
    /// Accessibility access on the Mac. `Mirror` keeps the original
    /// ScreenCaptureKit + CGEvent path as an explicit compatibility mode.
    pub backend: DeviceBackend,
    /// IP address / hostname to listen on. Default: `127.0.0.1`.
    pub host: String,
    /// TCP port to listen on. Default: `44321`.
    pub port: u16,
    /// Optional shared password clients must supply.
    pub password: Option<String>,
    /// Optional shared secret for token signing.
    pub secret: Option<String>,
    /// Session TTL in seconds. Default: `28800` (8 h).
    pub session_ttl_secs: u64,
    /// Optional directory for persistent state files.
    pub state_dir: Option<PathBuf>,
    /// Optional dedicated bearer token for agent/API access (`PHONE_REMOTE_AGENT_TOKEN`).
    ///
    /// When set, the agent `Authorization: Bearer <token>` path checks this value
    /// **instead of** the human login password — the password is no longer accepted
    /// as a bearer credential (clean separation of human and machine secrets).
    ///
    /// When absent (the default), the existing behavior is preserved: the password
    /// is also accepted as a bearer, and open mode (no password) passes everything.
    pub agent_token: Option<String>,
    /// Persistent target device identifier for WDA bring-up and recovery.
    ///
    /// Without this, an idle restart or browser reconnect re-ran auto-detection
    /// and could silently select a different iPhone on a multi-device Mac.
    pub device_udid: Option<String>,
}

/// How the daemon reaches the iPhone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceBackend {
    /// WebDriverAgent input + on-device video. No iPhone Mirroring dependency.
    Direct,
    /// Legacy iPhone Mirroring capture + Mac-side CGEvent input.
    Mirror,
}

impl DeviceBackend {
    fn parse(value: Option<String>) -> Self {
        match value.as_deref().map(str::trim) {
            Some("mirror") | Some("legacy-mirror") => Self::Mirror,
            // Direct is deliberately the safe default, including for unknown
            // values: an upgrade must never silently regain Mac screen/cursor
            // access because of a typo.
            _ => Self::Direct,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Mirror => "mirror",
        }
    }
}

// ---------------------------------------------------------------------------
// Defaults matching v1 phone_remote_server.py
// ---------------------------------------------------------------------------

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 44321;
const DEFAULT_SESSION_TTL_SECS: u64 = 8 * 3600; // 28 800

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl Config {
    /// Build a [`Config`] from the real process environment.
    pub fn from_env() -> Config {
        Config::from_map(|key| std::env::var(key).ok())
    }

    /// Build a [`Config`] from an arbitrary key→value lookup function.
    ///
    /// This is the testable core: `from_env` simply delegates here with
    /// `std::env::var`.  In tests, pass a closure over a `HashMap<&str,
    /// &str>` (or similar) to avoid side-effects on the real environment.
    pub fn from_map(get: impl Fn(&str) -> Option<String>) -> Config {
        let backend = DeviceBackend::parse(get("PHONE_REMOTE_BACKEND"));
        let host = get("PHONE_REMOTE_HOST")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_owned());

        let port = get("PHONE_REMOTE_PORT")
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);

        let password = get("PHONE_REMOTE_PASSWORD").filter(|s| !s.is_empty());
        let secret = get("PHONE_REMOTE_SECRET").filter(|s| !s.is_empty());

        // A zero TTL signs tokens that are already expired when issued, which
        // locks the operator out of their own web UI with no error to read —
        // the login just never sticks. Treat it as the typo it always is and
        // fall back, the same way the activation deadline does (#29/#30).
        // Unlike PHONE_REMOTE_IDLE_RELEASE_SECS, zero has no "disabled"
        // meaning here: there is no such thing as a session that never ends.
        let session_ttl_secs = get("PHONE_REMOTE_SESSION_TTL")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_SESSION_TTL_SECS);

        let state_dir = get("PHONE_REMOTE_STATE_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        let agent_token = get("PHONE_REMOTE_AGENT_TOKEN").filter(|s| !s.is_empty());
        let device_udid = get("PHONE_REMOTE_UDID")
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));

        Config {
            backend,
            host,
            port,
            password,
            secret,
            session_ttl_secs,
            state_dir,
            agent_token,
            device_udid,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn map_cfg(pairs: &[(&str, &str)]) -> Config {
        let m: HashMap<&str, &str> = pairs.iter().cloned().collect();
        Config::from_map(|k| m.get(k).map(|v| v.to_string()))
    }

    // --- defaults ---

    #[test]
    fn defaults_when_empty_env() {
        let cfg = map_cfg(&[]);
        assert_eq!(cfg.backend, DeviceBackend::Direct);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 44321);
        assert_eq!(cfg.password, None);
        assert_eq!(cfg.secret, None);
        assert_eq!(cfg.session_ttl_secs, 8 * 3600);
        assert_eq!(cfg.state_dir, None);
    }

    // --- overrides ---

    #[test]
    fn host_override() {
        let cfg = map_cfg(&[("PHONE_REMOTE_HOST", "0.0.0.0")]);
        assert_eq!(cfg.host, "0.0.0.0");
    }

    #[test]
    fn port_override_valid() {
        let cfg = map_cfg(&[("PHONE_REMOTE_PORT", "9000")]);
        assert_eq!(cfg.port, 9000);
    }

    #[test]
    fn password_and_secret_override() {
        let cfg = map_cfg(&[
            ("PHONE_REMOTE_PASSWORD", "hunter2"),
            ("PHONE_REMOTE_SECRET", "s3cr3t"),
        ]);
        assert_eq!(cfg.password, Some("hunter2".to_owned()));
        assert_eq!(cfg.secret, Some("s3cr3t".to_owned()));
    }

    #[test]
    fn session_ttl_override() {
        let cfg = map_cfg(&[("PHONE_REMOTE_SESSION_TTL", "3600")]);
        assert_eq!(cfg.session_ttl_secs, 3600);
        assert_eq!(
            map_cfg(&[("PHONE_REMOTE_SESSION_TTL", " 3600 ")]).session_ttl_secs,
            3600
        );
    }

    #[test]
    fn session_ttl_never_signs_an_already_expired_token() {
        // A 0 TTL issues tokens that expire the instant they are minted, so
        // login silently never sticks and the operator is locked out of their
        // own UI with nothing to read. Junk deserves the same fallback.
        for raw in ["0", "", "abc", "-1", "3600s", "1.5"] {
            assert_eq!(
                map_cfg(&[("PHONE_REMOTE_SESSION_TTL", raw)]).session_ttl_secs,
                DEFAULT_SESSION_TTL_SECS,
                "PHONE_REMOTE_SESSION_TTL={raw:?} must fall back, not lock the user out"
            );
        }
    }

    #[test]
    fn state_dir_override() {
        let cfg = map_cfg(&[("PHONE_REMOTE_STATE_DIR", "/var/lib/hermes")]);
        assert_eq!(cfg.state_dir, Some(PathBuf::from("/var/lib/hermes")));
    }

    // --- bad values fall back to defaults, don't panic ---

    #[test]
    fn bad_port_falls_back_to_default() {
        let cfg = map_cfg(&[("PHONE_REMOTE_PORT", "not-a-number")]);
        assert_eq!(cfg.port, 44321);
    }

    #[test]
    fn port_out_of_u16_range_falls_back_to_default() {
        let cfg = map_cfg(&[("PHONE_REMOTE_PORT", "99999")]);
        assert_eq!(cfg.port, 44321);
    }

    #[test]
    fn bad_ttl_falls_back_to_default() {
        let cfg = map_cfg(&[("PHONE_REMOTE_SESSION_TTL", "abc")]);
        assert_eq!(cfg.session_ttl_secs, 8 * 3600);
    }

    #[test]
    fn negative_ttl_string_falls_back_to_default() {
        // "-1" can't parse as u64, so default is used.
        let cfg = map_cfg(&[("PHONE_REMOTE_SESSION_TTL", "-1")]);
        assert_eq!(cfg.session_ttl_secs, 8 * 3600);
    }

    // --- empty strings treated as unset ---

    #[test]
    fn empty_host_uses_default() {
        let cfg = map_cfg(&[("PHONE_REMOTE_HOST", "")]);
        assert_eq!(cfg.host, "127.0.0.1");
    }

    #[test]
    fn empty_password_gives_none() {
        let cfg = map_cfg(&[("PHONE_REMOTE_PASSWORD", "")]);
        assert_eq!(cfg.password, None);
    }

    // --- agent_token ---

    #[test]
    fn agent_token_unset_gives_none() {
        let cfg = map_cfg(&[]);
        assert_eq!(cfg.agent_token, None);
        assert_eq!(cfg.device_udid, None);
    }

    #[test]
    fn backend_defaults_to_direct_and_mirror_is_explicit() {
        assert_eq!(map_cfg(&[]).backend, DeviceBackend::Direct);
        assert_eq!(
            map_cfg(&[("PHONE_REMOTE_BACKEND", "direct")]).backend,
            DeviceBackend::Direct
        );
        assert_eq!(
            map_cfg(&[("PHONE_REMOTE_BACKEND", "mirror")]).backend,
            DeviceBackend::Mirror
        );
        assert_eq!(
            map_cfg(&[("PHONE_REMOTE_BACKEND", "legacy-mirror")]).backend,
            DeviceBackend::Mirror
        );
    }

    #[test]
    fn unknown_backend_fails_closed_to_direct() {
        let cfg = map_cfg(&[("PHONE_REMOTE_BACKEND", "miror")]);
        assert_eq!(cfg.backend, DeviceBackend::Direct);
    }

    #[test]
    fn agent_token_empty_gives_none() {
        let cfg = map_cfg(&[("PHONE_REMOTE_AGENT_TOKEN", "")]);
        assert_eq!(cfg.agent_token, None);
    }

    #[test]
    fn device_udid_is_preserved_and_empty_is_ignored() {
        assert_eq!(
            map_cfg(&[("PHONE_REMOTE_UDID", "00008110-001234567890001E")]).device_udid,
            Some("00008110-001234567890001E".to_string())
        );
        assert_eq!(map_cfg(&[("PHONE_REMOTE_UDID", "   ")]).device_udid, None);
        assert_eq!(
            map_cfg(&[("PHONE_REMOTE_UDID", "device</string>")]).device_udid,
            None
        );
    }

    #[test]
    fn agent_token_set_is_preserved() {
        let cfg = map_cfg(&[("PHONE_REMOTE_AGENT_TOKEN", "sk-agent-abc123")]);
        assert_eq!(cfg.agent_token, Some("sk-agent-abc123".to_owned()));
    }

    #[test]
    fn agent_token_can_coexist_with_password() {
        let cfg = map_cfg(&[
            ("PHONE_REMOTE_PASSWORD", "hunter2"),
            ("PHONE_REMOTE_AGENT_TOKEN", "sk-agent-xyz"),
        ]);
        assert_eq!(cfg.password, Some("hunter2".to_owned()));
        assert_eq!(cfg.agent_token, Some("sk-agent-xyz".to_owned()));
    }
}
