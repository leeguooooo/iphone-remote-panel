//! Which daemon this is.
//!
//! One daemon drives one phone. Everything that names *this* daemon on the
//! Mac — its state directory, the launchd labels it and its WDA supervisor
//! run under, the files it shares with `setup-wda.sh` — derives from one
//! instance name, so a second daemon for a second phone (#67) cannot collide
//! with the first. The default instance keeps every path and label the
//! single-phone install has always used.
//!
//! The derivation rules are pinned by `scripts/fixtures/instance-derivation.json`,
//! which the shell side checks against the same cases.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_NAME: &str = "default";
const DAEMON_LABEL: &str = "com.leeguoo.iphone-use";
/// The one name that would make a named instance's daemon label collide with
/// the default instance's WDA label.
const RESERVED_NAMES: &[&str] = &["wda"];
const NAME_MAX_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// `default`, or the validated name.
    pub name: String,
    pub home: PathBuf,
    /// Where status, logs, the helper script, and the WDA checkout live.
    pub state_dir: PathBuf,
    /// launchd label of the daemon itself.
    pub daemon_label: String,
    /// launchd label of the dedicated WDA supervisor.
    pub wda_label: String,
}

/// `[a-z][a-z0-9-]{0,31}`, not reserved.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && name.len() <= NAME_MAX_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !RESERVED_NAMES.contains(&name)
}

impl Instance {
    /// Derive an instance from its name. An empty name means the default.
    pub fn derive(
        name: &str,
        home: impl Into<PathBuf>,
        state_dir_override: Option<PathBuf>,
    ) -> Result<Instance, String> {
        let home = home.into();
        let name = if name.is_empty() { DEFAULT_NAME } else { name };
        if name != DEFAULT_NAME && !valid_name(name) {
            return Err(format!(
                "PHONE_REMOTE_INSTANCE={name:?} is not a valid instance name: lowercase [a-z][a-z0-9-], at most {NAME_MAX_LEN} chars, not one of {RESERVED_NAMES:?}"
            ));
        }
        let base = home.join(".iphone-use");
        let (state_dir, daemon_label, wda_label) = if name == DEFAULT_NAME {
            (base, DAEMON_LABEL.to_string(), format!("{DAEMON_LABEL}.wda"))
        } else {
            (
                base.join("instances").join(name),
                format!("{DAEMON_LABEL}.{name}"),
                format!("{DAEMON_LABEL}.wda.{name}"),
            )
        };
        Ok(Instance {
            name: name.to_string(),
            home,
            state_dir: state_dir_override.unwrap_or(state_dir),
            daemon_label,
            wda_label,
        })
    }

    /// From `PHONE_REMOTE_INSTANCE`, `HOME`, and an optional
    /// `PHONE_REMOTE_STATE_DIR` override.
    pub fn from_env() -> Result<Instance, String> {
        let name = std::env::var("PHONE_REMOTE_INSTANCE").unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_default();
        let state_dir = std::env::var("PHONE_REMOTE_STATE_DIR")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Self::derive(name.trim(), home, state_dir)
    }

    pub fn is_default(&self) -> bool {
        self.name == DEFAULT_NAME
    }
    pub fn setup_sh(&self) -> PathBuf {
        self.state_dir.join("setup-wda.sh")
    }
    pub fn agent_log(&self) -> PathBuf {
        self.state_dir.join("wda-agent.log")
    }
    pub fn runner_log(&self) -> PathBuf {
        self.state_dir.join("wda-runner.log")
    }
    pub fn status_file(&self) -> PathBuf {
        self.state_dir.join("wda-setup-status.json")
    }
    pub fn intents_registry(&self) -> PathBuf {
        self.state_dir.join("intents-registry.json")
    }
    /// The supervisor's LaunchAgent plist. LaunchAgents always live under the
    /// home directory, never under an overridden state dir.
    pub fn wda_plist(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", self.wda_label))
    }
    /// Path helper for callers that still work in strings.
    pub fn path_str(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }
}

static CURRENT: OnceLock<Instance> = OnceLock::new();

/// Pin the process-wide instance. Call once at startup, before anything reads
/// a path or label; a second call with a different instance is a bug.
pub fn install(instance: Instance) -> &'static Instance {
    let pinned = CURRENT.get_or_init(|| instance.clone());
    debug_assert_eq!(pinned, &instance, "the instance was pinned differently");
    pinned
}

/// The process-wide instance. Derives the default from the environment when
/// nothing was installed (tests, tools), so callers never see a missing value.
pub fn current() -> &'static Instance {
    CURRENT.get_or_init(|| {
        Instance::from_env().unwrap_or_else(|error| {
            tracing::warn!("{error}; using the default instance");
            Instance::derive("", std::env::var("HOME").unwrap_or_default(), None)
                .expect("the default instance always derives")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../../../scripts/fixtures/instance-derivation.json");

    #[test]
    fn derivation_matches_the_shared_golden_fixture() {
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).expect("fixture parses");
        let cases = golden["cases"].as_array().expect("cases");
        assert!(cases.len() >= 4);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let home = case["home"].as_str().unwrap();
            let over = case
                .get("state_dir_override")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            let instance = Instance::derive(name, home, over).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(instance.state_dir, PathBuf::from(case["state_dir"].as_str().unwrap()), "{name}: state_dir");
            assert_eq!(instance.daemon_label, case["daemon_label"].as_str().unwrap(), "{name}: daemon_label");
            assert_eq!(instance.wda_label, case["wda_label"].as_str().unwrap(), "{name}: wda_label");
            assert_eq!(instance.wda_plist(), PathBuf::from(case["wda_plist"].as_str().unwrap()), "{name}: wda_plist");
        }
        for bad in golden["invalid_names"].as_array().expect("invalid_names") {
            let bad = bad.as_str().unwrap();
            assert!(Instance::derive(bad, "/Users/x", None).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn the_default_instance_keeps_every_legacy_path_and_label() {
        let d = Instance::derive("", "/Users/leo", None).unwrap();
        assert!(d.is_default());
        assert_eq!(d.setup_sh(), PathBuf::from("/Users/leo/.iphone-use/setup-wda.sh"));
        assert_eq!(d.agent_log(), PathBuf::from("/Users/leo/.iphone-use/wda-agent.log"));
        assert_eq!(d.runner_log(), PathBuf::from("/Users/leo/.iphone-use/wda-runner.log"));
        assert_eq!(d.status_file(), PathBuf::from("/Users/leo/.iphone-use/wda-setup-status.json"));
        assert_eq!(d.intents_registry(), PathBuf::from("/Users/leo/.iphone-use/intents-registry.json"));
        assert_eq!(d.wda_label, "com.leeguoo.iphone-use.wda");
    }

    #[test]
    fn a_state_dir_override_never_moves_the_launch_agent() {
        let i = Instance::derive("lab", "/Users/leo", Some(PathBuf::from("/Volumes/fast/iu-lab"))).unwrap();
        assert_eq!(i.status_file(), PathBuf::from("/Volumes/fast/iu-lab/wda-setup-status.json"));
        assert_eq!(i.wda_plist(), PathBuf::from("/Users/leo/Library/LaunchAgents/com.leeguoo.iphone-use.wda.lab.plist"));
    }
}
