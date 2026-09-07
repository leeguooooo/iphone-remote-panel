//! Deterministic, file-backed flow validation and execution.
//!
//! The file format deliberately reuses the exact `PhoneStep` input accepted by
//! the MCP `phone_run_steps` tool. A saved flow is therefore a low-token replay
//! surface, not a second automation engine with subtly different semantics.
//!
//! Version 1 files may additionally carry *registry metadata* (`app`,
//! `category`, `risk`, `locale`, `tags`, `verified_on`). Every metadata field
//! is optional so a flow recorded by the browser stays valid; the fields only
//! matter to `flow list`, to the side-effect confirmation gate, and to the
//! official flow registry (see `registry.rs`).

use crate::client::DaemonClient;
use crate::registry;
use crate::server::{phone_steps_request, PhoneStep};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub const FLOW_VERSION: u32 = 1;
pub const MAX_FLOW_BYTES: u64 = 64 * 1024;
const MAX_NAME_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 1_000;
const MAX_INPUTS: usize = 16;
const MAX_INPUT_NAME_CHARS: usize = 64;
const MAX_INPUT_DESCRIPTION_CHARS: usize = 200;
const MAX_META_CHARS: usize = 64;
const MAX_TAGS: usize = 8;
const MAX_VERIFICATIONS: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlowDocument {
    version: u32,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    inputs: BTreeMap<String, FlowInputDefinition>,
    steps: Vec<serde_json::Value>,
    // ---- registry metadata (all optional) ----
    /// Bundle identifier of the app this flow operates, e.g. `com.apple.Health`.
    #[serde(default)]
    app: Option<String>,
    /// Registry category slug, e.g. `health`, `system`, `finance`, `im`.
    #[serde(default)]
    category: Option<String>,
    /// What the flow does to the world. `side_effect` flows need `--confirm`.
    #[serde(default)]
    risk: Option<FlowRisk>,
    /// BCP-47-ish UI locale the labels were recorded under, e.g. `en`, `zh-CN`.
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    /// Hardware runs that proved this exact file. Empty means unverified.
    #[serde(default)]
    verified_on: Vec<FlowVerification>,
    /// Lowest app version (iOS version for Apple system apps) this flow is
    /// known to work on. Installed versions below it are `incompatible`.
    #[serde(default)]
    app_version_min: Option<String>,
    /// Harmless example values for the declared inputs, so re-verification
    /// (and a curious human) can run a parameterized flow unattended.
    #[serde(default)]
    example_inputs: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlowInputDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_required")]
    pub required: bool,
}

/// Risk class of a flow. Missing metadata is treated as `Unknown`, which is
/// allowed to run (backwards compatible) but is reported as such.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowRisk {
    /// Only reads or navigates; nothing leaves the phone.
    ReadOnly,
    /// Changes on-device UI state (opens an app, moves between screens).
    Navigation,
    /// Sends, publishes, pays, deletes, or otherwise acts on the outside world.
    SideEffect,
}

impl FlowRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            FlowRisk::ReadOnly => "read_only",
            FlowRisk::Navigation => "navigation",
            FlowRisk::SideEffect => "side_effect",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FlowVerification {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ios: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

fn default_required() -> bool {
    true
}

/// Registry-facing metadata of a validated flow. Serialized into the local
/// store index so `flow list` never has to re-parse every file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub steps: usize,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<FlowRisk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verified_on: Vec<FlowVerification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version_min: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub example_inputs: BTreeMap<String, String>,
}

impl FlowMeta {
    pub fn verified(&self) -> bool {
        !self.verified_on.is_empty()
    }
    /// Highest `app_version` among the recorded verifications.
    pub fn verified_up_to(&self) -> Option<String> {
        let mut best: Option<String> = None;
        for v in self
            .verified_on
            .iter()
            .filter_map(|v| v.app_version.as_deref())
        {
            match &best {
                Some(b)
                    if crate::compat::compare_versions(v, b)
                        != Some(std::cmp::Ordering::Greater) => {}
                _ => best = Some(v.to_string()),
            }
        }
        best
    }
    pub fn risk_label(&self) -> &'static str {
        self.risk.map(FlowRisk::as_str).unwrap_or("unknown")
    }
}

#[derive(Debug)]
pub struct ValidatedFlow {
    pub meta: FlowMeta,
    pub inputs: BTreeMap<String, FlowInputDefinition>,
    pub step_templates: Vec<serde_json::Value>,
}

impl ValidatedFlow {
    pub fn name(&self) -> &str {
        &self.meta.name
    }
}

fn valid_input_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && name.chars().count() <= MAX_INPUT_NAME_CHARS
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

/// Lowercase slug used for categories and tags: `[a-z0-9][a-z0-9_-]*`.
pub fn valid_slug(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
        && value.len() <= MAX_META_CHARS
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-'))
}

/// Reverse-DNS bundle identifier: dot-separated labels of `[A-Za-z0-9-]`.
fn valid_bundle_id(value: &str) -> bool {
    value.len() <= 255
        && value.contains('.')
        && value.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
}

/// `en`, `zh-CN`, `ja-JP`, `zh-Hans-CN`.
fn valid_locale(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(language) = parts.next() else {
        return false;
    };
    (2..=3).contains(&language.len())
        && language.chars().all(|ch| ch.is_ascii_lowercase())
        && parts.all(|part| {
            (2..=8).contains(&part.len()) && part.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
}

fn short_printable(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_META_CHARS
        && !value.chars().any(char::is_control)
}

fn validate_metadata(document: &FlowDocument) -> Result<()> {
    if let Some(app) = &document.app {
        if !valid_bundle_id(app) {
            bail!(
                "flow app {app:?} must be a reverse-DNS bundle identifier such as com.apple.Health"
            );
        }
    }
    if let Some(category) = &document.category {
        if !valid_slug(category) {
            bail!("flow category {category:?} must be a lowercase slug ([a-z0-9][a-z0-9_-]*)");
        }
    }
    if let Some(locale) = &document.locale {
        if !valid_locale(locale) {
            bail!("flow locale {locale:?} must look like en, zh-CN or ja-JP");
        }
    }
    if document.tags.len() > MAX_TAGS {
        bail!("flow tags exceeds the maximum of {MAX_TAGS}");
    }
    let mut seen = BTreeSet::new();
    for tag in &document.tags {
        if !valid_slug(tag) {
            bail!("flow tag {tag:?} must be a lowercase slug");
        }
        if !seen.insert(tag) {
            bail!("flow tag {tag:?} is listed more than once");
        }
    }
    if let Some(min) = &document.app_version_min {
        if !short_printable(min) || crate::compat::compare_versions(min, "0").is_none() {
            bail!("flow app_version_min {min:?} must be a dotted numeric version such as 27.0 or 8.0.76");
        }
    }
    if document.example_inputs.len() > MAX_INPUTS {
        bail!("flow example_inputs exceeds the maximum of {MAX_INPUTS}");
    }
    for (name, value) in &document.example_inputs {
        if !document.inputs.contains_key(name) {
            bail!("flow example_inputs names undefined input {name:?}");
        }
        if value.is_empty() || value.chars().count() > 200 || value.chars().any(char::is_control) {
            bail!("flow example_inputs[{name:?}] must contain 1 to 200 printable characters");
        }
    }
    if document.verified_on.len() > MAX_VERIFICATIONS {
        bail!("flow verified_on exceeds the maximum of {MAX_VERIFICATIONS} entries");
    }
    for (index, verification) in document.verified_on.iter().enumerate() {
        let fields = [
            ("device", &verification.device),
            ("ios", &verification.ios),
            ("app_version", &verification.app_version),
            ("date", &verification.date),
        ];
        if fields.iter().all(|(_, value)| value.is_none()) {
            bail!("flow verified_on[{index}] must name at least one of device, ios, app_version, date");
        }
        for (field, value) in fields {
            if let Some(value) = value {
                if !short_printable(value) {
                    bail!(
                        "flow verified_on[{index}].{field} must contain 1 to {MAX_META_CHARS} printable characters"
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_input_definitions(inputs: &BTreeMap<String, FlowInputDefinition>) -> Result<()> {
    if inputs.len() > MAX_INPUTS {
        bail!("flow inputs exceeds the maximum of {MAX_INPUTS}");
    }
    for (name, definition) in inputs {
        if !valid_input_name(name) {
            bail!(
                "flow input name {name:?} must start with an ASCII letter and contain at most \
                 {MAX_INPUT_NAME_CHARS} ASCII letters, digits, '-' or '_'"
            );
        }
        if definition.kind != "string" {
            bail!(
                "flow input {name:?} has unsupported type {:?}; expected \"string\"",
                definition.kind
            );
        }
        if definition.description.as_ref().is_some_and(|description| {
            description.chars().count() > MAX_INPUT_DESCRIPTION_CHARS
                || description.chars().any(char::is_control)
        }) {
            bail!(
                "flow input {name:?} description must contain at most \
                 {MAX_INPUT_DESCRIPTION_CHARS} printable characters"
            );
        }
    }
    Ok(())
}

fn referenced_input(step: &serde_json::Value) -> Result<Option<&str>> {
    let Some(object) = step.as_object() else {
        bail!("every flow step must be a JSON object");
    };
    let input = object.get("input");
    if input.is_none() {
        return Ok(None);
    }
    if object.get("kind").and_then(serde_json::Value::as_str) != Some("type") {
        bail!("only a kind=\"type\" flow step may reference an input");
    }
    if object.contains_key("text") {
        bail!("a kind=\"type\" step must use either input or text, never both");
    }
    input
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("a kind=\"type\" step input must be a non-empty string"))
}

fn materialize_steps(
    templates: &[serde_json::Value],
    definitions: &BTreeMap<String, FlowInputDefinition>,
    values: Option<&BTreeMap<String, String>>,
) -> Result<Vec<PhoneStep>> {
    let mut referenced = BTreeSet::new();
    let mut steps = Vec::with_capacity(templates.len());
    for (index, template) in templates.iter().enumerate() {
        let mut materialized = template.clone();
        if let Some(name) = referenced_input(template)
            .with_context(|| format!("validate steps[{index}] input reference"))?
        {
            let definition = definitions.get(name).ok_or_else(|| {
                anyhow::anyhow!("steps[{index}] references undefined flow input {name:?}")
            })?;
            referenced.insert(name.to_string());
            let value = match values.and_then(|provided| provided.get(name)) {
                Some(value) => value.clone(),
                None if values.is_none() || !definition.required => String::new(),
                None => bail!("missing required flow input {name:?}; no action was sent"),
            };
            let object = materialized
                .as_object_mut()
                .expect("referenced_input already proved this step is an object");
            object.remove("input");
            object.insert("text".to_string(), serde_json::Value::String(value));
        }
        let step = serde_json::from_value::<PhoneStep>(materialized)
            .with_context(|| format!("validate steps[{index}]"))?;
        steps.push(step);
    }
    let unused = definitions
        .keys()
        .filter(|name| !referenced.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !unused.is_empty() {
        bail!("flow defines unused inputs: {}", unused.join(", "));
    }
    Ok(steps)
}

pub fn parse_input_assignments(
    assignments: &[String],
    definitions: &BTreeMap<String, FlowInputDefinition>,
) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for assignment in assignments {
        let (name, value) = assignment
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("flow input must use KEY=VALUE form"))?;
        if !definitions.contains_key(name) {
            bail!("unknown flow input {name:?}; no action was sent");
        }
        if values.insert(name.to_string(), value.to_string()).is_some() {
            bail!("flow input {name:?} was provided more than once; no action was sent");
        }
    }
    Ok(values)
}

/// Check a caller-supplied input map (MCP) against the flow definition.
pub fn check_input_map(
    values: &BTreeMap<String, String>,
    definitions: &BTreeMap<String, FlowInputDefinition>,
) -> Result<()> {
    for name in values.keys() {
        if !definitions.contains_key(name) {
            bail!("unknown flow input {name:?}; no action was sent");
        }
    }
    Ok(())
}

/// Read a flow file with the same tamper checks the runner has always
/// applied: no symlinks, regular file, owned by the current uid, not
/// group/world-writable, 1..=64 KiB.
pub fn read_flow_bytes(path: &Path) -> Result<Vec<u8>> {
    // O_NOFOLLOW makes validation and execution reject a last-component
    // symlink without a metadata/open race. Flow files can contain text and
    // taps with real-world effects, so only regular, current-user-owned files
    // that are not group/world-writable are accepted.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "open flow file without following symlinks: {}",
                path.display()
            )
        })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect flow file: {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("flow path is not a regular file: {}", path.display());
    }
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        bail!(
            "flow file is not owned by the current user (uid {effective_uid}): {}",
            path.display()
        );
    }
    if metadata.mode() & 0o022 != 0 {
        bail!(
            "flow file must not be group- or world-writable: {}",
            path.display()
        );
    }
    if metadata.len() == 0 || metadata.len() > MAX_FLOW_BYTES {
        bail!(
            "flow file size must be between 1 and {MAX_FLOW_BYTES} bytes: {}",
            path.display()
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read flow file: {}", path.display()))?;
    if bytes.len() as u64 > MAX_FLOW_BYTES {
        bail!("flow file grew beyond {MAX_FLOW_BYTES} bytes while being read");
    }
    Ok(bytes)
}

/// Parse and fully validate flow JSON. `origin` only labels error messages.
pub fn parse_flow(bytes: &[u8], origin: &str) -> Result<ValidatedFlow> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_FLOW_BYTES {
        bail!("flow document size must be between 1 and {MAX_FLOW_BYTES} bytes: {origin}");
    }
    let document: FlowDocument =
        serde_json::from_slice(bytes).with_context(|| format!("parse flow JSON: {origin}"))?;
    if document.version != FLOW_VERSION {
        bail!(
            "unsupported flow version {}; expected {FLOW_VERSION}",
            document.version
        );
    }
    let name = document.name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        bail!("flow name must contain 1 to {MAX_NAME_CHARS} printable characters");
    }
    if document
        .description
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_DESCRIPTION_CHARS)
    {
        bail!("flow description exceeds {MAX_DESCRIPTION_CHARS} characters");
    }

    validate_metadata(&document)?;
    validate_input_definitions(&document.inputs)?;
    let step_count = document.steps.len();
    let validated_steps = materialize_steps(&document.steps, &document.inputs, None)?;
    phone_steps_request(validated_steps).map_err(anyhow::Error::msg)?;
    Ok(ValidatedFlow {
        meta: FlowMeta {
            name: name.to_string(),
            description: document.description,
            steps: step_count,
            inputs: document.inputs.keys().cloned().collect(),
            app: document.app,
            category: document.category,
            risk: document.risk,
            locale: document.locale,
            tags: document.tags,
            verified_on: document.verified_on,
            app_version_min: document.app_version_min,
            example_inputs: document.example_inputs,
        },
        inputs: document.inputs,
        step_templates: document.steps,
    })
}

pub fn load_flow(path: &Path) -> Result<ValidatedFlow> {
    let bytes = read_flow_bytes(path)?;
    parse_flow(&bytes, &path.display().to_string())
}

/// JSON summary shared by `flow validate` and `flow info`.
pub fn flow_summary(flow: &ValidatedFlow) -> serde_json::Value {
    let mut value = serde_json::to_value(&flow.meta).expect("FlowMeta serializes");
    let object = value.as_object_mut().expect("FlowMeta is an object");
    object.insert("ok".into(), serde_json::Value::Bool(true));
    object.insert("version".into(), serde_json::json!(FLOW_VERSION));
    object.insert(
        "verified".into(),
        serde_json::Value::Bool(flow.meta.verified()),
    );
    object.insert("risk".into(), serde_json::json!(flow.meta.risk_label()));
    value
}

/// `flow validate <file|id>`: offline, never contacts the daemon.
pub fn validate_command(target: &str) -> Result<()> {
    let path = registry::resolve_target(target)?;
    let flow = load_flow(&path)?;
    let mut summary = flow_summary(&flow);
    summary["path"] = serde_json::json!(path.display().to_string());
    summary["network"] = serde_json::json!("not_contacted");
    println!("{summary}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Run evidence (`--artifacts-dir`)
// ---------------------------------------------------------------------------

/// Version of the on-disk evidence shape. Bump when a reader would break.
const EVIDENCE_SCHEMA: &str = "iphone-use/flow-run-evidence@1";

/// A prepared, writable evidence directory.
///
/// Preparation happens BEFORE anything is sent to the phone: a directory that
/// cannot be created is a problem worth failing on while nothing has happened
/// yet. After the run, a write failure can no longer undo what the phone did,
/// so it is reported alongside the result instead of replacing it.
#[derive(Debug)]
pub struct ArtifactsDir {
    dir: std::path::PathBuf,
}

impl ArtifactsDir {
    /// Prepare a private per-run directory under `base`. Call before the first
    /// mutation.
    ///
    /// The base directory is the user's and is left exactly as they made it —
    /// a `755` directory they already had is not silently tightened. What this
    /// run writes goes into its own subdirectory, created EXCLUSIVELY with
    /// mode 0700 at creation time. Creating with the mode (rather than
    /// `exists()` + `create_dir_all` + `chmod`) leaves no window in which the
    /// directory is readable by anyone else, and no check-then-create race.
    pub fn prepare(base: &Path) -> Result<Self> {
        std::fs::create_dir_all(base)
            .with_context(|| format!("create artifacts directory {}", base.display()))?;
        let mut last = None;
        for _ in 0..8 {
            let dir = base.join(format!("run-{}", unique_suffix()));
            let created = {
                use std::os::unix::fs::DirBuilderExt as _;
                std::fs::DirBuilder::new()
                    .recursive(false)
                    .mode(0o700)
                    .create(&dir)
            };
            match created {
                Ok(()) => return Ok(Self { dir }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = Some((dir, error));
                    continue;
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error)).with_context(|| {
                        format!("create private run directory under {}", base.display())
                    });
                }
            }
        }
        let (dir, error) = last.expect("the loop only exits here after a collision");
        Err(anyhow::Error::new(error))
            .with_context(|| format!("create private run directory {}", dir.display()))
    }

    /// The directory this run writes into.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write one run's evidence, created exclusively with mode 0600.
    ///
    /// `create_new` means this never overwrites existing evidence and never
    /// follows a symlink someone planted at the target path — the file is
    /// private from the moment it exists, rather than being written first and
    /// tightened afterwards.
    fn write(&self, evidence: &serde_json::Value, stem: &str) -> Result<std::path::PathBuf> {
        let body = serde_json::to_vec_pretty(evidence).context("serialize run evidence")?;
        let mut last_error = None;
        for attempt in 0..8 {
            let name = if attempt == 0 {
                format!("{stem}.json")
            } else {
                format!("{stem}-{}.json", unique_suffix())
            };
            let path = self.dir.join(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write as _;
                    file.write_all(&body)
                        .with_context(|| format!("write run evidence {}", path.display()))?;
                    return Ok(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_error = Some((path, error));
                    continue;
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error))
                        .with_context(|| format!("write run evidence {}", path.display()));
                }
            }
        }
        let (path, error) = last_error.expect("the loop only exits here after a collision");
        Err(anyhow::Error::new(error))
            .with_context(|| format!("write run evidence {}", path.display()))
    }
}

/// A short, monotonic-ish suffix that makes a filename unique within a second.
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// Record where the evidence went — or why it did not get there.
///
/// The phone has already acted by the time this runs. A failure to WRITE that
/// down cannot undo it and must not look like a failed run, so the outcome
/// fields are left exactly as the daemon reported them and the problem is
/// reported beside them as `artifact_error`.
fn attach_artifact(value: &mut serde_json::Value, written: Result<std::path::PathBuf>) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    match written {
        Ok(path) => {
            object.insert(
                "artifact".into(),
                serde_json::json!(path.display().to_string()),
            );
        }
        Err(error) => {
            object.insert(
                "artifact_error".into(),
                serde_json::json!(format!("{error:#}")),
            );
        }
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Assemble the machine-readable record of one run.
///
/// Only white-listed STRUCTURE goes in: the result passes through the same
/// projection a public issue gets ([`crate::contrib::redact_result`]), so
/// neither typed input nor screen text is written to disk by default. What a
/// reader needs to reproduce or triage — the flow's identity and hash, the
/// versions the run happened against, the outcome, and per-step structure — is
/// all here.
///
/// A version that could not be read is reported as `unavailable`. It is never
/// guessed, and nothing here starts or claims the device to find one out.
#[allow(clippy::too_many_arguments)]
fn build_evidence(
    target: &str,
    flow: &ValidatedFlow,
    sha256: Option<&str>,
    compat: &crate::compat::CompatReport,
    installed: Option<&crate::compat::InstalledApps>,
    daemon_version: Option<&str>,
    result: &serde_json::Value,
    started_at: u64,
    duration_ms: u64,
) -> serde_json::Value {
    let unavailable = || serde_json::Value::String("unavailable".to_string());
    serde_json::json!({
        "schema": EVIDENCE_SCHEMA,
        "flow": {
            "target": target,
            "name": flow.name(),
            "steps": flow.meta.steps,
            "risk": flow.meta.risk_label(),
            "verified": flow.meta.verified(),
            "sha256": sha256.map_or_else(unavailable, |sha| serde_json::json!(sha)),
        },
        "versions": {
            "daemon": daemon_version.map_or_else(unavailable, |v| serde_json::json!(v)),
            "app": compat
                .installed_version
                .as_deref()
                .map_or_else(unavailable, |v| serde_json::json!(v)),
            "ios": installed
                .and_then(|apps| apps.ios.as_deref())
                .map_or_else(unavailable, |v| serde_json::json!(v)),
            "verified_up_to": compat
                .verified_up_to
                .as_deref()
                .map_or_else(unavailable, |v| serde_json::json!(v)),
            "compat": compat.compat.as_str(),
        },
        "run": {
            "started_at_unix": started_at,
            // Measured with a monotonic clock, not a difference of wall-clock
            // seconds: a run under a second is not a run of zero.
            "duration_ms": duration_ms,
        },
        // Structure only — same projection a public report gets.
        "result": crate::contrib::redact_result(result),
    })
}

// ---------------------------------------------------------------------------
// Failure diagnosis (B1/B3)
// ---------------------------------------------------------------------------

/// How long the post-failure look at the screen may take. Deliberately short:
/// the run is already over and its result is already decided, so diagnosis is
/// a courtesy that must never become the reason a command hangs.
const DIAGNOSIS_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// At most this many candidate elements are reported. A diagnosis is a lead,
/// not a screen dump.
const MAX_CANDIDATES: usize = 5;

/// Locator fields the daemon matches on, in the order a reader scans them.
const LOCATOR_FIELDS: [&str; 7] = [
    "label",
    "identifier",
    "kind",
    "value",
    "focused",
    "enabled",
    "visible",
];

/// The locator a failed step was looking for, if it had one.
///
/// Deliberately conservative: only the shapes the daemon itself matches on are
/// understood, and an unrecognised step yields `None` rather than a guess.
fn failed_step_locator(
    step: &serde_json::Value,
    observation: Option<&serde_json::Value>,
) -> Option<(serde_json::Value, &'static str)> {
    let indices = |key: &str| -> Vec<usize> {
        observation
            .and_then(|observation| observation.get(key))
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(serde_json::Value::as_u64)
                    .map(|index| index as usize)
                    .collect()
            })
            .unwrap_or_default()
    };
    if let Some(expect) = step.get("expect") {
        // Which locator actually failed is in the daemon's own observation.
        // Guessing "the first `present` one" describes a different failure
        // whenever a later locator is the one that did not match.
        let missing = indices("missing_present");
        if let Some(locator) = missing
            .first()
            .and_then(|index| expect.get("present")?.as_array()?.get(*index))
        {
            return Some((locator.clone(), "present"));
        }
        let violated = indices("violated_absent");
        if let Some(locator) = violated
            .first()
            .and_then(|index| expect.get("absent")?.as_array()?.get(*index))
        {
            return Some((locator.clone(), "absent"));
        }
        // No usable observation (the screen was never read): fall back to the
        // first `present` locator, which is at least what the step asked for.
        if let Some(locator) = expect
            .get("present")
            .and_then(serde_json::Value::as_array)
            .and_then(|present| present.first())
        {
            return Some((locator.clone(), "present"));
        }
        return None;
    }
    let action = step.get("action").unwrap_or(step);
    if let Some(locator) = action.get("locator") {
        return Some((locator.clone(), "locator"));
    }
    // A label-addressed tap is a one-field locator.
    action
        .get("label")
        .map(|label| (serde_json::json!({ "label": label }), "locator"))
}

/// Container/decoration kinds — the same set the daemon calls `container_only`.
const CONTAINER_KINDS: [&str; 4] = ["Application", "Window", "Other", "Image"];

/// Rank a candidate by WHICH locator fields matched, not how many. Identity
/// (`identifier`) outranks a human label, which outranks incidental booleans.
fn locator_weight(matched: &[String]) -> u32 {
    matched
        .iter()
        .map(|field| match field.as_str() {
            "identifier" => 100,
            "label" => 40,
            "kind" => 10,
            _ => 1,
        })
        .sum()
}

/// Score one element row against a locator: which fields matched, which
/// differed. The basis is reported as FIELD NAMES, so a reader can see why a
/// candidate is a candidate without the daemon having to explain itself.
fn candidate_basis(
    row: &serde_json::Value,
    locator: &serde_json::Value,
) -> (Vec<String>, Vec<String>) {
    let (mut matched, mut differed) = (Vec::new(), Vec::new());
    let Some(fields) = locator.as_object() else {
        return (matched, differed);
    };
    for (key, expected) in fields {
        if !LOCATOR_FIELDS.contains(&key.as_str()) {
            continue;
        }
        // The element tree is sparse: a row omits `enabled` / `visible` when
        // they are true and `focused` when it is false. The daemon's matcher
        // applies exactly those defaults, so comparing raw presence here would
        // report a field as `differed` that the daemon considers a MATCH —
        // a diagnosis that disagrees with the thing it is diagnosing.
        let actual = match row.get(key) {
            Some(value) => Some(value.clone()),
            None => match key.as_str() {
                "enabled" | "visible" => Some(serde_json::Value::Bool(true)),
                "focused" => Some(serde_json::Value::Bool(false)),
                _ => None,
            },
        };
        match actual {
            Some(actual) if actual == *expected => matched.push(key.clone()),
            _ => differed.push(key.clone()),
        }
    }
    (matched, differed)
}

/// Look at the screen ONCE after a failed run and say what is there.
///
/// Three rules this obeys, because a diagnosis that breaks them is worse than
/// no diagnosis at all:
///
/// * It never re-sends anything. It reads the element tree; that is all.
/// * It never changes the run's result. A diagnosis that fails is reported as
///   `diagnosis.error`; `outcome`, `applied_actions` and `retry_safe` are
///   already decided and are not touched.
/// * It never silently substitutes a locator. Candidates are reported with the
///   basis on which they were picked, for a human or agent to judge — the flow
///   itself is not edited, and nothing is retried against a candidate.
async fn diagnose_failure(
    ran_steps: Option<&serde_json::Value>,
    result: &serde_json::Value,
    daemon: &DaemonClient,
) -> Option<serde_json::Value> {
    if result.get("ok").and_then(serde_json::Value::as_bool) != Some(false) {
        return None;
    }
    let failed_step = result.get("failed_step").and_then(serde_json::Value::as_u64)?;
    let step = ran_steps
        .and_then(serde_json::Value::as_array)
        .and_then(|steps| steps.get(failed_step as usize))?;
    let mut diagnosis = serde_json::json!({
        // The daemon's own 0-based index, carried through unchanged.
        "failed_step": failed_step,
        "budget_ms": DIAGNOSIS_BUDGET.as_millis() as u64,
    });
    if let Some(kind) = step.get("kind").and_then(serde_json::Value::as_str) {
        diagnosis["step_kind"] = serde_json::json!(kind);
    }

    let observation = result.get("observation");
    let Some((locator, expectation)) = failed_step_locator(step, observation) else {
        diagnosis["observable"] = serde_json::json!(false);
        diagnosis["reason"] = serde_json::json!("step_has_no_locator");
        return Some(diagnosis);
    };
    diagnosis["expectation"] = serde_json::json!(expectation);

    let elements = match tokio::time::timeout(DIAGNOSIS_BUDGET, daemon.elements()).await {
        Err(_) => {
            diagnosis["observable"] = serde_json::json!(false);
            diagnosis["reason"] = serde_json::json!("diagnosis_timeout");
            return Some(diagnosis);
        }
        Ok(Err(error)) => {
            diagnosis["observable"] = serde_json::json!(false);
            diagnosis["reason"] = serde_json::json!("screen_unreadable");
            diagnosis["error"] = serde_json::json!(format!("{error:#}"));
            return Some(diagnosis);
        }
        Ok(Ok(body)) => body,
    };
    let tree = serde_json::from_str::<serde_json::Value>(&elements).ok();
    // A tree we could not parse, that carries no `elements`, or that holds
    // nothing to act on is NOT an observation. Saying `observable: true` here
    // would be the same mistake as letting an empty tree prove absence.
    let rows = match tree.as_ref().and_then(|tree| tree.get("elements")) {
        Some(serde_json::Value::Array(rows)) if !rows.is_empty() => rows.clone(),
        _ => {
            diagnosis["observable"] = serde_json::json!(false);
            diagnosis["reason"] = serde_json::json!("no_readable_tree");
            return Some(diagnosis);
        }
    };
    if rows.iter().all(|row| {
        row.get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| CONTAINER_KINDS.contains(&kind))
    }) {
        diagnosis["observable"] = serde_json::json!(false);
        diagnosis["reason"] = serde_json::json!("no_readable_tree");
        diagnosis["rows"] = serde_json::json!(rows.len());
        return Some(diagnosis);
    }
    diagnosis["observable"] = serde_json::json!(true);
    diagnosis["rows"] = serde_json::json!(rows.len());

    let mut scored: Vec<(u32, serde_json::Value)> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let (matched, differed) = candidate_basis(row, &locator);
            // Nothing in common is not a candidate, it is noise.
            (!matched.is_empty()).then(|| {
                (
                    locator_weight(&matched),
                    serde_json::json!({
                        "index": row.get("index").cloned().unwrap_or(serde_json::json!(index)),
                        "matched": matched,
                        "differed": differed,
                        // Screen text, for THIS authenticated response only.
                        // It is not written to disk and not published: the
                        // evidence file and any issue carry structure alone.
                        "label": row.get("label").cloned().unwrap_or(serde_json::Value::Null),
                        "kind": row.get("kind").cloned().unwrap_or(serde_json::Value::Null),
                    }),
                )
            })
        })
        .collect();
    // Identity beats resemblance: an `identifier` match outranks a `label`
    // match, which outranks a pile of matching booleans.
    scored.sort_by_key(|(weight, _)| std::cmp::Reverse(*weight));
    let exact = scored
        .iter()
        .filter(|(_, candidate)| {
            candidate["differed"]
                .as_array()
                .is_some_and(|differed| differed.is_empty())
        })
        .count();
    diagnosis["reason"] = serde_json::json!(match (expectation, exact, scored.len()) {
        // The element the flow wanted GONE is still here.
        ("absent", _, _) if exact >= 1 => "still_present",
        // The locator resolves NOW but did not during the run: a timing
        // problem, not a wrong selector.
        (_, 1, _) => "locator_matches_now",
        (_, 0, 0) => "no_similar_element",
        (_, 0, _) => "locator_no_match",
        _ => "locator_ambiguous",
    });
    diagnosis["exact_matches"] = serde_json::json!(exact);
    diagnosis["candidates"] = serde_json::Value::Array(
        scored
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|(_, candidate)| candidate)
            .collect(),
    );
    diagnosis["hint"] = serde_json::json!(
        "candidates are reported for review only; the flow was not edited and nothing was retried"
    );
    Some(diagnosis)
}

/// One executed flow, already turned into the value every entry point reports.
pub struct DiagnosedRun {
    /// The daemon's result body, plus `http_status` and, on failure, a
    /// `diagnosis` block.
    pub value: serde_json::Value,
    pub succeeded: bool,
    /// `None` when no answer arrived at all.
    pub status: Option<u16>,
    pub daemon_version: Option<String>,
    /// The pre-flight `/agent/status` body, so a caller that wants device
    /// context after a failure does not have to ask again.
    pub preflight_status: serde_json::Value,
}

/// Execute a flow and, if it failed, look once at the screen to say why.
///
/// Shared by every entry point on purpose: the CLI and the MCP tool are the
/// same job, and a diagnosis that only one of them can see is a diagnosis the
/// agent doing the work never reads.
pub async fn execute_and_diagnose(
    flow: &ValidatedFlow,
    inputs: &BTreeMap<String, String>,
    daemon: &DaemonClient,
    confirm: bool,
) -> Result<DiagnosedRun> {
    let run = execute_flow_run(flow, inputs, daemon, confirm).await?;
    // An answer we could not read and an answer that never arrived are the
    // same thing to a caller: the request went out and we do not know what the
    // phone did. Both are reported as an unknown outcome that is not safe to
    // retry, with no invented step and no invented count.
    let unknown = |reason: &str, detail: serde_json::Value| {
        serde_json::json!({
            "ok": false,
            "error": "outcome_unknown",
            "outcome": "unknown",
            "retry_safe": false,
            "reason": reason,
            "detail": detail,
        })
    };
    let (status, succeeded, mut value) = match &run.outcome {
        RunAnswer::Answered(response) => {
            let status = response.status.as_u16();
            // Success requires POSITIVE evidence, not merely a 2xx: the daemon
            // answers `200 {"ok":false}` for refusals it wants read, and a 2xx
            // whose body we could not parse proves the request was accepted,
            // not that the phone did anything.
            // Only two answers count as the daemon having spoken: an explicit
            // confirmation, and an explicit refusal. A body that merely parses
            // — `{}` , or `{"unrelated":1}` next to a 500 — told us nothing,
            // and reading it as a verdict would invent one.
            let value = if response.confirms_action() || response.explicit_refusal() {
                response
                    .json
                    .clone()
                    .expect("a confirmation or refusal has a body")
            } else {
                unknown(
                    if response.too_large {
                        "response_too_large"
                    } else if response.json.is_none() {
                        "unparseable_response"
                    } else {
                        "no_verdict_in_response"
                    },
                    serde_json::json!(response.preview()),
                )
            };
            (Some(status), response.confirms_action(), value)
        }
        RunAnswer::Unknown { reason, detail } => {
            (None, false, unknown(reason, serde_json::json!(detail)))
        }
    };
    if let (Some(object), Some(status)) = (value.as_object_mut(), status) {
        object.insert("http_status".into(), serde_json::json!(status));
    }
    // Reporting only: the run already happened and its verdict is fixed.
    let ran_steps = run.request.get("steps").cloned();
    if let Some(diagnosis) = diagnose_failure(ran_steps.as_ref(), &value, daemon).await {
        if let Some(object) = value.as_object_mut() {
            object.insert("diagnosis".into(), diagnosis);
        }
    }
    Ok(DiagnosedRun {
        value,
        succeeded,
        status,
        daemon_version: run.daemon_version,
        preflight_status: run.preflight_status,
    })
}

/// `flow run <file|id> [--input K=V]... [--confirm]`.
pub async fn run_command(
    target: &str,
    assignments: &[String],
    confirm: bool,
    force: bool,
    artifacts_dir: Option<&Path>,
) -> Result<()> {
    let path = registry::resolve_target(target)?;
    // Hash the EXACT bytes this run parses. An index entry describes what was
    // downloaded once; it does not prove what is on disk now, and a local file
    // run directly has no index entry at all.
    let bytes = read_flow_bytes(&path)?;
    let sha256 = registry::sha256_hex(&bytes);
    let flow = parse_flow(&bytes, &path.display().to_string())?;
    let inputs = parse_input_assignments(assignments, &flow.inputs)?;
    let daemon = DaemonClient::from_env();
    // One lookup, reused for the evidence record: asking twice would be a
    // second round trip purely to write down what we already knew.
    let (report, installed) = compat_gate(&flow, &daemon, force).await?;
    // Prepared BEFORE anything is sent: an unusable evidence directory is
    // worth failing on while the phone has not moved yet.
    let artifacts = artifacts_dir.map(ArtifactsDir::prepare).transpose()?;

    let started_at = unix_seconds();
    let clock = std::time::Instant::now();
    let run = execute_and_diagnose(&flow, &inputs, &daemon, confirm).await?;
    let duration_ms = clock.elapsed().as_millis() as u64;
    let (succeeded, status) = (run.succeeded, run.status);
    let mut value = run.value;

    if let Some(artifacts) = &artifacts {
        let evidence = build_evidence(
            target,
            &flow,
            Some(&sha256),
            &report,
            installed.as_ref(),
            run.daemon_version.as_deref(),
            &value,
            started_at,
            duration_ms,
        );
        let written = artifacts.write(&evidence, "result");
        attach_artifact(&mut value, written);
    }
    if let Some(object) = value.as_object_mut() {
        object.insert("compat".into(), serde_json::to_value(&report)?);
        if report.compat == crate::compat::Compat::UntestedNewer {
            object.insert(
                "hint".into(),
                serde_json::json!(
                    "this flow ran on an app version newer than its last verification; if the phone ended where the flow promised, publish an updated verified_on (flow publish / phone_flow_publish)"
                ),
            );
        }
    }
    println!("{value}");
    if !succeeded {
        // The machine-readable result is on stdout; the exit code still says
        // the run failed. Recording evidence never turns a failure into a
        // success, and an `artifact_error` never turns a success into failure.
        bail!(
            "flow {:?} did not succeed ({}); the full result is on stdout — never replay \
             automatically",
            flow.name(),
            match status {
                Some(code) => format!("HTTP {code}"),
                // No answer came back. The request went out, so this is an
                // unknown outcome — not "it never ran".
                None => "no answer from the daemon; outcome unknown".to_string(),
            }
        );
    }
    Ok(())
}

/// Compute the compat verdict and refuse `broken` / `incompatible` flows
/// unless forced. Never contacts the phone itself.
pub async fn compat_gate(
    flow: &ValidatedFlow,
    daemon: &DaemonClient,
    force: bool,
) -> Result<(crate::compat::CompatReport, Option<crate::compat::InstalledApps>)> {
    let installed = crate::compat::installed_apps(daemon).await;
    let report = crate::compat::compat_for(&flow.meta, installed.as_ref());
    if report.compat.blocks_run() && !force {
        bail!(
            "flow {:?} is {} ({}); no action was sent. Explore the app by hand and publish a fix, \
             or pass --force / force=true to run it anyway",
            flow.name(),
            report.compat.as_str(),
            report.reason
        );
    }
    Ok((report, installed))
}

/// Execute one validated flow exactly once. `confirm` is the explicit
/// acknowledgement a `risk:"side_effect"` flow requires; without it nothing is
/// sent. Unverified flows still run (that is how they get verified) but the
/// caller is expected to surface `verified:false` from the summary.
/// Whether this error proves the request never left this machine.
///
/// Only a connect failure does. A timeout or a broken read happens after the
/// bytes went out, and the phone may well have acted on them.
fn transport_never_sent(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_connect)
    })
}

/// One executed flow: the daemon's answer, plus what we learned on the way in.
/// What came back from a dispatched batch — or the fact that nothing did.
///
/// A request that was written to the socket and then lost its answer is NOT
/// the same as a request that never left. The first is an unknown outcome the
/// caller must not retry; the second is genuinely "nothing was sent". Only the
/// connect phase can honestly claim the latter.
#[derive(Debug)]
pub enum RunAnswer {
    Answered(crate::client::DaemonResponse),
    /// The request went out; the answer did not come back.
    Unknown {
        reason: &'static str,
        detail: String,
    },
}

#[derive(Debug)]
pub struct FlowRun {
    pub outcome: RunAnswer,
    /// The pre-flight status body, kept so nothing has to re-ask for it.
    pub preflight_status: serde_json::Value,
    /// The exact request body the daemon ran, inputs already substituted.
    /// Diagnosis reads the steps from here, never from the templates: a
    /// parameterized template still holds a placeholder, and looking for a
    /// placeholder on screen finds nothing.
    pub request: serde_json::Value,
    /// Daemon version from the pre-flight status — already fetched, so no
    /// second round trip after the run just to record a version.
    pub daemon_version: Option<String>,
}

/// Execute one validated flow exactly once and keep the daemon's answer whole,
/// including a FAILURE answer.
///
/// A failed flow is a non-2xx status with a complete JSON body — `failed_step`,
/// `outcome`, `applied_actions`, `retry_safe`, per-step results. That body is
/// the whole point of running a flow that then fails, so it is carried back
/// rather than collapsed into an error string.
pub async fn execute_flow_run(
    flow: &ValidatedFlow,
    inputs: &BTreeMap<String, String>,
    daemon: &DaemonClient,
    confirm: bool,
) -> Result<FlowRun> {
    if flow.meta.risk == Some(FlowRisk::SideEffect) && !confirm {
        bail!(
            "flow {:?} is declared risk=side_effect (sends, publishes, pays, or deletes); \
             re-run with --confirm (CLI) or confirm=true (MCP) after checking the target and \
             inputs. No action was sent",
            flow.name()
        );
    }
    let steps = materialize_steps(&flow.step_templates, &flow.inputs, Some(inputs))?;
    let request = phone_steps_request(steps).map_err(anyhow::Error::msg)?;
    let status_body = daemon
        .get_text("/agent/status")
        .await
        .context("preflight GET /agent/status before flow execution")?;
    let status: serde_json::Value =
        serde_json::from_str(&status_body).context("parse /agent/status")?;
    if status.get("backend").and_then(serde_json::Value::as_str) != Some("direct") {
        bail!(
            "flow execution requires backend=direct; daemon reported {:?}",
            status.get("backend")
        );
    }
    if status.get("drivable").and_then(serde_json::Value::as_bool) != Some(true) {
        bail!(
            "phone is not drivable; no flow action was sent (state={:?}, hint={:?})",
            status.get("device_state"),
            status.get("hint")
        );
    }
    let daemon_version = status
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let outcome = match daemon.actions_outcome(&request).await {
        Ok(response) => RunAnswer::Answered(response),
        // A connection that never opened means nothing reached the phone, and
        // saying so is safe. Anything later — the request was already on the
        // wire — leaves us not knowing what the phone did.
        Err(error) if transport_never_sent(&error) => {
            return Err(error).with_context(|| {
                format!(
                    "flow {:?} was not sent: the daemon could not be reached",
                    flow.name()
                )
            });
        }
        Err(error) => RunAnswer::Unknown {
            reason: "transport_error",
            detail: format!("{error:#}"),
        },
    };
    Ok(FlowRun {
        outcome,
        request,
        daemon_version,
        preflight_status: status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    fn valid_flow() -> &'static str {
        r#"{
          "version": 1,
          "name": "Open search",
          "description": "A deterministic read-only navigation example.",
          "steps": [
            {"kind":"shortcut","name":"home"},
            {"kind":"pause","ms":250},
            {"kind":"shortcut","name":"spotlight"},
            {"kind":"wait_for","expect":{"present":[{"kind":"TextField"}]},"timeout_ms":2000,"poll_ms":100}
          ]
        }"#
    }

    fn parameterized_flow() -> &'static str {
        r#"{
          "version": 1,
          "name": "Search",
          "inputs": {
            "query": {
              "type": "string",
              "description": "Search words for this run."
            }
          },
          "steps": [
            {"kind":"shortcut","name":"spotlight"},
            {"kind":"type","input":"query","clear":true},
            {"kind":"key","name":"return"}
          ]
        }"#
    }

    fn mock_daemon_sequence(
        responses: &[(&str, &str)],
    ) -> (String, JoinHandle<()>, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let responses = responses
            .iter()
            .map(|(status, body)| (status.to_string(), body.to_string()))
            .collect::<Vec<_>>();
        let (request_tx, request_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 8_192];
                let bytes = stream.read(&mut request).unwrap();
                request_tx
                    .send(String::from_utf8_lossy(&request[..bytes]).to_string())
                    .unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}"), task, request_rx)
    }

    #[test]
    fn validates_a_versioned_flow_offline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        assert_eq!(flow.name(), "Open search");
        assert_eq!(flow.meta.steps, 4);
        let steps = materialize_steps(&flow.step_templates, &flow.inputs, None).unwrap();
        let request = phone_steps_request(steps).unwrap();
        assert_eq!(request["steps"][0]["action"]["type"], "shortcut");
        assert_eq!(request["steps"][3]["kind"], "wait_for");
    }

    #[test]
    fn validates_parameterized_text_without_persisting_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parameterized.json");
        fs::write(&path, parameterized_flow()).unwrap();

        let flow = load_flow(&path).unwrap();
        assert_eq!(
            flow.inputs.keys().cloned().collect::<Vec<_>>(),
            vec!["query"]
        );
        let validation_steps = materialize_steps(&flow.step_templates, &flow.inputs, None).unwrap();
        let validation_request = phone_steps_request(validation_steps).unwrap();
        assert_eq!(validation_request["steps"][1]["action"]["type"], "text");
        assert_eq!(validation_request["steps"][1]["action"]["text"], "");

        let inputs =
            parse_input_assignments(&["query=咖啡=东京".to_string()], &flow.inputs).unwrap();
        let steps = materialize_steps(&flow.step_templates, &flow.inputs, Some(&inputs)).unwrap();
        let request = phone_steps_request(steps).unwrap();
        assert_eq!(request["steps"][1]["action"]["text"], "咖啡=东京");
        assert_eq!(request["steps"][1]["action"]["clear"], true);
    }

    #[test]
    fn rejects_undefined_unused_unknown_and_duplicate_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let undefined = dir.path().join("undefined.json");
        fs::write(
            &undefined,
            r#"{
              "version":1,
              "name":"x",
              "steps":[{"kind":"type","input":"query"}]
            }"#,
        )
        .unwrap();
        assert!(load_flow(&undefined)
            .unwrap_err()
            .to_string()
            .contains("undefined flow input"));

        let unused = dir.path().join("unused.json");
        fs::write(
            &unused,
            r#"{
              "version":1,
              "name":"x",
              "inputs":{"query":{"type":"string"}},
              "steps":[{"kind":"pause","ms":1}]
            }"#,
        )
        .unwrap();
        assert!(load_flow(&unused)
            .unwrap_err()
            .to_string()
            .contains("unused inputs"));

        let parameterized = dir.path().join("parameterized.json");
        fs::write(&parameterized, parameterized_flow()).unwrap();
        let flow = load_flow(&parameterized).unwrap();
        assert!(
            parse_input_assignments(&["other=x".to_string()], &flow.inputs)
                .unwrap_err()
                .to_string()
                .contains("unknown flow input")
        );
        assert!(parse_input_assignments(
            &["query=x".to_string(), "query=y".to_string()],
            &flow.inputs
        )
        .unwrap_err()
        .to_string()
        .contains("provided more than once"));
    }

    #[test]
    fn rejects_unknown_fields_and_future_versions() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = dir.path().join("unknown.json");
        fs::write(
            &unknown,
            r#"{"version":1,"name":"x","steps":[{"kind":"pause","ms":1}],"retry":true}"#,
        )
        .unwrap();
        assert!(load_flow(&unknown)
            .unwrap_err()
            .to_string()
            .contains("parse flow JSON"));

        let future = dir.path().join("future.json");
        fs::write(
            &future,
            r#"{"version":2,"name":"x","steps":[{"kind":"pause","ms":1}]}"#,
        )
        .unwrap();
        assert!(load_flow(&future)
            .unwrap_err()
            .to_string()
            .contains("unsupported flow version"));
    }

    #[test]
    fn rejects_a_symlinked_flow_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        let link = dir.path().join("flow.json");
        fs::write(&target, valid_flow()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(load_flow(&link)
            .unwrap_err()
            .to_string()
            .contains("without following symlinks"));
    }

    #[test]
    fn rejects_a_group_or_world_writable_flow_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

        assert!(load_flow(&path)
            .unwrap_err()
            .to_string()
            .contains("must not be group- or world-writable"));
    }

    #[tokio::test]
    async fn run_preflight_sends_no_actions_when_phone_is_not_drivable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        let status = r#"{
          "ok":true,
          "backend":"direct",
          "drivable":false,
          "device_state":"locked",
          "hint":"unlock the phone"
        }"#;
        let (url, task, requests) = mock_daemon_sequence(&[("200 OK", status)]);

        let error = execute_flow_run(
            &flow,
            &BTreeMap::new(),
            &DaemonClient::new(url, None),
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        task.join().unwrap();

        assert!(error.contains("not drivable"));
        assert!(error.contains("no flow action was sent"));
        assert!(requests.recv().unwrap().starts_with("GET /agent/status "));
        assert!(requests.try_recv().is_err());
    }

    // -----------------------------------------------------------------
    // Failure diagnosis
    // -----------------------------------------------------------------

    fn failing_result(step: u64) -> serde_json::Value {
        serde_json::json!({
            "ok": false,
            "error": "expectation_timeout",
            "failed_step": step,
            "outcome": "not_sent",
            "failed_step_outcome": "not_sent",
            "batch_outcome": "partially_applied",
            "applied_actions": 2,
            "retry_safe": true,
            "observation": {"read": true, "sparse": false, "missing_present": [0]}
        })
    }

    /// The steps as the daemon received them, inputs already substituted.
    fn ran_steps(flow: &ValidatedFlow, inputs: &BTreeMap<String, String>) -> serde_json::Value {
        let steps = materialize_steps(&flow.step_templates, &flow.inputs, Some(inputs)).unwrap();
        phone_steps_request(steps).unwrap()["steps"].clone()
    }

    /// The element tree is sparse: a row omits `enabled`/`visible` when they
    /// are true and `focused` when it is false. The daemon's matcher applies
    /// those defaults, so a diagnosis that compares raw presence would call a
    /// field `differed` on a row the daemon considers a MATCH — and send a
    /// reader hunting for a difference that does not exist.
    #[test]
    fn absent_boolean_fields_match_the_daemons_defaults() {
        // A row as WDA actually serialises it: nothing but identity.
        let sparse = serde_json::json!({"index": 0, "kind": "Button", "label": "搜索"});

        let (matched, differed) = candidate_basis(
            &sparse,
            &serde_json::json!({"kind": "Button", "enabled": true, "visible": true}),
        );
        assert_eq!(differed, Vec::<String>::new(), "matched={matched:?}");
        assert!(matched.contains(&"enabled".to_string()), "{matched:?}");
        assert!(matched.contains(&"visible".to_string()), "{matched:?}");

        let (matched, differed) = candidate_basis(
            &sparse,
            &serde_json::json!({"kind": "Button", "focused": false}),
        );
        assert_eq!(differed, Vec::<String>::new(), "matched={matched:?}");
        assert!(matched.contains(&"focused".to_string()), "{matched:?}");
    }

    /// The defaults are defaults, not wildcards: asking for the opposite of
    /// what an omitted field means must still be a difference.
    #[test]
    fn asking_for_the_opposite_of_a_default_is_a_difference() {
        let sparse = serde_json::json!({"index": 0, "kind": "Button", "label": "搜索"});

        for (field, wanted) in [("enabled", false), ("visible", false), ("focused", true)] {
            let (matched, differed) = candidate_basis(
                &sparse,
                &serde_json::json!({"kind": "Button", field: wanted}),
            );
            assert_eq!(
                differed,
                vec![field.to_string()],
                "{field}={wanted} must differ from the default; matched={matched:?}"
            );
        }
    }

    /// An explicit value on the row still wins over the default.
    #[test]
    fn an_explicit_field_is_compared_as_given() {
        let row = serde_json::json!({"kind": "Button", "enabled": false, "focused": true});

        let (matched, _) = candidate_basis(
            &row,
            &serde_json::json!({"enabled": false, "focused": true}),
        );
        assert_eq!(matched, vec!["enabled".to_string(), "focused".to_string()]);

        let (_, differed) =
            candidate_basis(&row, &serde_json::json!({"enabled": true, "focused": false}));
        assert_eq!(differed, vec!["enabled".to_string(), "focused".to_string()]);
    }

    #[tokio::test]
    async fn diagnosis_reports_candidates_with_the_basis_it_picked_them_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        // Step 3 waits for a TextField. The screen has one, plus a button that
        // shares nothing with the locator.
        let elements = r#"{"ok":true,"snapshot":"S","elements":[
            {"index":0,"kind":"Button","label":"取消"},
            {"index":1,"kind":"TextField","label":"搜索"}
        ]}"#;
        let (url, task, requests) = mock_daemon_sequence(&[("200 OK", elements)]);

        let steps = ran_steps(&flow, &BTreeMap::new());
        let diagnosis = diagnose_failure(
            Some(&steps),
            &failing_result(3),
            &DaemonClient::new(url, None),
        )
        .await
        .expect("a failed run is diagnosed");
        task.join().unwrap();

        assert_eq!(diagnosis["failed_step"], 3, "the daemon's 0-based index is carried through");
        assert_eq!(diagnosis["step_kind"], "wait_for");
        assert_eq!(diagnosis["observable"], true);
        assert_eq!(diagnosis["rows"], 2);
        // The locator resolves NOW: a timing problem, not a wrong selector.
        assert_eq!(diagnosis["reason"], "locator_matches_now");
        assert_eq!(diagnosis["exact_matches"], 1);
        let candidates = diagnosis["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1, "the unrelated button is noise, not a candidate");
        assert_eq!(candidates[0]["index"], 1);
        assert_eq!(candidates[0]["matched"], serde_json::json!(["kind"]));
        assert_eq!(candidates[0]["differed"], serde_json::json!([]));

        // Exactly one read, and it is a read.
        let request = requests.recv().unwrap();
        assert!(request.starts_with("GET /agent/elements"), "{request}");
        assert!(requests.try_recv().is_err(), "diagnosis must not send anything else");
    }

    /// Diagnosis is a courtesy. When the screen cannot be read it says so, and
    /// the run's own result is not affected in any way.
    #[tokio::test]
    async fn an_unreadable_screen_is_reported_without_touching_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        let (url, task, _requests) =
            mock_daemon_sequence(&[("502 Bad Gateway", r#"{"error":"wda_source_failed"}"#)]);

        let result = failing_result(3);
        let steps = ran_steps(&flow, &BTreeMap::new());
        let diagnosis = diagnose_failure(Some(&steps), &result, &DaemonClient::new(url, None))
            .await
            .unwrap();
        task.join().unwrap();

        assert_eq!(diagnosis["observable"], false);
        assert_eq!(diagnosis["reason"], "screen_unreadable");
        // Untouched — including fields this diagnosis path knows nothing
        // about. Diagnosis annotates a failure; it never rewrites the daemon's
        // account of what happened.
        assert_eq!(result["outcome"], "not_sent");
        assert_eq!(result["failed_step_outcome"], "not_sent");
        assert_eq!(result["batch_outcome"], "partially_applied");
        assert_eq!(result["applied_actions"], 2);
        assert_eq!(result["retry_safe"], true);
    }

    #[tokio::test]
    async fn a_successful_run_is_not_diagnosed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        // No mock daemon at all: a passing run must not read the screen.
        let steps = ran_steps(&flow, &BTreeMap::new());
        let diagnosis = diagnose_failure(
            Some(&steps),
            &serde_json::json!({"ok": true, "completed": 4}),
            &DaemonClient::new("http://127.0.0.1:1".to_string(), None),
        )
        .await;
        assert!(diagnosis.is_none());
    }

    // -----------------------------------------------------------------
    // Run evidence
    // -----------------------------------------------------------------

    #[test]
    fn an_unusable_artifacts_directory_fails_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where a directory would have to be.
        let blocker = dir.path().join("not-a-dir");
        fs::write(&blocker, b"x").unwrap();
        let error = ArtifactsDir::prepare(&blocker.join("runs")).unwrap_err();
        assert!(
            error.to_string().contains("create artifacts directory"),
            "{error:#}"
        );
    }

    #[test]
    fn evidence_is_written_private_and_carries_structure_only() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let flow_dir = tempfile::tempdir().unwrap();
        let path = flow_dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();

        let artifacts = ArtifactsDir::prepare(&home.path().join("runs")).unwrap();
        // The per-run directory is private from the moment it exists.
        assert_eq!(
            fs::metadata(artifacts.dir()).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let compat = crate::compat::compat_for(&flow.meta, None);
        let result = serde_json::json!({
            "ok": false,
            "error": "expectation_timeout",
            "failed_step": 3,
            "outcome": "not_sent",
            "applied_actions": 2,
            "observation": {"read": true, "sparse": false, "application": "招商银行"},
            "steps": [{"index": 0, "kind": "action", "action": {"type": "text", "text": "私密内容"}}]
        });
        let evidence = build_evidence(
            "health/open",
            &flow,
            Some("abc123"),
            &compat,
            None,
            None,
            &result,
            1_700_000_000,
            12_345,
        );
        let written = artifacts.write(&evidence, "result").unwrap();

        assert_eq!(
            fs::metadata(&written).unwrap().permissions().mode() & 0o777,
            0o600,
            "run evidence must not be world-readable"
        );
        assert!(written.ends_with("result.json"), "{written:?}");

        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(&written).unwrap()).unwrap();
        assert_eq!(stored["schema"], EVIDENCE_SCHEMA);
        assert_eq!(stored["flow"]["sha256"], "abc123");
        assert_eq!(stored["flow"]["steps"], 4);
        // A version nobody could read is said to be unavailable, never guessed.
        assert_eq!(stored["versions"]["daemon"], "unavailable");
        assert_eq!(stored["versions"]["app"], "unavailable");
        assert_eq!(stored["run"]["started_at_unix"], 1_700_000_000_u64);
        assert_eq!(stored["run"]["duration_ms"], 12_345);
        assert_eq!(stored["versions"]["ios"], "unavailable");
        // Structure survives; screen text and typed input do not.
        assert_eq!(stored["result"]["error"], "expectation_timeout");
        assert_eq!(stored["result"]["observation"]["read"], true);
        let text = String::from_utf8(fs::read(&written).unwrap()).unwrap();
        assert!(!text.contains("招商银行"), "{text}");
        assert!(!text.contains("私密内容"), "{text}");
    }

    /// The phone has already acted by the time evidence is written. A write
    /// that fails must not rewrite that, and must not look like a failed run.
    #[test]
    fn a_write_failure_after_the_run_keeps_the_result_intact() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let artifacts = ArtifactsDir::prepare(&home.path().join("runs")).unwrap();
        let dir = artifacts.dir().to_path_buf();
        // The directory stops being writable AFTER the run — exactly the case
        // where the phone has already acted.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();

        let mut value = serde_json::json!({
            "ok": true,
            "completed": 4,
            "applied_actions": 2,
            "outcome": "applied",
            "retry_safe": false
        });
        let written = artifacts.write(&serde_json::json!({"schema": EVIDENCE_SCHEMA}), "result");
        // Restore before the assertions so the tempdir can clean itself up.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(written.is_err(), "the write must actually fail");
        attach_artifact(&mut value, written);

        assert!(value["artifact_error"].is_string(), "{value}");
        assert!(value["artifact"].is_null());
        // The run's own verdict is untouched.
        assert_eq!(value["ok"], true);
        assert_eq!(value["outcome"], "applied");
        assert_eq!(value["applied_actions"], 2);
        assert_eq!(value["retry_safe"], false);
    }

    /// Two runs in the same second are two runs. Neither may overwrite the
    /// other's evidence just because the names would collide.
    #[test]
    fn two_runs_in_the_same_second_keep_both_records() {
        let home = tempfile::tempdir().unwrap();
        let artifacts = ArtifactsDir::prepare(&home.path().join("runs")).unwrap();

        let first = artifacts.write(&serde_json::json!({"run": 1}), "result").unwrap();
        let second = artifacts.write(&serde_json::json!({"run": 2}), "result").unwrap();

        assert_ne!(first, second);
        let first: serde_json::Value =
            serde_json::from_slice(&fs::read(&first).unwrap()).unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&fs::read(&second).unwrap()).unwrap();
        assert_eq!(first["run"], 1, "the first record survived the second run");
        assert_eq!(second["run"], 2);
    }

    /// Nothing already sitting at the evidence path is touched — not a plain
    /// file, and not a symlink pointing somewhere else entirely.
    #[test]
    fn an_existing_file_or_symlink_at_the_target_is_never_written_through() {
        let home = tempfile::tempdir().unwrap();
        let artifacts = ArtifactsDir::prepare(&home.path().join("runs")).unwrap();
        let dir = artifacts.dir().to_path_buf();

        // Something precious, reachable through a symlink at the target name.
        let precious = home.path().join("precious.txt");
        fs::write(&precious, b"do not clobber").unwrap();
        std::os::unix::fs::symlink(&precious, dir.join("result.json")).unwrap();

        let written = artifacts
            .write(&serde_json::json!({"schema": EVIDENCE_SCHEMA}), "result")
            .unwrap();

        assert_ne!(written, dir.join("result.json"));
        assert_eq!(
            fs::read_to_string(&precious).unwrap(),
            "do not clobber",
            "the symlink target must be untouched"
        );
    }

    /// A directory the user already had is theirs; recording a run does not
    /// quietly tighten its permissions.
    #[test]
    fn an_existing_directory_keeps_its_own_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("mine");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        let artifacts = ArtifactsDir::prepare(&dir).unwrap();

        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755,
            "the base directory the user made is left as they made it"
        );
        assert_eq!(
            fs::metadata(artifacts.dir()).unwrap().permissions().mode() & 0o777,
            0o700,
            "but this run's own directory is private"
        );
    }

    /// A failure body survives as a RESULT, with everything a caller needs to
    /// decide what to do next. (The path that raised such a body as an error —
    /// and forced callers to reverse-parse JSON out of a message — is gone.)
    #[tokio::test]
    async fn a_failure_body_comes_back_as_a_result_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        let status = r#"{"ok":true,"backend":"direct","drivable":true}"#;
        let failure = r#"{"ok":false,"error":"expectation_timeout","failed_step":3,"applied_actions":2}"#;

        let (url, task, _requests) =
            mock_daemon_sequence(&[("200 OK", status), ("409 Conflict", failure)]);
        let run = execute_flow_run(&flow, &BTreeMap::new(), &DaemonClient::new(url, None), false)
            .await
            .expect("a failed flow is a result, not an error");
        task.join().unwrap();

        let RunAnswer::Answered(response) = run.outcome else {
            panic!("a 409 IS an answer");
        };
        assert!(!response.ok());
        assert!(response.explicit_refusal(), "the daemon said no, explicitly");
        assert_eq!(response.status.as_u16(), 409);
        let body = response.json.expect("the daemon's JSON body is kept");
        assert_eq!(body["failed_step"], 3);
        assert_eq!(body["applied_actions"], 2, "what the phone did is not lost");
    }

    /// The whole CLI path on the failure that matters    /// The whole CLI path on the failure that matters: the daemon answers 409
    /// with a complete body, and the run has already applied actions.
    ///
    /// Before the structured result path existed, `actions()` raised that 409
    /// as an error, `run_command` returned early, and the diagnosis and the
    /// evidence file never happened at all — on precisely the runs they exist
    /// for.
    #[tokio::test]
    async fn a_failing_run_still_reports_diagnoses_and_records() {
        let home = tempfile::tempdir().unwrap();
        let flow_path = home.path().join("flow.json");
        fs::write(&flow_path, valid_flow()).unwrap();
        let runs = home.path().join("runs");

        let status = r#"{"ok":true,"backend":"direct","drivable":true,"version":"0.6.4"}"#;
        // A real failure: two steps applied, the third expectation never met.
        let failure = r#"{"ok":false,"error":"expectation_timeout","failed_step":3,
            "completed":3,"applied_actions":2,"outcome":"not_sent","retry_safe":true,
            "observation":{"read":true,"sparse":false,"missing_present":[0]}}"#;
        let elements = r#"{"ok":true,"snapshot":"S","elements":[
            {"index":0,"kind":"TextField","label":"搜索"}
        ]}"#;
        let (url, task, requests) = mock_daemon_sequence(&[
            // compat lookup: no apps, and no udid to fall back on
            ("200 OK", "{}"),
            ("200 OK", r#"{"ok":true}"#),
            // the run itself
            ("200 OK", status),
            ("409 Conflict", failure),
            // the one diagnostic read
            ("200 OK", elements),
        ]);
        let store = tempfile::tempdir().unwrap();
        std::env::set_var("PHONE_REMOTE_URL", &url);
        std::env::set_var(registry::STORE_ENV, store.path());

        let error = run_command(
            flow_path.to_str().unwrap(),
            &[],
            false,
            true, // --force: compat is not what this test is about
            Some(&runs),
        )
        .await
        .expect_err("a failed flow must still exit non-zero");
        task.join().unwrap();
        std::env::remove_var("PHONE_REMOTE_URL");
        std::env::remove_var(registry::STORE_ENV);

        assert!(error.to_string().contains("did not succeed (HTTP 409)"), "{error:#}");

        // Exactly one mutation, and exactly one read after it.
        assert!(requests.recv().unwrap().starts_with("GET /agent/apps"));
        assert!(requests.recv().unwrap().starts_with("GET /agent/status"));
        assert!(requests.recv().unwrap().starts_with("GET /agent/status"));
        let action = requests.recv().unwrap();
        assert!(action.starts_with("POST /agent/actions"), "{action}");
        let read = requests.recv().unwrap();
        assert!(read.starts_with("GET /agent/elements"), "{read}");
        assert!(requests.try_recv().is_err(), "nothing may be re-sent");

        // The evidence file exists and kept the failure verbatim.
        let run_dirs: Vec<_> = fs::read_dir(&runs)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        assert_eq!(run_dirs.len(), 1, "{run_dirs:?}");
        let written: Vec<_> = fs::read_dir(&run_dirs[0])
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert_eq!(written.len(), 1, "{written:?}");
        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(&written[0]).unwrap()).unwrap();
        assert_eq!(stored["result"]["error"], "expectation_timeout");
        assert_eq!(stored["result"]["failed_step"], 3);
        assert_eq!(
            stored["result"]["applied_actions"], 2,
            "what the phone actually did must survive: {stored}"
        );
        assert_eq!(stored["result"]["outcome"], "not_sent");
        assert_eq!(stored["versions"]["daemon"], "0.6.4");
        // The hash is of the bytes this run parsed, not of an index entry.
        assert_eq!(
            stored["flow"]["sha256"],
            registry::sha256_hex(&fs::read(&flow_path).unwrap())
        );
        assert!(stored["run"]["duration_ms"].is_u64());
    }

    #[tokio::test]
    async fn run_posts_one_guarded_batch_after_a_drivable_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        let status = r#"{"ok":true,"backend":"direct","drivable":true}"#;
        let result = r#"{"ok":true,"completed":4,"applied_actions":2}"#;
        let (url, task, requests) = mock_daemon_sequence(&[("200 OK", status), ("200 OK", result)]);

        let body = execute_flow_run(
            &flow,
            &BTreeMap::new(),
            &DaemonClient::new(url, None),
            false,
        )
        .await
        .unwrap();
        task.join().unwrap();

        let RunAnswer::Answered(response) = body.outcome else {
            panic!("the daemon answered");
        };
        assert_eq!(response.body(), result);
        assert!(requests.recv().unwrap().starts_with("GET /agent/status "));
        let action = requests.recv().unwrap();
        assert!(action.starts_with("POST /agent/actions "));
        assert!(action.to_ascii_lowercase().contains("x-phone-control: 1"));
    }

    #[tokio::test]
    async fn missing_required_input_fails_before_contacting_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parameterized.json");
        fs::write(&path, parameterized_flow()).unwrap();
        let flow = load_flow(&path).unwrap();

        let error = execute_flow_run(
            &flow,
            &BTreeMap::new(),
            &DaemonClient::new("http://127.0.0.1:9".to_string(), None),
            false,
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("missing required flow input"));
        assert!(error.contains("no action was sent"));
    }
    #[test]
    fn accepts_and_validates_registry_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        fs::write(
            &path,
            r#"{
              "version":1,
              "name":"Export",
              "app":"com.apple.Health",
              "category":"health",
              "risk":"navigation",
              "locale":"en",
              "tags":["export","backup"],
              "verified_on":[{"device":"iPhone 15 Pro","ios":"26.0","date":"2026-09-05"}],
              "steps":[{"kind":"launch_app","bundle":"com.apple.Health"}]
            }"#,
        )
        .unwrap();
        let flow = load_flow(&path).unwrap();
        assert_eq!(flow.meta.app.as_deref(), Some("com.apple.Health"));
        assert_eq!(flow.meta.risk, Some(FlowRisk::Navigation));
        assert!(flow.meta.verified());
        let summary = flow_summary(&flow);
        assert_eq!(summary["risk"], "navigation");
        assert_eq!(summary["verified"], true);
        assert_eq!(summary["tags"][1], "backup");

        for (field, body) in [
            ("app", r#""app":"Health""#),
            ("category", r#""category":"Health""#),
            ("locale", r#""locale":"english""#),
            ("risk", r#""risk":"dangerous""#),
            ("tags", r#""tags":["a","a"]"#),
            ("verified_on", r#""verified_on":[{}]"#),
        ] {
            let bad = dir.path().join(format!("{field}.json"));
            fs::write(
                &bad,
                format!(r#"{{"version":1,"name":"x",{body},"steps":[{{"kind":"pause","ms":1}}]}}"#),
            )
            .unwrap();
            assert!(load_flow(&bad).is_err(), "{field} should be rejected");
        }
    }

    #[tokio::test]
    async fn side_effect_flows_need_confirm_before_any_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("send.json");
        fs::write(
            &path,
            r#"{"version":1,"name":"Send","risk":"side_effect","steps":[{"kind":"key","name":"return"}]}"#,
        )
        .unwrap();
        let flow = load_flow(&path).unwrap();
        let unreachable = DaemonClient::new("http://127.0.0.1:9".to_string(), None);
        let error = execute_flow_run(&flow, &BTreeMap::new(), &unreachable, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("side_effect"));
        assert!(error.contains("No action was sent"));

        let status = r#"{"ok":true,"backend":"direct","drivable":true}"#;
        let result = r#"{"ok":true,"completed":1,"applied_actions":1}"#;
        let (url, task, _requests) =
            mock_daemon_sequence(&[("200 OK", status), ("200 OK", result)]);
        let body = execute_flow_run(&flow, &BTreeMap::new(), &DaemonClient::new(url, None), true)
            .await
            .unwrap();
        task.join().unwrap();
        let RunAnswer::Answered(response) = body.outcome else {
            panic!("the daemon answered");
        };
        assert_eq!(response.body(), result);
    }
}
