//! App-version compatibility for registry flows.
//!
//! A flow records the app (or, for Apple system apps, the iOS) version it was
//! last proved on in `verified_on[].app_version`, and optionally a floor in
//! `app_version_min`. Given what is installed on the phone, every flow gets a
//! `Compat` verdict that `flow list`, `phone_flow_list`, the `registry` block
//! in `phone_elements`, and `flow run` all act on.
//!
//! Installed versions come from `GET /agent/apps` (daemon, issue #76). Until
//! that endpoint exists, a daemon on loopback lets the CLI ask `devicectl`
//! directly; results are cached in the flow store for ten minutes.

use crate::client::DaemonClient;
use crate::flow::FlowMeta;
use crate::registry;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

const CACHE_FILE: &str = ".apps-cache.json";
const CACHE_TTL_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

/// Dotted numeric compare: `8.0.76` < `8.1`, `27.0` == `27`. Non-numeric
/// tails are ignored (`26.0b3` reads as `26.0`). Returns `None` when either
/// side has no leading digits at all.
pub fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let out: Vec<u64> = v
            .trim()
            .split('.')
            .map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u64>().ok()
            })
            .take_while(|p| p.is_some())
            .map(|p| p.unwrap())
            .collect();
        (!out.is_empty()).then_some(out)
    }
    let (mut a, mut b) = (parts(a)?, parts(b)?);
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    Some(a.cmp(&b))
}

// ---------------------------------------------------------------------------
// Installed apps
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstalledApp {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub bundle_version: Option<String>,
    #[serde(default)]
    pub system: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstalledApps {
    #[serde(default)]
    pub ios: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub fetched_at: u64,
    #[serde(default)]
    pub apps: BTreeMap<String, InstalledApp>,
}

impl InstalledApps {
    /// The version a flow for `bundle` should be compared against: the iOS
    /// version for Apple system apps (their own `version` is a placeholder),
    /// the marketing version otherwise. `None` when the app is not installed
    /// or nothing is known.
    pub fn compat_version(&self, bundle: &str) -> Option<String> {
        let app = self.apps.get(bundle);
        let is_system = app.map(|a| a.system).unwrap_or(false)
            || (bundle.starts_with("com.apple.") && self.apps.is_empty());
        if bundle.starts_with("com.apple.") && (is_system || app.is_none()) {
            return self.ios.clone();
        }
        app.and_then(|a| a.version.clone())
    }

    pub fn installed(&self, bundle: &str) -> Option<bool> {
        if self.apps.is_empty() {
            return None;
        }
        Some(self.apps.contains_key(bundle) || bundle.starts_with("com.apple."))
    }

    /// Parse the daemon's `GET /agent/apps` body (issue #76 contract).
    pub fn from_daemon_json(body: &str) -> Result<Self> {
        let v: serde_json::Value = serde_json::from_str(body).context("parse /agent/apps")?;
        let mut out = InstalledApps {
            ios: v["device"]["ios"].as_str().map(str::to_string),
            device: v["device"]["marketing_name"].as_str().map(str::to_string),
            source: v["source"].as_str().unwrap_or("daemon").to_string(),
            fetched_at: now(),
            apps: BTreeMap::new(),
        };
        for app in v["apps"].as_array().into_iter().flatten() {
            let Some(bundle) = app["bundle"].as_str() else {
                continue;
            };
            out.apps.insert(
                bundle.to_string(),
                InstalledApp {
                    name: app["name"].as_str().map(str::to_string),
                    version: app["version"].as_str().map(str::to_string),
                    bundle_version: app["bundle_version"].as_str().map(str::to_string),
                    system: app["system"].as_bool().unwrap_or(false),
                },
            );
        }
        Ok(out)
    }

    /// Parse `devicectl device info apps --json-output` plus
    /// `devicectl device info details --json-output`.
    pub fn from_devicectl_json(apps_json: &str, details_json: Option<&str>) -> Result<Self> {
        let apps: serde_json::Value =
            serde_json::from_str(apps_json).context("parse devicectl apps JSON")?;
        let mut out = InstalledApps {
            source: "devicectl".to_string(),
            fetched_at: now(),
            ..Default::default()
        };
        if let Some(details) = details_json {
            let d: serde_json::Value =
                serde_json::from_str(details).context("parse devicectl details JSON")?;
            out.ios = d["result"]["deviceProperties"]["osVersionNumber"]
                .as_str()
                .map(str::to_string);
            out.device = d["result"]["hardwareProperties"]["marketingName"]
                .as_str()
                .map(str::to_string);
        }
        for app in apps["result"]["apps"].as_array().into_iter().flatten() {
            let Some(bundle) = app["bundleIdentifier"].as_str() else {
                continue;
            };
            out.apps.insert(
                bundle.to_string(),
                InstalledApp {
                    name: app["name"].as_str().map(str::to_string),
                    version: app["version"].as_str().map(str::to_string),
                    bundle_version: app["bundleVersion"].as_str().map(str::to_string),
                    system: app["defaultApp"].as_bool().unwrap_or(false),
                },
            );
        }
        Ok(out)
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> Option<std::path::PathBuf> {
    registry::store_dir().ok().map(|d| d.join(CACHE_FILE))
}

fn read_cache() -> Option<InstalledApps> {
    let path = cache_path()?;
    let bytes = std::fs::read(path).ok()?;
    let apps: InstalledApps = serde_json::from_slice(&bytes).ok()?;
    (now().saturating_sub(apps.fetched_at) < CACHE_TTL_SECS && !apps.apps.is_empty())
        .then_some(apps)
}

fn write_cache(apps: &InstalledApps) {
    if let Some(path) = cache_path() {
        if let Ok(bytes) = serde_json::to_vec(apps) {
            let _ = std::fs::write(path, bytes);
        }
    }
}

fn is_loopback(url: &str) -> bool {
    url.contains("://127.0.0.1") || url.contains("://localhost") || url.contains("://[::1]")
}

/// Run `xcrun devicectl` on this Mac. Only meaningful when the daemon (and
/// therefore the phone) is on this machine.
fn devicectl_apps(udid: &str) -> Result<InstalledApps> {
    let dir = tempfile::tempdir().context("temp dir for devicectl output")?;
    let apps_out = dir.path().join("apps.json");
    let details_out = dir.path().join("details.json");
    let run = |args: &[&str]| -> Result<()> {
        let status = std::process::Command::new("xcrun")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("run xcrun devicectl")?;
        anyhow::ensure!(status.success(), "devicectl exited with {status}");
        Ok(())
    };
    run(&[
        "devicectl",
        "device",
        "info",
        "apps",
        "--device",
        udid,
        "--include-all-apps",
        "--json-output",
        apps_out.to_str().unwrap(),
    ])?;
    let _ = run(&[
        "devicectl",
        "device",
        "info",
        "details",
        "--device",
        udid,
        "--json-output",
        details_out.to_str().unwrap(),
    ]);
    let apps_json = std::fs::read_to_string(&apps_out)?;
    let details_json = std::fs::read_to_string(&details_out).ok();
    InstalledApps::from_devicectl_json(&apps_json, details_json.as_deref())
}

/// Best-effort installed-app inventory: daemon endpoint first, then the
/// local devicectl fallback for a loopback daemon, with a short cache. Never
/// fails — compat simply becomes `unknown`.
pub async fn installed_apps(daemon: &DaemonClient) -> Option<InstalledApps> {
    if let Some(cached) = read_cache() {
        return Some(cached);
    }
    if let Ok(body) = daemon.get_text("/agent/apps").await {
        if let Ok(apps) = InstalledApps::from_daemon_json(&body) {
            if !apps.apps.is_empty() {
                write_cache(&apps);
                return Some(apps);
            }
        }
    }
    if !is_loopback(daemon.base_url()) {
        return None;
    }
    let udid = daemon
        .get_text("/agent/status")
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["udid"].as_str().map(str::to_string))?;
    let apps = tokio::task::spawn_blocking(move || devicectl_apps(&udid))
        .await
        .ok()?
        .ok()?;
    write_cache(&apps);
    Some(apps)
}

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Compat {
    /// Installed version is within the range this file was proved on.
    Verified,
    /// Installed version is newer than the last verified one: run it, then
    /// take a checkpoint and publish the new `verified_on` if it worked.
    UntestedNewer,
    /// Installed version is below `app_version_min`, or the app is missing.
    Incompatible,
    /// Tagged `broken` in the registry (a report confirmed it fails).
    Broken,
    /// Tagged `needs-verification` (re-verification failed; awaiting a fix).
    NeedsVerification,
    /// No hardware verification on record yet.
    Draft,
    /// Installed version not known (no daemon, no devicectl, or app has no
    /// `app` metadata).
    Unknown,
}

impl Compat {
    pub fn as_str(self) -> &'static str {
        match self {
            Compat::Verified => "verified",
            Compat::UntestedNewer => "untested-newer",
            Compat::Incompatible => "incompatible",
            Compat::Broken => "broken",
            Compat::NeedsVerification => "needs-verification",
            Compat::Draft => "draft",
            Compat::Unknown => "unknown",
        }
    }
    /// `flow run` refuses these without `--force`.
    pub fn blocks_run(self) -> bool {
        matches!(self, Compat::Incompatible | Compat::Broken)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CompatReport {
    pub compat: Compat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_up_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version_min: Option<String>,
    pub reason: &'static str,
}

pub fn compat_for(meta: &FlowMeta, installed: Option<&InstalledApps>) -> CompatReport {
    let verified_up_to = meta.verified_up_to();
    let mut report = CompatReport {
        compat: Compat::Unknown,
        installed_version: None,
        verified_up_to: verified_up_to.clone(),
        app_version_min: meta.app_version_min.clone(),
        reason: "installed app version not known",
    };
    if meta.tags.iter().any(|t| t == "broken") {
        report.compat = Compat::Broken;
        report.reason = "tagged broken in the registry";
        return report;
    }
    if meta.tags.iter().any(|t| t == "needs-verification") {
        report.compat = Compat::NeedsVerification;
        report.reason = "re-verification failed; awaiting a fix";
        return report;
    }
    let Some(bundle) = meta.app.as_deref() else {
        report.reason = "flow declares no app";
        report.compat = if meta.verified() {
            Compat::Verified
        } else {
            Compat::Draft
        };
        return report;
    };
    let Some(installed) = installed else {
        if !meta.verified() {
            report.compat = Compat::Draft;
            report.reason = "no hardware verification on record";
        }
        return report;
    };
    if installed.installed(bundle) == Some(false) {
        report.compat = Compat::Incompatible;
        report.reason = "app is not installed on this phone";
        return report;
    }
    let Some(version) = installed.compat_version(bundle) else {
        if !meta.verified() {
            report.compat = Compat::Draft;
            report.reason = "no hardware verification on record";
        }
        return report;
    };
    report.installed_version = Some(version.clone());
    if let Some(min) = meta.app_version_min.as_deref() {
        if compare_versions(&version, min) == Some(Ordering::Less) {
            report.compat = Compat::Incompatible;
            report.reason = "installed version is below app_version_min";
            return report;
        }
    }
    if !meta.verified() {
        report.compat = Compat::Draft;
        report.reason = "no hardware verification on record";
        return report;
    }
    match verified_up_to
        .as_deref()
        .and_then(|up| compare_versions(&version, up))
    {
        Some(Ordering::Greater) => {
            report.compat = Compat::UntestedNewer;
            report.reason = "installed version is newer than the last verified one";
        }
        Some(_) => {
            report.compat = Compat::Verified;
            report.reason = "installed version within the verified range";
        }
        None => {
            report.compat = Compat::Verified;
            report.reason = "verified on hardware; version not comparable";
        }
    }
    report
}

/// Human-facing one-liner for `flow list`.
pub fn compat_label(report: &CompatReport) -> String {
    match (&report.installed_version, report.compat) {
        (Some(v), Compat::UntestedNewer) => format!(
            "untested-newer ({v} > {})",
            report.verified_up_to.as_deref().unwrap_or("?")
        ),
        (Some(v), Compat::Incompatible) => format!("incompatible ({v})"),
        (Some(_), other) => other.as_str().to_string(),
        (None, other) => other.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::FlowVerification;

    fn meta(app: Option<&str>, verified: &[&str], min: Option<&str>, tags: &[&str]) -> FlowMeta {
        FlowMeta {
            name: "t".into(),
            description: None,
            steps: 1,
            inputs: vec![],
            app: app.map(String::from),
            category: None,
            risk: None,
            locale: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            verified_on: verified
                .iter()
                .map(|v| FlowVerification {
                    app_version: Some(v.to_string()),
                    ..Default::default()
                })
                .collect(),
            app_version_min: min.map(String::from),
            example_inputs: Default::default(),
        }
    }

    fn installed(ios: &str, apps: &[(&str, &str, bool)]) -> InstalledApps {
        InstalledApps {
            ios: Some(ios.into()),
            apps: apps
                .iter()
                .map(|(b, v, sys)| {
                    (
                        b.to_string(),
                        InstalledApp {
                            version: Some(v.to_string()),
                            system: *sys,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn version_compare_is_numeric_and_tolerant() {
        assert_eq!(compare_versions("8.0.76", "8.1"), Some(Ordering::Less));
        assert_eq!(compare_versions("27.0", "27"), Some(Ordering::Equal));
        assert_eq!(compare_versions("26.0b3", "26"), Some(Ordering::Equal));
        assert_eq!(compare_versions("10.2", "9.9.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("abc", "1"), None);
    }

    #[test]
    fn system_apps_compare_against_ios_and_third_party_against_app_version() {
        let inst = installed(
            "27.0",
            &[
                ("com.apple.Health", "1.0", true),
                ("com.tencent.xin", "8.0.76", false),
            ],
        );
        assert_eq!(
            inst.compat_version("com.apple.Health").as_deref(),
            Some("27.0")
        );
        assert_eq!(
            inst.compat_version("com.tencent.xin").as_deref(),
            Some("8.0.76")
        );
        assert_eq!(inst.compat_version("com.example.missing"), None);
        assert_eq!(inst.installed("com.example.missing"), Some(false));
    }

    #[test]
    fn compat_matrix() {
        let inst = installed(
            "27.0",
            &[
                ("com.apple.Health", "1.0", true),
                ("com.tencent.xin", "8.0.76", false),
            ],
        );
        let v = |m: FlowMeta| compat_for(&m, Some(&inst)).compat;
        assert_eq!(
            v(meta(Some("com.apple.Health"), &["27.0"], None, &[])),
            Compat::Verified
        );
        assert_eq!(
            v(meta(Some("com.apple.Health"), &["26.1"], None, &[])),
            Compat::UntestedNewer
        );
        assert_eq!(
            v(meta(Some("com.tencent.xin"), &["8.0.80"], None, &[])),
            Compat::Verified
        );
        assert_eq!(
            v(meta(Some("com.tencent.xin"), &["8.0.76"], Some("9.0"), &[])),
            Compat::Incompatible
        );
        assert_eq!(
            v(meta(Some("com.example.missing"), &["1.0"], None, &[])),
            Compat::Incompatible
        );
        assert_eq!(
            v(meta(Some("com.apple.Health"), &[], None, &[])),
            Compat::Draft
        );
        assert_eq!(
            v(meta(Some("com.apple.Health"), &["27.0"], None, &["broken"])),
            Compat::Broken
        );
        assert_eq!(
            v(meta(
                Some("com.apple.Health"),
                &["27.0"],
                None,
                &["needs-verification"]
            )),
            Compat::NeedsVerification
        );
        assert_eq!(
            compat_for(&meta(Some("com.apple.Health"), &["27.0"], None, &[]), None).compat,
            Compat::Unknown
        );
        assert_eq!(
            compat_for(&meta(None, &["27.0"], None, &[]), None).compat,
            Compat::Verified
        );
        assert!(
            Compat::Broken.blocks_run()
                && Compat::Incompatible.blocks_run()
                && !Compat::UntestedNewer.blocks_run()
        );
    }

    #[test]
    fn parses_daemon_and_devicectl_shapes() {
        let daemon = r#"{"ok":true,"device":{"ios":"27.0","marketing_name":"iPhone 17 Pro Max"},"source":"devicectl","apps":[{"bundle":"com.tencent.xin","name":"WeChat","version":"8.0.76","system":false},{"bundle":"com.apple.Health","version":"1.0","system":true}]}"#;
        let a = InstalledApps::from_daemon_json(daemon).unwrap();
        assert_eq!(a.ios.as_deref(), Some("27.0"));
        assert_eq!(
            a.compat_version("com.apple.Health").as_deref(),
            Some("27.0")
        );
        assert_eq!(
            a.compat_version("com.tencent.xin").as_deref(),
            Some("8.0.76")
        );

        let apps = r#"{"result":{"apps":[{"bundleIdentifier":"com.apple.Health","version":"1.0","bundleVersion":"7027.0.72.2.7","name":"Health","defaultApp":true},{"bundleIdentifier":"com.tencent.xin","version":"8.0.76","name":"WeChat","defaultApp":false}]}}"#;
        let details = r#"{"result":{"deviceProperties":{"osVersionNumber":"27.0"},"hardwareProperties":{"marketingName":"iPhone 17 Pro Max"}}}"#;
        let d = InstalledApps::from_devicectl_json(apps, Some(details)).unwrap();
        assert_eq!(d.source, "devicectl");
        assert_eq!(d.device.as_deref(), Some("iPhone 17 Pro Max"));
        assert!(d.apps["com.apple.Health"].system);
        assert_eq!(
            compat_label(&compat_for(
                &meta(Some("com.apple.Health"), &["26.0"], None, &[]),
                Some(&d)
            )),
            "untested-newer (27.0 > 26.0)"
        );
    }
}
