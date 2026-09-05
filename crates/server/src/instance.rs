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
        let state_dir = match state_dir_override {
            Some(dir) => {
                check_state_dir_override(&dir, &home)?;
                dir
            }
            None => state_dir,
        };
        Ok(Instance {
            name: name.to_string(),
            home,
            state_dir,
            daemon_label,
            wda_label,
        })
    }

    /// Refuse a state directory the daemon must not own: one that exists but
    /// is not a plain directory, is reached through a symlink, or belongs to
    /// someone other than the owner of HOME. A missing directory is fine —
    /// the helper creates it.
    pub fn verify_on_disk(&self) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt as _;
        let dir = &self.state_dir;
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return Ok(());
        };
        if meta.file_type().is_symlink() {
            return Err(format!("state dir {} is a symlink; refusing to use it", dir.display()));
        }
        if !meta.is_dir() {
            return Err(format!("state dir {} is not a directory", dir.display()));
        }
        let canonical = dir
            .canonicalize()
            .map_err(|e| format!("state dir {}: {e}", dir.display()))?;
        let home_canonical = self.home.canonicalize().unwrap_or_else(|_| self.home.clone());
        let expected = if dir.starts_with(&self.home) {
            home_canonical.join(dir.strip_prefix(&self.home).unwrap_or(dir))
        } else {
            dir.clone()
        };
        if canonical != expected {
            return Err(format!(
                "state dir {} resolves through a symlink to {}; refusing to use it",
                dir.display(),
                canonical.display()
            ));
        }
        if let Ok(home_meta) = std::fs::metadata(&self.home) {
            if home_meta.uid() != meta.uid() {
                return Err(format!(
                    "state dir {} is owned by uid {} but HOME by uid {}; refusing to use it",
                    dir.display(),
                    meta.uid(),
                    home_meta.uid()
                ));
            }
        }
        Ok(())
    }

    /// From `PHONE_REMOTE_INSTANCE`, `HOME`, and an optional
    /// `PHONE_REMOTE_STATE_DIR` override. Values are taken verbatim: a name
    /// with stray whitespace is invalid, not trimmed into a different
    /// instance.
    pub fn from_env() -> Result<Instance, String> {
        let name = std::env::var("PHONE_REMOTE_INSTANCE").unwrap_or_default();
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return Err("HOME is not set; cannot derive the instance state dir".into());
        }
        let state_dir = std::env::var("PHONE_REMOTE_STATE_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Self::derive(&name, home, state_dir)
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

/// Lexical rules for a `PHONE_REMOTE_STATE_DIR` override. The on-disk rules
/// (symlink, ownership) are [`Instance::verify_on_disk`].
///
/// The override must be absolute and must stay clear of the `~/.iphone-use`
/// namespace entirely — not equal to it, not inside it, not an ancestor of
/// it — so it can neither alias the default instance nor overlap another
/// named instance's derived directory. Nor may it be `/` or HOME.
fn check_state_dir_override(dir: &Path, home: &Path) -> Result<(), String> {
    let reject = |why: &str| Err(format!("PHONE_REMOTE_STATE_DIR={}: {why}", dir.display()));
    if !dir.is_absolute() {
        return reject("must be an absolute path");
    }
    if dir.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)) {
        return reject("must not contain `.` or `..` components");
    }
    if dir == Path::new("/") {
        return reject("must not be the filesystem root");
    }
    if dir == home {
        return reject("must not be HOME itself");
    }
    let namespace = home.join(".iphone-use");
    if dir == namespace || dir.starts_with(&namespace) || namespace.starts_with(dir) {
        return reject("must not be, contain, or live inside ~/.iphone-use");
    }
    Ok(())
}

static CURRENT: OnceLock<Instance> = OnceLock::new();

/// Pin the process-wide instance. Call once at startup, before anything reads
/// a path or label; a second call with a different instance is a bug.
pub fn install(instance: Instance) -> &'static Instance {
    let pinned = CURRENT.get_or_init(|| instance.clone());
    debug_assert_eq!(pinned, &instance, "the instance was pinned differently");
    pinned
}

/// The process-wide instance. Derives it from the environment when nothing
/// was installed (tests, tools). Fails closed: an invalid instance name or
/// state dir aborts rather than quietly falling back to the default — a
/// daemon with a mistyped name must never drive the default phone.
pub fn current() -> &'static Instance {
    CURRENT.get_or_init(|| match Instance::from_env() {
        Ok(instance) => instance,
        Err(error) => panic!("{error}"),
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
    fn a_state_dir_override_must_stay_out_of_the_namespace() {
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        for bad in golden["invalid_state_dir_overrides"].as_array().unwrap() {
            let bad = bad.as_str().unwrap();
            assert!(
                Instance::derive("lab", "/Users/leo", Some(PathBuf::from(bad))).is_err(),
                "override {bad:?} must be rejected"
            );
        }
        assert!(Instance::derive("lab", "/Users/leo", Some(PathBuf::from("/Volumes/fast/iu-lab"))).is_ok());
    }

    #[test]
    fn whitespace_around_a_name_is_not_trimmed_into_another_instance() {
        assert!(Instance::derive(" lab", "/Users/leo", None).is_err());
        assert!(Instance::derive("lab ", "/Users/leo", None).is_err());
        assert!(Instance::derive("default ", "/Users/leo", None).is_err());
    }

    #[test]
    fn on_disk_verification_refuses_a_symlinked_state_dir() {
        let root = std::env::temp_dir().join(format!("iu-inst-{}", std::process::id()));
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut ok = Instance::derive("", &root, None).unwrap();
        ok.state_dir = real.clone();
        // A plain directory under a home that is itself reached via /tmp's
        // symlink is compared through the same prefix, so it passes.
        assert!(ok.verify_on_disk().is_ok(), "{:?}", ok.verify_on_disk());
        let mut bad = ok.clone();
        bad.state_dir = link;
        assert!(bad.verify_on_disk().is_err());
        let mut missing = ok.clone();
        missing.state_dir = root.join("not-yet");
        assert!(missing.verify_on_disk().is_ok(), "a missing dir is created later");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_state_dir_override_never_moves_the_launch_agent() {
        let i = Instance::derive("lab", "/Users/leo", Some(PathBuf::from("/Volumes/fast/iu-lab"))).unwrap();
        assert_eq!(i.status_file(), PathBuf::from("/Volumes/fast/iu-lab/wda-setup-status.json"));
        assert_eq!(i.wda_plist(), PathBuf::from("/Users/leo/Library/LaunchAgents/com.leeguoo.iphone-use.wda.lab.plist"));
    }
}
