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
}

impl FlowMeta {
    pub fn verified(&self) -> bool {
        !self.verified_on.is_empty()
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

/// `flow run <file|id> [--input K=V]... [--confirm]`.
pub async fn run_command(target: &str, assignments: &[String], confirm: bool) -> Result<()> {
    let path = registry::resolve_target(target)?;
    let flow = load_flow(&path)?;
    let inputs = parse_input_assignments(assignments, &flow.inputs)?;
    let daemon = DaemonClient::from_env();
    let result = execute_flow(&flow, &inputs, &daemon, confirm).await?;
    println!("{result}");
    Ok(())
}

/// Execute one validated flow exactly once. `confirm` is the explicit
/// acknowledgement a `risk:"side_effect"` flow requires; without it nothing is
/// sent. Unverified flows still run (that is how they get verified) but the
/// caller is expected to surface `verified:false` from the summary.
pub async fn execute_flow(
    flow: &ValidatedFlow,
    inputs: &BTreeMap<String, String>,
    daemon: &DaemonClient,
    confirm: bool,
) -> Result<String> {
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
    let status = daemon
        .status()
        .await
        .context("preflight GET /agent/status before flow execution")?;
    if status.backend.as_deref() != Some("direct") {
        bail!(
            "flow execution requires backend=direct; daemon reported {:?}",
            status.backend
        );
    }
    if status.drivable != Some(true) {
        bail!(
            "phone is not drivable; no flow action was sent (state={:?}, hint={:?})",
            status.device_state,
            status.hint
        );
    }

    let result = daemon
        .actions(&request)
        .await
        .with_context(|| format!("run flow {:?}; never replay automatically", flow.name()))?;
    Ok(result)
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

        let error = execute_flow(
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

    #[tokio::test]
    async fn run_posts_one_guarded_batch_after_a_drivable_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.json");
        fs::write(&path, valid_flow()).unwrap();
        let flow = load_flow(&path).unwrap();
        let status = r#"{"ok":true,"backend":"direct","drivable":true}"#;
        let result = r#"{"ok":true,"completed":4,"applied_actions":2}"#;
        let (url, task, requests) = mock_daemon_sequence(&[("200 OK", status), ("200 OK", result)]);

        let body = execute_flow(
            &flow,
            &BTreeMap::new(),
            &DaemonClient::new(url, None),
            false,
        )
        .await
        .unwrap();
        task.join().unwrap();

        assert_eq!(body, result);
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

        let error = execute_flow(
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
        let error = execute_flow(&flow, &BTreeMap::new(), &unreachable, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("side_effect"));
        assert!(error.contains("No action was sent"));

        let status = r#"{"ok":true,"backend":"direct","drivable":true}"#;
        let result = r#"{"ok":true,"completed":1,"applied_actions":1}"#;
        let (url, task, _requests) =
            mock_daemon_sequence(&[("200 OK", status), ("200 OK", result)]);
        let body = execute_flow(&flow, &BTreeMap::new(), &DaemonClient::new(url, None), true)
            .await
            .unwrap();
        task.join().unwrap();
        assert_eq!(body, result);
    }
}
