//! Deterministic, file-backed flow validation and execution.
//!
//! The file format deliberately reuses the exact `PhoneStep` input accepted by
//! the MCP `phone_run_steps` tool. A saved flow is therefore a low-token replay
//! surface, not a second automation engine with subtly different semantics.

use crate::client::DaemonClient;
use crate::server::{phone_steps_request, PhoneStep};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const FLOW_VERSION: u32 = 1;
const MAX_FLOW_BYTES: u64 = 64 * 1024;
const MAX_NAME_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 1_000;
const MAX_INPUTS: usize = 16;
const MAX_INPUT_NAME_CHARS: usize = 64;
const MAX_INPUT_DESCRIPTION_CHARS: usize = 200;

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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlowInputDefinition {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_required")]
    required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Debug)]
struct ValidatedFlow {
    name: String,
    description: Option<String>,
    step_count: usize,
    inputs: BTreeMap<String, FlowInputDefinition>,
    step_templates: Vec<serde_json::Value>,
}

fn valid_input_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && name.chars().count() <= MAX_INPUT_NAME_CHARS
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
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

fn parse_input_assignments(
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

fn load_flow(path: &Path) -> Result<ValidatedFlow> {
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

    let document: FlowDocument = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse flow JSON: {}", path.display()))?;
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

    validate_input_definitions(&document.inputs)?;
    let step_count = document.steps.len();
    let validated_steps = materialize_steps(&document.steps, &document.inputs, None)?;
    phone_steps_request(validated_steps).map_err(anyhow::Error::msg)?;
    Ok(ValidatedFlow {
        name: name.to_string(),
        description: document.description,
        step_count,
        inputs: document.inputs,
        step_templates: document.steps,
    })
}

pub fn validate_command(path: &Path) -> Result<()> {
    let flow = load_flow(path)?;
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "version": FLOW_VERSION,
            "name": flow.name,
            "description": flow.description,
            "steps": flow.step_count,
            "inputs": flow.inputs.keys().collect::<Vec<_>>(),
            "network": "not_contacted"
        })
    );
    Ok(())
}

pub async fn run_command(path: &Path, assignments: &[String]) -> Result<()> {
    let flow = load_flow(path)?;
    let inputs = parse_input_assignments(assignments, &flow.inputs)?;
    let daemon = DaemonClient::from_env();
    let result = execute_flow(&flow, &inputs, &daemon).await?;
    println!("{result}");
    Ok(())
}

async fn execute_flow(
    flow: &ValidatedFlow,
    inputs: &BTreeMap<String, String>,
    daemon: &DaemonClient,
) -> Result<String> {
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
        .with_context(|| format!("run flow {:?}; never replay automatically", flow.name))?;
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
        assert_eq!(flow.name, "Open search");
        assert_eq!(flow.step_count, 4);
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

        let error = execute_flow(&flow, &BTreeMap::new(), &DaemonClient::new(url, None))
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

        let body = execute_flow(&flow, &BTreeMap::new(), &DaemonClient::new(url, None))
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
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("missing required flow input"));
        assert!(error.contains("no action was sent"));
    }
}
