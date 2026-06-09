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
    /// IP address / hostname to listen on. Default: `127.0.0.1`.
    pub host: String,
    /// TCP port to listen on. Default: `8787`.
    pub port: u16,
    /// Optional shared password clients must supply.
    pub password: Option<String>,
    /// Optional shared secret for token signing.
    pub secret: Option<String>,
    /// Session TTL in seconds. Default: `28800` (8 h).
    pub session_ttl_secs: u64,
    /// Optional directory for persistent state files.
    pub state_dir: Option<PathBuf>,
    /// Path to the `cua-driver` binary.
    /// Default: `/Users/leo/.local/bin/cua-driver`.
    pub cua_driver: PathBuf,
}

// ---------------------------------------------------------------------------
// Defaults matching v1 phone_remote_server.py
// ---------------------------------------------------------------------------

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8787;
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
        let host = get("PHONE_REMOTE_HOST")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_HOST.to_owned());

        let port = get("PHONE_REMOTE_PORT")
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(DEFAULT_PORT);

        let password = get("PHONE_REMOTE_PASSWORD").filter(|s| !s.is_empty());
        let secret = get("PHONE_REMOTE_SECRET").filter(|s| !s.is_empty());

        let session_ttl_secs = get("PHONE_REMOTE_SESSION_TTL")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SESSION_TTL_SECS);

        let state_dir = get("PHONE_REMOTE_STATE_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);

        // cua-driver path: explicit CUA_DRIVER, else $HOME/.local/bin/cua-driver
        // (a LaunchAgent runs with a minimal PATH, so an absolute path is needed),
        // else a bare `cua-driver` resolved via PATH.
        let cua_driver = get("CUA_DRIVER")
            .filter(|s| !s.is_empty())
            .or_else(|| {
                get("HOME")
                    .filter(|s| !s.is_empty())
                    .map(|home| format!("{home}/.local/bin/cua-driver"))
            })
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cua-driver"));

        Config {
            host,
            port,
            password,
            secret,
            session_ttl_secs,
            state_dir,
            cua_driver,
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
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8787);
        assert_eq!(cfg.password, None);
        assert_eq!(cfg.secret, None);
        assert_eq!(cfg.session_ttl_secs, 8 * 3600);
        assert_eq!(cfg.state_dir, None);
        // No CUA_DRIVER and no HOME → bare `cua-driver` (PATH lookup).
        assert_eq!(cfg.cua_driver, PathBuf::from("cua-driver"));
    }

    #[test]
    fn cua_driver_defaults_under_home() {
        let cfg = map_cfg(&[("HOME", "/Users/someone")]);
        assert_eq!(
            cfg.cua_driver,
            PathBuf::from("/Users/someone/.local/bin/cua-driver")
        );
    }

    #[test]
    fn cua_driver_env_beats_home_default() {
        let cfg = map_cfg(&[
            ("HOME", "/Users/someone"),
            ("CUA_DRIVER", "/opt/cua-driver"),
        ]);
        assert_eq!(cfg.cua_driver, PathBuf::from("/opt/cua-driver"));
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
    }

    #[test]
    fn state_dir_override() {
        let cfg = map_cfg(&[("PHONE_REMOTE_STATE_DIR", "/var/lib/hermes")]);
        assert_eq!(cfg.state_dir, Some(PathBuf::from("/var/lib/hermes")));
    }

    #[test]
    fn cua_driver_override() {
        let cfg = map_cfg(&[("CUA_DRIVER", "/usr/local/bin/cua-driver")]);
        assert_eq!(cfg.cua_driver, PathBuf::from("/usr/local/bin/cua-driver"));
    }

    // --- bad values fall back to defaults, don't panic ---

    #[test]
    fn bad_port_falls_back_to_default() {
        let cfg = map_cfg(&[("PHONE_REMOTE_PORT", "not-a-number")]);
        assert_eq!(cfg.port, 8787);
    }

    #[test]
    fn port_out_of_u16_range_falls_back_to_default() {
        let cfg = map_cfg(&[("PHONE_REMOTE_PORT", "99999")]);
        assert_eq!(cfg.port, 8787);
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
}
