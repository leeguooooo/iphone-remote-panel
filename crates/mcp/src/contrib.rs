//! Contributing back to the official flow registry: `flow publish` opens a
//! pull request with a validated flow, `flow report` files an issue when an
//! installed flow breaks. Both shell out to the GitHub CLI (`gh`) so the
//! user's existing login and 2FA are reused and nothing new stores a token.
//!
//! Everything here is *outward-facing*: it creates a branch on the user's
//! fork, a PR, or an issue in a public repository. Callers (CLI flags, MCP
//! `confirm=true`) must have the user's go-ahead before invoking it; the
//! functions themselves never run without being asked.
//!
//! Overrides for tests and self-hosting:
//! * `IPHONE_USE_GH_BIN`         — path to a `gh`-compatible binary (default `gh`)
//! * `IPHONE_USE_FLOWS_REPO`     — `owner/name` (default `leeguooooo/iphone-use-flows`)
//! * `IPHONE_USE_FLOWS_REPO_URL` — clone URL; a local path works for offline tests

use crate::flow::{self, ValidatedFlow};
use crate::registry;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_REPO: &str = "leeguooooo/iphone-use-flows";
pub const REPO_ENV: &str = "IPHONE_USE_FLOWS_REPO";
pub const REPO_URL_ENV: &str = "IPHONE_USE_FLOWS_REPO_URL";
pub const GH_BIN_ENV: &str = "IPHONE_USE_GH_BIN";
pub const LABEL_BROKEN: &str = "flow-broken";
pub const LABEL_NEW_FLOW: &str = "new-flow";

fn repo_slug() -> String {
    std::env::var(REPO_ENV)
        .ok()
        .filter(|value| value.contains('/'))
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

fn repo_clone_url(slug: &str) -> String {
    std::env::var(REPO_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("https://github.com/{slug}.git"))
}

fn gh_bin() -> String {
    std::env::var(GH_BIN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "gh".to_string())
}

fn run(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command.output().with_context(|| {
        if program == gh_bin() {
            format!(
                "run `{program} {}` — is the GitHub CLI installed? (brew install gh && gh auth login)",
                args.join(" ")
            )
        } else {
            format!("run `{program} {}`", args.join(" "))
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "`{program} {}` failed ({}): {}{}",
            args.join(" "),
            output.status,
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" / {}", stdout.trim())
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn gh(args: &[&str]) -> Result<String> {
    run(&gh_bin(), args, None)
}

fn gh_login() -> Result<String> {
    let login = gh(&["api", "user", "--jq", ".login"])
        .context("GitHub CLI is not logged in; run `gh auth login` first")?;
    if login.is_empty() {
        bail!("`gh api user` returned an empty login; run `gh auth login`");
    }
    Ok(login)
}

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PublishOptions {
    /// Registry id to publish under (`<app>/<flow>`).
    pub id: String,
    /// Human app name for a new `app.json` (defaults to the app slug).
    pub app_name: Option<String>,
    /// Foreground-app labels (per locale) that should surface this app's flows
    /// from `phone_elements`, e.g. `Health`, `健康`.
    pub aliases: Vec<String>,
    /// Extra PR body text: what was verified, on which device/locale.
    pub note: Option<String>,
    /// Open the PR as a draft.
    pub draft: bool,
}

#[derive(Debug, Serialize)]
pub struct PublishReport {
    pub ok: bool,
    pub id: String,
    pub repo: String,
    pub branch: String,
    pub head: String,
    pub pr_url: String,
    pub new_app_json: bool,
}

/// Body of the pull request. Pure function so it is testable and so the CLI
/// and MCP paths cannot drift.
pub fn pr_body(id: &str, flow: &ValidatedFlow, note: Option<&str>) -> String {
    let meta = &flow.meta;
    let mut body = String::new();
    body.push_str(&format!("## `{id}` — {}\n\n", meta.name));
    if let Some(description) = &meta.description {
        body.push_str(description);
        body.push_str("\n\n");
    }
    body.push_str("| | |\n|---|---|\n");
    body.push_str(&format!(
        "| app | `{}` |\n",
        meta.app.as_deref().unwrap_or("—")
    ));
    body.push_str(&format!(
        "| category | {} |\n",
        meta.category.as_deref().unwrap_or("—")
    ));
    body.push_str(&format!("| risk | {} |\n", meta.risk_label()));
    body.push_str(&format!(
        "| locale | {} |\n",
        meta.locale.as_deref().unwrap_or("—")
    ));
    body.push_str(&format!("| steps | {} |\n", meta.steps));
    body.push_str(&format!(
        "| inputs | {} |\n",
        if meta.inputs.is_empty() {
            "—".to_string()
        } else {
            meta.inputs.join(", ")
        }
    ));
    body.push_str("\n### Verified on\n\n");
    if meta.verified_on.is_empty() {
        body.push_str("_Not yet run on hardware — submitted as a draft._\n");
    } else {
        for verification in &meta.verified_on {
            body.push_str(&format!(
                "- {} · iOS {} · app {} · {}\n",
                verification.device.as_deref().unwrap_or("?"),
                verification.ios.as_deref().unwrap_or("?"),
                verification.app_version.as_deref().unwrap_or("?"),
                verification.date.as_deref().unwrap_or("?")
            ));
        }
    }
    if let Some(note) = note.filter(|note| !note.trim().is_empty()) {
        body.push_str("\n### Notes\n\n");
        body.push_str(note.trim());
        body.push('\n');
    }
    body.push_str(
        "\n### Checklist\n\n\
         - [x] `iphone-use-mcp flow validate` passes\n\
         - [x] no literal typed text, credentials, or private content in the file\n\
         - [x] `risk` is honest (`side_effect` for send/publish/pay/delete)\n\
         - [x] `index.json` regenerated with `scripts/build-index.py`\n\n\
         _Opened with `iphone-use-mcp flow publish`._\n",
    );
    body
}

/// Fork if needed, branch, add the flow (+ `app.json` when new), rebuild the
/// index, push, and open a pull request. Returns the PR URL.
pub fn publish(source: &Path, options: &PublishOptions) -> Result<PublishReport> {
    let id = options.id.as_str();
    if !registry::valid_flow_id(id) {
        bail!("{id:?} is not a valid registry id; use <app>/<flow> lowercase slugs");
    }
    let bytes = flow::read_flow_bytes(source)?;
    let validated = flow::parse_flow(&bytes, &source.display().to_string())?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)?;
    let (app_slug, flow_slug) = id.split_once('/').expect("validated id");

    let slug = repo_slug();
    let (owner, _) = slug.split_once('/').expect("repo slug has an owner");
    let login = gh_login()?;
    let head_owner = if login == owner {
        login.clone()
    } else {
        // Idempotent: gh prints a notice when the fork already exists.
        gh(&["repo", "fork", &slug, "--clone=false"])
            .with_context(|| format!("fork {slug} into {login}'s account"))?;
        login.clone()
    };

    let workdir = tempfile::Builder::new()
        .prefix("iphone-use-flow-publish-")
        .tempdir()
        .context("create temporary clone directory")?;
    let clone_dir = workdir.path().join("repo");
    run(
        "git",
        &[
            "clone",
            "--quiet",
            "--depth",
            "1",
            &repo_clone_url(&slug),
            clone_dir.to_str().unwrap(),
        ],
        None,
    )?;
    let branch = format!("flow/{app_slug}-{flow_slug}");
    run(
        "git",
        &["checkout", "--quiet", "-b", &branch],
        Some(&clone_dir),
    )?;

    let app_dir = clone_dir.join(app_slug);
    std::fs::create_dir_all(&app_dir)?;
    let app_json = app_dir.join("app.json");
    let new_app_json = !app_json.exists();
    if new_app_json {
        let mut app = serde_json::json!({
            "id": app_slug,
            "name": options.app_name.clone().unwrap_or_else(|| app_slug.to_string()),
        });
        if let Some(bundle) = &validated.meta.app {
            app["bundle"] = serde_json::json!(bundle);
        }
        if let Some(category) = &validated.meta.category {
            app["category"] = serde_json::json!(category);
        }
        if !options.aliases.is_empty() {
            app["aliases"] = serde_json::json!(options.aliases);
        }
        std::fs::write(&app_json, serde_json::to_string_pretty(&app)? + "\n")?;
    } else if !options.aliases.is_empty() {
        let mut app: serde_json::Value = serde_json::from_slice(&std::fs::read(&app_json)?)?;
        let mut aliases: Vec<String> = app
            .get("aliases")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        for alias in &options.aliases {
            if !aliases.contains(alias) {
                aliases.push(alias.clone());
            }
        }
        app["aliases"] = serde_json::json!(aliases);
        std::fs::write(&app_json, serde_json::to_string_pretty(&app)? + "\n")?;
    }
    let target = app_dir.join(format!("{flow_slug}.json"));
    if target.exists() {
        bail!(
            "{id} already exists in {slug}; bump `verified_on` and describe the fix in the PR, or pick another id"
        );
    }
    std::fs::write(&target, serde_json::to_string_pretty(&document)? + "\n")?;

    let index_script = clone_dir.join("scripts").join("build-index.py");
    if index_script.is_file() {
        run(
            "python3",
            &[index_script.to_str().unwrap()],
            Some(&clone_dir),
        )
        .context("regenerate index.json")?;
    }

    run("git", &["add", "-A"], Some(&clone_dir))?;
    let title = format!("feat({app_slug}): {flow_slug} — {}", validated.meta.name);
    run(
        "git",
        &[
            "-c",
            "user.name=iphone-use flow publish",
            "-c",
            &format!("user.email={login}@users.noreply.github.com"),
            "commit",
            "--quiet",
            "-m",
            &title,
        ],
        Some(&clone_dir),
    )?;
    let push_remote = if head_owner == owner {
        "origin".to_string()
    } else {
        let fork_url = format!(
            "https://github.com/{head_owner}/{}.git",
            slug.split('/').nth(1).unwrap()
        );
        run(
            "git",
            &["remote", "add", "fork", &fork_url],
            Some(&clone_dir),
        )?;
        "fork".to_string()
    };
    run(
        "git",
        &["push", "--quiet", "-u", &push_remote, &branch],
        Some(&clone_dir),
    )?;

    let body = pr_body(id, &validated, options.note.as_deref());
    let head = format!("{head_owner}:{branch}");
    let mut args = vec![
        "pr",
        "create",
        "-R",
        &slug,
        "--head",
        &head,
        "--title",
        &title,
        "--body",
        &body,
        "--label",
        LABEL_NEW_FLOW,
    ];
    if options.draft || !validated.meta.verified() {
        args.push("--draft");
    }
    let pr_url = match gh(&args) {
        Ok(url) => url,
        Err(error) if error.to_string().contains("label") => {
            // The label may not exist on a self-hosted mirror; retry without it.
            let args: Vec<&str> = args
                .iter()
                .copied()
                .filter(|a| *a != "--label" && *a != LABEL_NEW_FLOW)
                .collect();
            gh(&args)?
        }
        Err(error) => return Err(error),
    };
    Ok(PublishReport {
        ok: true,
        id: id.to_string(),
        repo: slug,
        branch,
        head,
        pr_url,
        new_app_json,
    })
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportContext {
    pub id: String,
    /// The daemon's `/agent/actions` result for the failed run, if any.
    pub result: Option<serde_json::Value>,
    /// `phone_status` snapshot (daemon version, device_state, …), if any.
    pub status: Option<serde_json::Value>,
    /// Foreground application label at failure time (locale-specific).
    pub application: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReportOutcome {
    pub ok: bool,
    pub id: String,
    pub repo: String,
    pub issue_url: String,
}

/// Every `error` code the daemon's agent surface can emit, gathered from its
/// JSON responses. A public report shows a code only if it is on this list.
///
/// A shape check (lowercase snake_case, short) is NOT enough: `secret123` and
/// `my_private_token` both satisfy it. Only an explicit set can promise that
/// what gets published is a daemon-authored code and not something that
/// arrived from elsewhere.
///
/// When the daemon grows a code and this list has not caught up, the report
/// says [`UNKNOWN_ERROR`] rather than dropping the field — you can still see
/// that there WAS an error, and the original never leaves the local response.
const DAEMON_ERROR_CODES: &[&str] = &[
    "adjust_no_effect",
    "ambiguous_element_label",
    "backend_is_direct",
    "backend_is_mirror",
    "batch_deadline",
    "batch_deadline_after_action",
    "batch_requires_direct_wda",
    "device_locked",
    "device_not_drivable",
    "device_release_in_progress",
    "device_transition_in_progress",
    "element_not_found",
    "expectation_timeout",
    "intent_args_too_large",
    "intent_bridge_unavailable",
    "intent_dispatch_failed",
    "intent_invalid_request",
    "intent_not_found",
    "intent_requires_operator_confirmation",
    "intent_timeout",
    "invalid_actions_request",
    "invalid_control_deadline",
    "invalid_control_message",
    "invalid_element_snapshot",
    "invalid_element_target",
    "invalid_hold",
    "invalid_owner",
    "invalid_owner_request",
    "invalid_value",
    "legacy_mirror_uses_webrtc",
    "lifecycle_busy",
    "missing_control_header",
    "no_alert",
    "not_sent",
    "outcome_unknown",
    "phone_handed_to_human",
    "phone_owned",
    "reconnect_in_progress",
    "serialization_failed",
    "stale_element_snapshot",
    "target_change_requires_restart",
    "target_not_configured",
    "target_required",
    "unauthorized",
    "unsupported_control",
    "unsupported_perform_action",
    "wda_batch_failed",
    "wda_is_externally_managed",
    "wda_not_configured",
    "wda_pre_dispatch_failed",
    "wda_source_failed",
    "wda_source_timeout",
    "wda_unavailable_or_unsupported",
];

/// Stand-in for an `error` code that is not (yet) in [`DAEMON_ERROR_CODES`].
/// Deliberately not the original text.
const UNKNOWN_ERROR: &str = "unknown_error";

/// Step kinds `/agent/actions` accepts.
const STEP_KINDS: &[&str] = &["action", "wait_for", "pause"];

/// An `error` field: a known daemon code, or a fixed placeholder. Never the
/// original string.
fn error_code(value: &serde_json::Value) -> Option<serde_json::Value> {
    let text = value.as_str()?;
    Some(serde_json::Value::String(
        if DAEMON_ERROR_CODES.contains(&text) {
            text.to_string()
        } else {
            UNKNOWN_ERROR.to_string()
        },
    ))
}

fn one_of(value: &serde_json::Value, allowed: &[&str]) -> Option<serde_json::Value> {
    let text = value.as_str()?;
    allowed
        .contains(&text)
        .then(|| serde_json::Value::String(text.to_string()))
}

fn count(value: &serde_json::Value) -> Option<serde_json::Value> {
    value.as_u64().map(serde_json::Value::from)
}

fn flag(value: &serde_json::Value) -> Option<serde_json::Value> {
    value.as_bool().map(serde_json::Value::Bool)
}

/// An array of element indices — numbers only, never a bare string.
fn indices(value: &serde_json::Value) -> Option<serde_json::Value> {
    let items = value.as_array()?;
    Some(serde_json::Value::Array(
        items.iter().filter_map(|item| item.as_u64().map(serde_json::Value::from)).collect(),
    ))
}

/// One field's type check: `Some` keeps the projected value, `None` drops it.
type Projection = fn(&serde_json::Value) -> Option<serde_json::Value>;

/// Build an object from `(key, projection)` pairs, dropping anything the
/// projection rejects and anything the source does not have.
fn project(source: &serde_json::Value, fields: &[(&str, Projection)]) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (key, projection) in fields {
        if let Some(value) = source.get(*key).and_then(projection) {
            out.insert((*key).to_string(), value);
        }
    }
    serde_json::Value::Object(out)
}

fn project_observation(value: &serde_json::Value) -> Option<serde_json::Value> {
    value.is_object().then(|| {
        project(
            value,
            &[
                ("read", flag),
                ("reads", count),
                ("attempts", count),
                ("stale", flag),
                ("sparse", flag),
                ("rows", count),
                ("application_matches", flag),
                ("missing_present", indices),
                ("violated_absent", indices),
                ("absent_unproven", indices),
            ],
        )
    })
}

fn project_settle(value: &serde_json::Value) -> Option<serde_json::Value> {
    value.is_object().then(|| {
        project(
            value,
            &[
                ("settled", flag),
                (
                    "reason",
                    |v| one_of(v, &["stable", "budget_exhausted", "observation_failed"]),
                ),
                ("waited_ms", count),
                ("captures", count),
                ("budget_ms", count),
                ("sparse", flag),
                ("stale", flag),
                // NOTE: `settle.error` is deliberately absent. It is
                // `format!("{error:#}")` of a WDA failure and embeds URLs,
                // hosts and ports. The structured `reason` says everything a
                // public report needs.
            ],
        )
    })
}

fn project_step(value: &serde_json::Value) -> Option<serde_json::Value> {
    value.is_object().then(|| {
        let mut step = project(
            value,
            &[
                ("index", count),
                ("kind", |v| one_of(v, STEP_KINDS)),
                ("ok", flag),
                ("attempts", count),
            ],
        );
        // A step's own `observation` gets the same treatment as the top-level
        // one; its `action` payload (typed text, labels) never survives.
        if let Some(observation) = value.get("observation").and_then(project_observation) {
            step["observation"] = observation;
        }
        step
    })
}

fn project_steps(value: &serde_json::Value) -> Option<serde_json::Value> {
    let items = value.as_array()?;
    Some(serde_json::Value::Array(
        items.iter().filter_map(project_step).collect(),
    ))
}

/// Project a daemon result down to publishable structure before it is pasted
/// into a PUBLIC issue.
///
/// This is a typed projection, not a key filter. Two properties matter:
///
/// * **Allow-list, not deny-list.** A field the daemon grows later is absent
///   from a report until someone adds it here. Forgetting costs a diagnostic;
///   with a deny-list, forgetting publishes private screen content.
/// * **Path- and type-aware.** `error` at the top level is a machine code;
///   `settle.error` is free text holding a WDA URL. A key-name rule cannot
///   tell them apart, so every field is projected at its own path and checked
///   for its own type. Strings survive only where the value is a machine code
///   or a known enum — never as free text.
pub fn redact_result(value: &serde_json::Value) -> serde_json::Value {
    let mut out = project(
        value,
        &[
            ("ok", flag),
            ("error", error_code),
            ("outcome", |v| {
                one_of(v, &["applied", "not_sent", "unknown", "no_effect"])
            }),
            // Same closed enum as `outcome`, under the name that says which
            // thing it describes.
            ("failed_step_outcome", |v| {
                one_of(v, &["applied", "not_sent", "unknown", "no_effect"])
            }),
            // The batch verdict is its own, smaller enum. Projected through
            // `one_of` like the others so an unexpected value is dropped
            // rather than published.
            ("batch_outcome", |v| {
                one_of(v, &["nothing_applied", "partially_applied", "unknown"])
            }),
            ("retry_safe", flag),
            ("failed_step", count),
            ("completed", count),
            ("applied_actions", count),
        ],
    );
    if let Some(steps) = value.get("steps").and_then(project_steps) {
        out["steps"] = steps;
    }
    if let Some(observation) = value.get("observation").and_then(project_observation) {
        out["observation"] = observation;
    }
    if let Some(settle) = value.get("settle").and_then(project_settle) {
        out["settle"] = settle;
    }
    out
}

pub fn issue_title(context: &ReportContext) -> String {
    let step = context
        .result
        .as_ref()
        .and_then(|r| r.get("failed_step"))
        .and_then(|v| v.as_u64())
        .map(|n| format!(" at step {n}"))
        .unwrap_or_default();
    // Through the same projection as the body: an issue TITLE is just as
    // public, and `error` is only safe because it is a machine code.
    let error = context
        .result
        .as_ref()
        .and_then(|r| r.get("error"))
        .and_then(error_code)
        .and_then(|code| code.as_str().map(|code| format!(" ({code})")))
        .unwrap_or_default();
    format!("{} failed{step}{error}", context.id)
}

pub fn issue_body(
    context: &ReportContext,
    flow: Option<&ValidatedFlow>,
    sha256: Option<&str>,
) -> String {
    let mut body = String::new();
    body.push_str(&format!("### Flow\n\n`{}`", context.id));
    if let Some(sha) = sha256 {
        body.push_str(&format!(" · sha256 `{}`", &sha[..sha.len().min(12)]));
    }
    body.push_str("\n\n");
    if let Some(flow) = flow {
        body.push_str(&format!(
            "risk {} · locale {} · verified_on {}\n\n",
            flow.meta.risk_label(),
            flow.meta.locale.as_deref().unwrap_or("—"),
            if flow.meta.verified_on.is_empty() {
                "none".to_string()
            } else {
                flow.meta
                    .verified_on
                    .iter()
                    .map(|v| {
                        format!(
                            "{}/{}/{}",
                            v.device.as_deref().unwrap_or("?"),
                            v.ios.as_deref().unwrap_or("?"),
                            v.date.as_deref().unwrap_or("?")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    if let Some(result) = &context.result {
        let redacted = redact_result(result);
        if let (Some(flow), Some(index)) =
            (flow, result.get("failed_step").and_then(|v| v.as_u64()))
        {
            if let Some(template) = flow.step_templates.get(index as usize) {
                body.push_str(&format!(
                    "### Failed step {index}\n\n```json\n{}\n```\n\n",
                    serde_json::to_string_pretty(template).unwrap_or_default()
                ));
            }
        }
        body.push_str(&format!(
            "### Daemon result (redacted)\n\n```json\n{}\n```\n\n",
            serde_json::to_string_pretty(&redacted).unwrap_or_default()
        ));
    }
    body.push_str("### Device\n\n");
    if let Some(app) = &context.application {
        body.push_str(&format!("- foreground app: `{app}`\n"));
    }
    if let Some(status) = &context.status {
        for key in [
            "version",
            "device_state",
            "backend",
            "mode",
            "wda_actionable",
        ] {
            if let Some(value) = status.get(key) {
                body.push_str(&format!("- {key}: `{value}`\n"));
            }
        }
    }
    if let Some(note) = context.note.as_deref().filter(|n| !n.trim().is_empty()) {
        body.push_str(&format!("\n### Notes\n\n{}\n", note.trim()));
    }
    body.push_str(
        "\n---\n_Filed with `iphone-use-mcp flow report`. Screen labels, typed text, and element \
         lists were stripped before posting; add them manually only if they contain nothing private._\n",
    );
    body
}

/// File an issue against the registry for a flow that failed.
pub fn report(context: &ReportContext) -> Result<ReportOutcome> {
    if !registry::valid_flow_id(&context.id) {
        bail!("{:?} is not a valid registry id", context.id);
    }
    let slug = repo_slug();
    let (flow, sha) = match registry::resolve_target(&context.id) {
        Ok(path) => {
            let flow = flow::load_flow(&path).ok();
            let sha = registry::load_index()
                .ok()
                .flatten()
                .and_then(|index| index.flows.get(&context.id).map(|e| e.sha256.clone()));
            (flow, sha)
        }
        Err(_) => (None, None),
    };
    let title = issue_title(context);
    let body = issue_body(context, flow.as_ref(), sha.as_deref());
    let args = [
        "issue",
        "create",
        "-R",
        &slug,
        "--title",
        &title,
        "--body",
        &body,
        "--label",
        LABEL_BROKEN,
    ];
    let issue_url = match gh(&args) {
        Ok(url) => url,
        Err(error) if error.to_string().contains("label") => gh(&[
            "issue", "create", "-R", &slug, "--title", &title, "--body", &body,
        ])?,
        Err(error) => return Err(error),
    };
    Ok(ReportOutcome {
        ok: true,
        id: context.id.clone(),
        repo: slug,
        issue_url,
    })
}

/// Where a locally added or exported flow file should be looked up when the
/// caller passes a registry id to `flow publish`.
pub fn publish_source(target: &str) -> Result<PathBuf> {
    registry::resolve_target(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn flow_file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    const FLOW: &str = r#"{"version":1,"name":"Open Health","description":"Launch Health.","app":"com.apple.Health","category":"health","risk":"navigation","locale":"zh-CN","verified_on":[{"device":"iPhone 17 Pro Max","ios":"26","date":"2026-09-05"}],"steps":[{"kind":"launch_app","bundle":"com.apple.Health"}]}"#;

    #[test]
    fn pr_body_lists_metadata_and_verification() {
        let dir = tempfile::tempdir().unwrap();
        let path = flow_file(dir.path(), "f.json", FLOW);
        let flow = flow::load_flow(&path).unwrap();
        let body = pr_body("health/open", &flow, Some("ran twice"));
        assert!(body.contains("`health/open` — Open Health"));
        assert!(body.contains("| risk | navigation |"));
        assert!(body.contains("iPhone 17 Pro Max · iOS 26"));
        assert!(body.contains("ran twice"));
        assert!(!body.contains("Not yet run"));
    }

    /// The two new batch fields are closed enums, and the projection has to
    /// treat them as such: a known value survives, anything else — including a
    /// screen label smuggled into the same key — is dropped rather than
    /// published into a public issue.
    #[test]
    fn the_batch_outcome_fields_are_projected_as_enums_not_free_text() {
        let kept = redact_result(&serde_json::json!({
            "ok": false,
            "outcome": "not_sent",
            "failed_step_outcome": "not_sent",
            "batch_outcome": "partially_applied",
            "applied_actions": 2,
            "retry_safe": false,
        }));
        assert_eq!(kept["failed_step_outcome"], "not_sent");
        assert_eq!(kept["batch_outcome"], "partially_applied");

        let dropped = redact_result(&serde_json::json!({
            "ok": false,
            // Not members of either enum: one is a plausible-looking value
            // from the wrong enum, one is screen text.
            "failed_step_outcome": "partially_applied",
            "batch_outcome": "SCREEN-LABEL-CANARY",
        }));
        assert!(
            dropped.get("failed_step_outcome").is_none(),
            "a value from the wrong enum must not be published: {dropped}"
        );
        assert!(
            dropped.get("batch_outcome").is_none(),
            "free text must never reach a public issue: {dropped}"
        );
        assert!(
            !dropped.to_string().contains("SCREEN-LABEL-CANARY"),
            "{dropped}"
        );
    }

    #[test]
    fn issue_body_redacts_screen_text_and_names_the_failed_step() {
        let dir = tempfile::tempdir().unwrap();
        let path = flow_file(dir.path(), "f.json", FLOW);
        let flow = flow::load_flow(&path).unwrap();
        let context = ReportContext {
            id: "health/open".into(),
            result: Some(serde_json::json!({
                "ok": false, "failed_step": 0, "error": "element_not_found",
                "observation": {"label": "PRIVATE-TEXT-CANARY", "candidates": ["a"], "missing_present": [0]},
                "steps": [{"action": {"type": "text", "text": "secret"}}]
            })),
            status: Some(serde_json::json!({"version": "0.5.4", "device_state": "ready"})),
            application: Some("健康".into()),
            note: None,
        };
        assert_eq!(
            issue_title(&context),
            "health/open failed at step 0 (element_not_found)"
        );
        let body = issue_body(&context, Some(&flow), Some("abcdef0123456789"));
        assert!(body.contains("### Failed step 0"));
        assert!(body.contains("com.apple.Health"));
        assert!(body.contains("missing_present"));
        assert!(!body.contains("PRIVATE-TEXT-CANARY"));
        assert!(!body.contains("secret"));
        assert!(!body.contains("candidates"));
        assert!(body.contains("- version: `\"0.5.4\"`"));
        assert!(body.contains("sha256 `abcdef012345`"));
    }

    /// The redaction is an allow-list, so a daemon field nobody thought about
    /// stays out of a public issue by default. A deny-list would publish it.
    #[test]
    fn a_daemon_field_nobody_allow_listed_is_not_published() {
        let redacted = redact_result(&serde_json::json!({
            "ok": false,
            "error": "expectation_timeout",
            "observation": {
                "read": true,
                "stale": true,
                "missing_present": [0],
                "hint": "no readable element tree was obtained",
                "application": "SCREEN-LABEL-CANARY"
            },
            "future_diagnostic": "PRIVATE-TEXT-CANARY"
        }));
        let text = serde_json::to_string(&redacted).unwrap();

        assert!(text.contains("expectation_timeout"), "{text}");
        assert!(text.contains("missing_present"), "{text}");
        assert!(text.contains("stale"), "{text}");
        assert!(!text.contains("SCREEN-LABEL-CANARY"), "{text}");
        assert!(!text.contains("future_diagnostic"), "{text}");
        assert!(!text.contains("PRIVATE-TEXT-CANARY"), "{text}");
    }

    /// `error` means different things at different paths: a machine code at
    /// the top, free text holding a WDA URL under `settle`. A key-name filter
    /// cannot tell them apart — this projection must.
    #[test]
    fn free_text_error_fields_never_survive_at_any_depth() {
        let redacted = redact_result(&serde_json::json!({
            "ok": true,
            "error": "wda_source_failed",
            "settle": {
                "settled": false,
                "reason": "observation_failed",
                "captures": 1,
                "error": "GET /source: error sending request for url \
                          (http://daemon.invalid:8100/source?format=json): PRIVATE-TEXT-CANARY"
            },
            "steps": [
                {"index": 0, "kind": "action", "ok": true},
                {"index": 1, "kind": "wait_for", "ok": false,
                 "error": "GET /source failed at http://daemon.invalid:8100 for SCREEN-LABEL-CANARY",
                 "observation": {"read": true, "sparse": false, "missing_present": [2],
                                 "application": "SCREEN-LABEL-CANARY"}}
            ]
        }));
        let text = serde_json::to_string(&redacted).unwrap();

        // Machine codes and structure survive.
        assert!(text.contains("wda_source_failed"), "{text}");
        assert!(text.contains("observation_failed"), "{text}");
        assert!(text.contains("wait_for"), "{text}");
        assert!(text.contains("missing_present"), "{text}");
        // Everything free-text does not, at any depth.
        assert!(!text.contains("daemon.invalid"), "{text}");
        assert!(!text.contains("8100"), "{text}");
        assert!(!text.contains("PRIVATE-TEXT-CANARY"), "{text}");
        assert!(!text.contains("SCREEN-LABEL-CANARY"), "{text}");
        assert!(!text.contains("GET /source"), "{text}");
    }

    /// Values of the wrong type are dropped rather than passed through: a
    /// free-text string parked on a field that should hold a count or an enum
    /// must not become a publication channel.
    #[test]
    fn a_wrongly_typed_field_is_dropped_not_forwarded() {
        let redacted = redact_result(&serde_json::json!({
            "ok": "PRIVATE-TEXT-CANARY",
            "failed_step": "http://daemon.invalid:8100",
            "outcome": "SCREEN-LABEL-CANARY",
            "observation": {"missing_present": ["PRIVATE-TEXT-CANARY"], "reads": true}
        }));
        let text = serde_json::to_string(&redacted).unwrap();

        assert!(!text.contains("PRIVATE-TEXT-CANARY"), "{text}");
        assert!(!text.contains("daemon.invalid"), "{text}");
        assert!(!text.contains("SCREEN-LABEL-CANARY"), "{text}");
        assert_eq!(redacted["observation"]["missing_present"], serde_json::json!([]));
        assert!(redacted["observation"]["reads"].is_null(), "{text}");
    }

    /// A string that merely LOOKS like a machine code is not one. `secret123`
    /// satisfies every shape rule and is exactly what a password or a pasted
    /// token looks like, so only membership in the daemon's own code set may
    /// put a string in a public report.
    #[test]
    fn a_string_shaped_like_a_code_is_still_not_published() {
        let redacted = redact_result(&serde_json::json!({
            "ok": false,
            "error": "secret123",
            "steps": [{"index": 0, "kind": "my_private_token", "ok": false}]
        }));
        let text = serde_json::to_string(&redacted).unwrap();

        assert!(!text.contains("secret123"), "{text}");
        assert!(!text.contains("my_private_token"), "{text}");
        // Not dropped silently: the report still says an error happened.
        assert_eq!(redacted["error"], "unknown_error");
        assert!(redacted["steps"][0]["kind"].is_null(), "{text}");
        assert_eq!(redacted["steps"][0]["index"], 0, "structure survives");
    }

    /// The other half of the same rule: real daemon codes must survive, or the
    /// redaction would make reports useless.
    #[test]
    fn real_daemon_codes_survive() {
        let redacted = redact_result(&serde_json::json!({
            "ok": false,
            "error": "expectation_timeout",
            "outcome": "not_sent",
            "steps": [
                {"index": 0, "kind": "action", "ok": true},
                {"index": 1, "kind": "wait_for", "ok": false},
                {"index": 2, "kind": "pause", "ok": false}
            ]
        }));

        assert_eq!(redacted["error"], "expectation_timeout");
        assert_eq!(redacted["outcome"], "not_sent");
        assert_eq!(redacted["steps"][0]["kind"], "action");
        assert_eq!(redacted["steps"][1]["kind"], "wait_for");
        assert_eq!(redacted["steps"][2]["kind"], "pause");
    }

    /// A code-shaped secret must not reach the issue TITLE either.
    #[test]
    fn a_code_shaped_secret_never_reaches_the_issue_title() {
        let context = ReportContext {
            id: "health/open".into(),
            result: Some(serde_json::json!({"failed_step": 0, "error": "secret123"})),
            status: None,
            application: None,
            note: None,
        };
        let title = issue_title(&context);
        assert_eq!(title, "health/open failed at step 0 (unknown_error)");
        assert!(!title.contains("secret123"));
    }

    /// The title is as public as the body and goes through the same check.
    #[test]
    fn a_free_text_error_never_reaches_the_issue_title() {
        let context = ReportContext {
            id: "health/open".into(),
            result: Some(serde_json::json!({
                "failed_step": 1,
                "error": "GET /source failed at http://daemon.invalid:8100 for SCREEN-LABEL-CANARY"
            })),
            status: None,
            application: None,
            note: None,
        };
        let title = issue_title(&context);
        assert_eq!(title, "health/open failed at step 1 (unknown_error)");
        assert!(!title.contains("daemon.invalid"));
        assert!(!title.contains("SCREEN-LABEL-CANARY"));
    }

    /// End-to-end publish against a local bare repo with a stub `gh`.
    #[test]
    fn publish_pushes_a_branch_and_opens_a_pr_through_gh() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        // Upstream: a bare repo seeded with scripts/build-index.py and one app.
        let seed = root.path().join("seed");
        fs::create_dir_all(seed.join("scripts")).unwrap();
        fs::create_dir_all(seed.join("system")).unwrap();
        fs::write(seed.join("scripts/build-index.py"), "#!/usr/bin/env python3\nimport json,pathlib\nflows=[str(p) for p in sorted(pathlib.Path('.').glob('*/*.json')) if not p.name=='app.json']\npathlib.Path('index.json').write_text(json.dumps({'version':1,'flows':flows}))\n").unwrap();
        fs::write(
            seed.join("system/app.json"),
            r#"{"id":"system","name":"iOS"}"#,
        )
        .unwrap();
        let git = |args: &[&str], cwd: &Path| run("git", args, Some(cwd)).unwrap();
        git(&["init", "--quiet", "-b", "main"], &seed);
        git(&["add", "-A"], &seed);
        git(
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--quiet",
                "-m",
                "seed",
            ],
            &seed,
        );
        let bare = root.path().join("upstream.git");
        run(
            "git",
            &[
                "clone",
                "--quiet",
                "--bare",
                seed.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
            None,
        )
        .unwrap();

        // Stub gh: logs every invocation, answers `api user` and `pr create`.
        let log = root.path().join("gh.log");
        let stub = root.path().join("gh");
        fs::write(&stub, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$1 $2\" in\n  'api user') echo tester ;;\n  'pr create') echo https://example.test/pr/1 ;;\nesac\n", log.display())).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

        std::env::set_var(GH_BIN_ENV, &stub);
        std::env::set_var(REPO_ENV, "tester/flows");
        std::env::set_var(REPO_URL_ENV, bare.to_str().unwrap());

        let source = flow_file(root.path(), "open.json", FLOW);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        let report = publish(
            &source,
            &PublishOptions {
                id: "health/open".into(),
                app_name: Some("Apple Health".into()),
                aliases: vec!["Health".into(), "健康".into()],
                note: Some("hw ok".into()),
                draft: false,
            },
        )
        .unwrap();
        std::env::remove_var(GH_BIN_ENV);
        std::env::remove_var(REPO_ENV);
        std::env::remove_var(REPO_URL_ENV);

        assert_eq!(report.pr_url, "https://example.test/pr/1");
        assert_eq!(report.branch, "flow/health-open");
        assert!(report.new_app_json);
        let logged = fs::read_to_string(&log).unwrap();
        assert!(logged.contains("api user --jq .login"));
        assert!(logged.contains("pr create -R tester/flows --head tester:flow/health-open"));
        assert!(
            !logged.contains("repo fork"),
            "owner publishes without forking"
        );
        // The branch reached upstream with the flow, the app.json, and a rebuilt index.
        let files = run(
            "git",
            &["ls-tree", "-r", "--name-only", "flow/health-open"],
            Some(&bare),
        )
        .unwrap();
        assert!(files.contains("health/open.json"));
        assert!(files.contains("health/app.json"));
        assert!(files.contains("index.json"));
        let app_json = run(
            "git",
            &["show", "flow/health-open:health/app.json"],
            Some(&bare),
        )
        .unwrap();
        assert!(app_json.contains("健康"));
        assert!(app_json.contains("com.apple.Health"));
    }
}
