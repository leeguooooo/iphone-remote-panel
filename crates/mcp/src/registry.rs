//! The official flow registry: a GitHub repository of reviewed, per-app flow
//! files that `iphone-use-mcp flow update` mirrors into a local store so a
//! repeated phone task becomes `flow run <app>/<flow>` with no model involved.
//!
//! Layout of the source (see <https://github.com/leeguooooo/iphone-use-flows>):
//!
//! ```text
//! index.json                 {"version":1,"flows":[{"id":"health/export-all","path":"health/export-all.json","sha256":"…"}],"apps":[…]}
//! <app>/app.json             {"id":"health","bundle":"com.apple.Health","name":"Health","category":"health"}
//! <app>/<flow>.json          strict flow v1 (+ optional registry metadata)
//! ```
//!
//! Only the single official source is supported. `IPHONE_USE_FLOWS_SOURCE`
//! may override it with another base URL or a local directory for development
//! and tests; there is deliberately no `sources add`.
//!
//! Local store: `$IPHONE_USE_FLOWS_DIR` or `~/.iphone-use/flows`, mode 0700.
//! Files are written 0600 so the runner's ownership/permission checks accept
//! them. `.index.json` caches the validated metadata of every stored flow and
//! remembers which ones came from the official source and which were added
//! locally with `flow add`, so an update never deletes a user's own flows.

use crate::flow::{self, FlowMeta};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const OFFICIAL_SOURCE_NAME: &str = "official";
pub const OFFICIAL_SOURCE_URL: &str =
    "https://raw.githubusercontent.com/leeguooooo/iphone-use-flows/main";
pub const OFFICIAL_REPO_URL: &str = "https://github.com/leeguooooo/iphone-use-flows";
pub const SOURCE_ENV: &str = "IPHONE_USE_FLOWS_SOURCE";
pub const STORE_ENV: &str = "IPHONE_USE_FLOWS_DIR";
pub const LOCAL_SOURCE_NAME: &str = "local";

const INDEX_FILE: &str = "index.json";
const LOCAL_INDEX_FILE: &str = ".index.json";
const MAX_INDEX_BYTES: usize = 1024 * 1024;
const MAX_FLOWS: usize = 2_000;
const MAX_APPS: usize = 500;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Identifiers and paths
// ---------------------------------------------------------------------------

/// A registry flow id is `<app>/<flow>`, both lowercase slugs.
pub fn valid_flow_id(id: &str) -> bool {
    match id.split_once('/') {
        Some((app, name)) => flow::valid_slug(app) && flow::valid_slug(name) && !name.contains('/'),
        None => false,
    }
}

/// Where flows live on this machine. Created 0700 on first use.
pub fn store_dir() -> Result<PathBuf> {
    let dir = match std::env::var(STORE_ENV) {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var("HOME")
                .context("HOME is not set; cannot locate ~/.iphone-use/flows")?;
            PathBuf::from(home).join(".iphone-use").join("flows")
        }
    };
    ensure_private_dir(&dir)?;
    Ok(dir)
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create flow store {}", dir.display()))?;
    }
    if !dir.is_dir() {
        bail!("flow store path is not a directory: {}", dir.display());
    }
    Ok(())
}

pub fn flow_path(store: &Path, id: &str) -> PathBuf {
    let (app, name) = id
        .split_once('/')
        .expect("valid_flow_id checked before flow_path");
    store.join(app).join(format!("{name}.json"))
}

/// Turn a CLI/MCP target into a file path. A target that names an existing
/// file, ends in `.json`, or looks like a path is used as-is; otherwise it must
/// be a registry id resolved inside the local store.
pub fn resolve_target(target: &str) -> Result<PathBuf> {
    let looks_like_path = target.starts_with('/')
        || target.starts_with("./")
        || target.starts_with("../")
        || target.starts_with("~/")
        || target.ends_with(".json");
    let as_path = Path::new(target);
    if looks_like_path || as_path.is_file() {
        return Ok(as_path.to_path_buf());
    }
    if !valid_flow_id(target) {
        bail!(
            "{target:?} is neither a flow file nor a registry id; registry ids look like \
             health/export-all (lowercase slugs). Run `flow list` to see installed flows"
        );
    }
    let store = store_dir()?;
    let path = flow_path(&store, target);
    if !path.is_file() {
        let hint = if load_index()?.is_none() {
            "the local store is empty; run `flow update` first"
        } else {
            "run `flow list` to see installed flows or `flow update` to refresh the store"
        };
        bail!("flow {target:?} is not installed ({hint})");
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// Index formats
// ---------------------------------------------------------------------------

/// `index.json` as published by the source. Extra fields are ignored so the
/// source can carry human-facing metadata without breaking older CLIs.
#[derive(Debug, Deserialize)]
struct SourceIndex {
    version: u32,
    #[serde(default)]
    flows: Vec<SourceFlow>,
    #[serde(default)]
    apps: Vec<AppEntry>,
}

#[derive(Debug, Deserialize)]
struct SourceFlow {
    id: String,
    path: String,
    sha256: String,
}

/// One app directory's `app.json`, mirrored into the local index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Foreground `Application` labels (one per locale) that identify this
    /// app in `/agent/elements`, e.g. `["Health", "健康"]`. Lets the MCP
    /// surface matching flows the moment the agent looks at the screen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// `.index.json` inside the local store.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LocalIndex {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub flows: BTreeMap<String, LocalFlow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<AppEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFlow {
    /// `official` or `local`.
    pub source: String,
    pub sha256: String,
    #[serde(flatten)]
    pub meta: FlowMeta,
}

pub fn load_index() -> Result<Option<LocalIndex>> {
    let path = store_dir()?.join(LOCAL_INDEX_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let index: LocalIndex =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(index))
}

fn save_index(store: &Path, index: &LocalIndex) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(index)?;
    write_private_atomic(store, LOCAL_INDEX_FILE, &bytes)
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

enum Fetcher {
    Http {
        client: reqwest::Client,
        base: String,
    },
    Dir(PathBuf),
}

impl Fetcher {
    fn from_env() -> Result<(Self, String)> {
        let source = std::env::var(SOURCE_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OFFICIAL_SOURCE_URL.to_string());
        if source.starts_with("http://") || source.starts_with("https://") {
            let client = reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .user_agent(concat!("iphone-use-mcp/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("build HTTP client for flow registry")?;
            let base = source.trim_end_matches('/').to_string();
            Ok((Fetcher::Http { client, base }, source))
        } else {
            let dir = PathBuf::from(&source);
            if !dir.is_dir() {
                bail!("{SOURCE_ENV}={source:?} is neither an http(s) URL nor a directory");
            }
            Ok((Fetcher::Dir(dir), source))
        }
    }

    async fn fetch(&self, relative: &str, limit: usize) -> Result<Vec<u8>> {
        match self {
            Fetcher::Http { client, base } => {
                let url = format!("{base}/{relative}");
                let response = client
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("GET {url}"))?;
                let status = response.status();
                if !status.is_success() {
                    bail!("GET {url} returned HTTP {status}");
                }
                let bytes = response
                    .bytes()
                    .await
                    .with_context(|| format!("read body of {url}"))?;
                if bytes.len() > limit {
                    bail!("{url} is larger than {limit} bytes");
                }
                Ok(bytes.to_vec())
            }
            Fetcher::Dir(dir) => {
                let path = dir.join(relative);
                let metadata =
                    fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
                if metadata.len() as usize > limit {
                    bail!("{} is larger than {limit} bytes", path.display());
                }
                fs::read(&path).with_context(|| format!("read {}", path.display()))
            }
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct UpdateReport {
    pub ok: bool,
    pub source: String,
    pub store: String,
    pub installed: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub removed: Vec<String>,
    pub kept_local: Vec<String>,
    pub apps: usize,
}

/// Mirror the official source into the local store. Every file is validated
/// with the same strict parser `flow run` uses before it is written; a single
/// bad file aborts the update and leaves the store untouched.
pub async fn update() -> Result<UpdateReport> {
    let (fetcher, source_label) = Fetcher::from_env()?;
    let store = store_dir()?;

    let index_bytes = fetcher.fetch(INDEX_FILE, MAX_INDEX_BYTES).await?;
    let index: SourceIndex =
        serde_json::from_slice(&index_bytes).context("parse registry index.json")?;
    if index.version != 1 {
        bail!(
            "registry index version {} is not supported by this CLI; upgrade iphone-use",
            index.version
        );
    }
    if index.flows.len() > MAX_FLOWS {
        bail!("registry index lists more than {MAX_FLOWS} flows");
    }
    if index.apps.len() > MAX_APPS {
        bail!("registry index lists more than {MAX_APPS} apps");
    }
    for app in &index.apps {
        if !flow::valid_slug(&app.id) {
            bail!("registry app id {:?} is not a lowercase slug", app.id);
        }
    }

    // Fetch + validate everything before touching the store.
    let mut staged: BTreeMap<String, (Vec<u8>, String, FlowMeta)> = BTreeMap::new();
    for entry in &index.flows {
        if !valid_flow_id(&entry.id) {
            bail!(
                "registry flow id {:?} must look like <app>/<flow>",
                entry.id
            );
        }
        if entry.path != format!("{}.json", entry.id) {
            bail!(
                "registry flow {:?} has path {:?}; expected {:?}",
                entry.id,
                entry.path,
                format!("{}.json", entry.id)
            );
        }
        if staged.contains_key(&entry.id) {
            bail!("registry flow {:?} is listed twice", entry.id);
        }
        let bytes = fetcher
            .fetch(&entry.path, flow::MAX_FLOW_BYTES as usize)
            .await
            .with_context(|| format!("fetch flow {}", entry.id))?;
        let digest = sha256_hex(&bytes);
        if !digest.eq_ignore_ascii_case(entry.sha256.trim()) {
            bail!(
                "registry flow {:?} failed its sha256 check (index says {}, file is {digest}); \
                 the source may be mid-publish — retry later",
                entry.id,
                entry.sha256
            );
        }
        let validated = flow::parse_flow(&bytes, &entry.id)
            .with_context(|| format!("validate registry flow {}", entry.id))?;
        staged.insert(entry.id.clone(), (bytes, digest, validated.meta));
    }

    let previous = load_index()?.unwrap_or_default();
    let mut next = LocalIndex {
        version: 1,
        updated_at: Some(now_rfc3339()),
        source: Some(source_label.clone()),
        flows: BTreeMap::new(),
        apps: index.apps.clone(),
    };
    let mut report = UpdateReport {
        ok: true,
        source: source_label,
        store: store.display().to_string(),
        installed: Vec::new(),
        updated: Vec::new(),
        unchanged: Vec::new(),
        removed: Vec::new(),
        kept_local: Vec::new(),
        apps: index.apps.len(),
    };

    for (id, (bytes, digest, meta)) in staged {
        match previous.flows.get(&id) {
            Some(existing)
                if existing.source == OFFICIAL_SOURCE_NAME
                    && existing.sha256 == digest
                    && flow_path(&store, &id).is_file() =>
            {
                report.unchanged.push(id.clone());
            }
            Some(existing) if existing.source == OFFICIAL_SOURCE_NAME => {
                write_flow_file(&store, &id, &bytes)?;
                report.updated.push(id.clone());
            }
            Some(_) => {
                // A locally added flow shadows the official one under the same
                // id; keep the user's file, but do not pretend it is official.
                report.kept_local.push(id.clone());
                if let Some(local) = previous.flows.get(&id) {
                    next.flows.insert(id.clone(), local.clone());
                }
                continue;
            }
            None => {
                write_flow_file(&store, &id, &bytes)?;
                report.installed.push(id.clone());
            }
        }
        next.flows.insert(
            id,
            LocalFlow {
                source: OFFICIAL_SOURCE_NAME.to_string(),
                sha256: digest,
                meta,
            },
        );
    }

    for (id, existing) in &previous.flows {
        if next.flows.contains_key(id) {
            continue;
        }
        if existing.source == OFFICIAL_SOURCE_NAME {
            let path = flow_path(&store, id);
            if path.is_file() {
                fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            }
            report.removed.push(id.clone());
        } else {
            next.flows.insert(id.clone(), existing.clone());
            report.kept_local.push(id.clone());
        }
    }

    save_index(&store, &next)?;
    Ok(report)
}

fn write_flow_file(store: &Path, id: &str, bytes: &[u8]) -> Result<()> {
    let path = flow_path(store, id);
    let parent = path.parent().expect("flow path has an app directory");
    ensure_private_dir(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("flow file name is a slug");
    write_private_atomic(parent, file_name, bytes)
}

/// Write `dir/name` with mode 0600 via a temp file + rename so a reader never
/// sees a partial flow and the runner's permission checks always pass.
fn write_private_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all().ok();
    }
    let target = dir.join(name);
    if let Err(error) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(error).with_context(|| format!("rename into {}", target.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list / info / add / remove / sources
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ListFilter {
    pub category: Option<String>,
    pub app: Option<String>,
    pub verified_only: bool,
}

/// Installed flows, sorted by id, after applying the filter. `app` matches
/// either the registry app directory or the declared bundle identifier.
pub fn list(filter: &ListFilter) -> Result<(Vec<(String, LocalFlow)>, LocalIndex)> {
    let Some(index) = load_index()? else {
        bail!("no flows are installed yet; run `iphone-use-mcp flow update` to mirror the official registry");
    };
    let entries = index
        .flows
        .iter()
        .filter(|(id, entry)| {
            let by_category = filter
                .category
                .as_deref()
                .is_none_or(|wanted| entry.meta.category.as_deref() == Some(wanted));
            let by_app = filter.app.as_deref().is_none_or(|wanted| {
                id.split('/').next() == Some(wanted) || entry.meta.app.as_deref() == Some(wanted)
            });
            let by_verified = !filter.verified_only || entry.meta.verified();
            by_category && by_app && by_verified
        })
        .map(|(id, entry)| (id.clone(), entry.clone()))
        .collect::<Vec<_>>();
    Ok((entries, index))
}

/// Human-readable table for the terminal.
pub fn list_text(entries: &[(String, LocalFlow)], index: &LocalIndex) -> String {
    let mut out = String::new();
    if entries.is_empty() {
        out.push_str("no installed flows match\n");
    } else {
        let id_width = entries
            .iter()
            .map(|(id, _)| id.len())
            .max()
            .unwrap_or(2)
            .max(2);
        out.push_str(&format!(
            "{:<id_width$}  {:<11}  {:<8}  {:<9}  {}\n",
            "ID", "RISK", "VERIFIED", "INPUTS", "NAME"
        ));
        for (id, entry) in entries {
            let inputs = if entry.meta.inputs.is_empty() {
                "-".to_string()
            } else {
                entry.meta.inputs.join(",")
            };
            out.push_str(&format!(
                "{:<id_width$}  {:<11}  {:<8}  {:<9}  {}\n",
                id,
                entry.meta.risk_label(),
                if entry.meta.verified() { "yes" } else { "no" },
                inputs,
                entry.meta.name
            ));
        }
    }
    if let Some(updated) = &index.updated_at {
        out.push_str(&format!(
            "{} flow(s) · source {} · updated {}\n",
            index.flows.len(),
            index.source.as_deref().unwrap_or(OFFICIAL_SOURCE_NAME),
            updated
        ));
    }
    out
}

pub fn list_json(entries: &[(String, LocalFlow)], index: &LocalIndex) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "source": index.source,
        "updated_at": index.updated_at,
        "apps": index.apps,
        "flows": entries.iter().map(|(id, entry)| {
            let mut value = serde_json::to_value(entry).expect("LocalFlow serializes");
            let object = value.as_object_mut().expect("LocalFlow is an object");
            object.insert("id".into(), serde_json::json!(id));
            object.insert("verified".into(), serde_json::json!(entry.meta.verified()));
            object.insert("risk".into(), serde_json::json!(entry.meta.risk_label()));
            value
        }).collect::<Vec<_>>()
    })
}

/// Full detail of one installed flow or flow file, including its step
/// templates (which never contain runtime input values).
pub fn info(target: &str) -> Result<serde_json::Value> {
    let path = resolve_target(target)?;
    let validated = flow::load_flow(&path)?;
    let mut summary = flow::flow_summary(&validated);
    summary["path"] = serde_json::json!(path.display().to_string());
    summary["inputs"] = serde_json::to_value(&validated.inputs)?;
    summary["steps"] = serde_json::json!(validated.step_templates);
    summary["step_count"] = serde_json::json!(validated.meta.steps);
    if valid_flow_id(target) {
        summary["id"] = serde_json::json!(target);
        if let Some(index) = load_index()? {
            if let Some(entry) = index.flows.get(target) {
                summary["source"] = serde_json::json!(entry.source);
                summary["sha256"] = serde_json::json!(entry.sha256);
            }
            if let Some(app) = index
                .apps
                .iter()
                .find(|app| Some(app.id.as_str()) == target.split('/').next())
            {
                summary["app_entry"] = serde_json::to_value(app)?;
            }
        }
    }
    Ok(summary)
}

/// Copy a validated local flow file into the store under a registry id so it
/// can be run by name next to the official flows. Survives `flow update`.
pub fn add(file: &Path, id: &str) -> Result<serde_json::Value> {
    if !valid_flow_id(id) {
        bail!("{id:?} is not a valid registry id; use <app>/<flow> lowercase slugs, e.g. myapp/daily-check");
    }
    let bytes = flow::read_flow_bytes(file)?;
    let validated = flow::parse_flow(&bytes, &file.display().to_string())?;
    let store = store_dir()?;
    let mut index = load_index()?.unwrap_or(LocalIndex {
        version: 1,
        ..Default::default()
    });
    if let Some(existing) = index.flows.get(id) {
        if existing.source == OFFICIAL_SOURCE_NAME {
            bail!("{id:?} is an official flow; pick a different id (for example {id}-mine) instead of shadowing it");
        }
    }
    write_flow_file(&store, id, &bytes)?;
    index.flows.insert(
        id.to_string(),
        LocalFlow {
            source: LOCAL_SOURCE_NAME.to_string(),
            sha256: sha256_hex(&bytes),
            meta: validated.meta.clone(),
        },
    );
    save_index(&store, &index)?;
    Ok(serde_json::json!({
        "ok": true,
        "id": id,
        "source": LOCAL_SOURCE_NAME,
        "path": flow_path(&store, id).display().to_string(),
        "name": validated.meta.name,
    }))
}

/// Remove one installed flow. An official flow comes back on the next update.
pub fn remove(id: &str) -> Result<serde_json::Value> {
    if !valid_flow_id(id) {
        bail!("{id:?} is not a valid registry id");
    }
    let store = store_dir()?;
    let mut index = load_index()?.unwrap_or_default();
    let entry = index.flows.remove(id);
    let path = flow_path(&store, id);
    let existed = path.is_file();
    if existed {
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    if entry.is_none() && !existed {
        bail!("flow {id:?} is not installed");
    }
    save_index(&store, &index)?;
    Ok(serde_json::json!({
        "ok": true,
        "id": id,
        "removed": true,
        "was_official": entry.as_ref().is_some_and(|e| e.source == OFFICIAL_SOURCE_NAME),
    }))
}

// ---------------------------------------------------------------------------
// Hints: bring the registry to the agent instead of hoping it looks
// ---------------------------------------------------------------------------

/// Installed flows whose app matches a foreground application label (via
/// `app.json` `aliases` / `name`) — locale-aware without the flow knowing.
pub fn flows_for_application(index: &LocalIndex, application: &str) -> Vec<(String, LocalFlow)> {
    let wanted = application.trim();
    if wanted.is_empty() {
        return Vec::new();
    }
    let app_ids: Vec<&str> = index
        .apps
        .iter()
        .filter(|app| {
            app.aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(wanted))
                || app
                    .name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
        })
        .map(|app| app.id.as_str())
        .collect();
    if app_ids.is_empty() {
        return Vec::new();
    }
    index
        .flows
        .iter()
        .filter(|(id, _)| app_ids.contains(&id.split('/').next().unwrap_or("")))
        .map(|(id, entry)| (id.clone(), entry.clone()))
        .collect()
}

/// A compact `registry` block for a `/agent/elements` response: the flows that
/// fit the foreground app, or a nudge to populate the store. Never fails —
/// hints must not break an elements read.
pub fn elements_hint(elements_body: &str) -> Option<serde_json::Value> {
    let body: serde_json::Value = serde_json::from_str(elements_body).ok()?;
    let application = body
        .get("elements")
        .and_then(|rows| rows.as_array())
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("kind").and_then(|k| k.as_str()) == Some("Application"))
        })
        .and_then(|row| row.get("label").and_then(|l| l.as_str()))
        .map(str::to_string);
    let index = match load_index() {
        Ok(Some(index)) => index,
        Ok(None) => {
            return Some(serde_json::json!({
                "installed": 0,
                "hint": "no registry flows installed — call phone_flow_update once, then phone_flow_list before driving this app by hand"
            }))
        }
        Err(_) => return None,
    };
    let matches = application
        .as_deref()
        .map(|app| flows_for_application(&index, app))
        .unwrap_or_default();
    let mut hint = serde_json::json!({
        "installed": index.flows.len(),
        "application": application,
    });
    if matches.is_empty() {
        hint["flows"] = serde_json::json!([]);
        hint["hint"] = serde_json::json!(
            "no installed flow targets this app — if you complete a multi-step task here, save it with phone_flow_publish so the next run costs one call"
        );
    } else {
        hint["flows"] = serde_json::json!(matches
            .iter()
            .map(|(id, entry)| serde_json::json!({
                "id": id,
                "name": entry.meta.name,
                "risk": entry.meta.risk_label(),
                "verified": entry.meta.verified(),
                "inputs": entry.meta.inputs,
            }))
            .collect::<Vec<_>>());
        hint["hint"] = serde_json::json!(
            "registry flows exist for this app — prefer phone_flow_run over step-by-step exploration when one matches the task"
        );
    }
    Some(hint)
}

/// Attach a `registry` block to a JSON body when it parses as an object;
/// otherwise return the body untouched.
pub fn attach_hint(body: String, key: &str, hint: Option<serde_json::Value>) -> String {
    let Some(hint) = hint else { return body };
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(mut value) if value.is_object() => {
            value[key] = hint;
            value.to_string()
        }
        _ => body,
    }
}

pub fn sources_json() -> Result<serde_json::Value> {
    let override_source = std::env::var(SOURCE_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    Ok(serde_json::json!({
        "ok": true,
        "official": {
            "name": OFFICIAL_SOURCE_NAME,
            "repo": OFFICIAL_REPO_URL,
            "raw_base": OFFICIAL_SOURCE_URL,
        },
        "override": override_source,
        "override_env": SOURCE_ENV,
        "store": store_dir()?.display().to_string(),
        "store_env": STORE_ENV,
        "note": "only the official source is supported; there is no `sources add`",
    }))
}

// ---------------------------------------------------------------------------
// Time (no chrono dependency)
// ---------------------------------------------------------------------------

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests mutate process env vars; serialize them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn flow_json(name: &str, extra: &str) -> String {
        format!(
            r#"{{"version":1,"name":"{name}",{extra}"steps":[{{"kind":"shortcut","name":"home"}}]}}"#
        )
    }

    fn write_source(dir: &Path, flows: &[(&str, &str)]) {
        let mut entries = Vec::new();
        for (id, body) in flows {
            let path = dir.join(format!("{id}.json"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
            entries.push(serde_json::json!({
                "id": id,
                "path": format!("{id}.json"),
                "sha256": sha256_hex(body.as_bytes()),
            }));
        }
        let index = serde_json::json!({
            "version": 1,
            "flows": entries,
            "apps": [{"id":"system","name":"iOS","category":"system"}]
        });
        fs::write(dir.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    }

    struct Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        _source: tempfile::TempDir,
        _store: tempfile::TempDir,
    }

    fn setup(flows: &[(&str, &str)]) -> Env {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let source = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_source(source.path(), flows);
        std::env::set_var(SOURCE_ENV, source.path());
        std::env::set_var(STORE_ENV, store.path().join("flows"));
        Env {
            _guard: guard,
            _source: source,
            _store: store,
        }
    }

    #[test]
    fn flow_ids_are_two_lowercase_slugs() {
        assert!(valid_flow_id("health/export-all"));
        assert!(valid_flow_id("system/open_spotlight2"));
        assert!(!valid_flow_id("health"));
        assert!(!valid_flow_id("Health/export"));
        assert!(!valid_flow_id("health/export/all"));
        assert!(!valid_flow_id("../x/y"));
        assert!(!valid_flow_id("health/"));
    }

    #[tokio::test]
    async fn update_installs_validates_and_removes_official_flows() {
        let env = setup(&[
            ("system/home", &flow_json("Home", "")),
            (
                "system/spot",
                &flow_json("Spot", r#""category":"system","risk":"navigation","#),
            ),
        ]);
        let report = update().await.unwrap();
        assert_eq!(report.installed, vec!["system/home", "system/spot"]);
        assert_eq!(report.apps, 1);

        let store = store_dir().unwrap();
        let path = flow_path(&store, "system/spot");
        assert!(path.is_file());
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(flow::load_flow(&path).is_ok());

        let (entries, _) = list(&ListFilter {
            category: Some("system".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "system/spot");
        assert_eq!(entries[0].1.meta.risk_label(), "navigation");

        // Second update: unchanged.
        let report = update().await.unwrap();
        assert_eq!(report.unchanged.len(), 2);
        assert!(report.installed.is_empty());

        // Drop one flow from the source: it is removed locally.
        write_source(
            env._source.path(),
            &[("system/home", &flow_json("Home", ""))],
        );
        let report = update().await.unwrap();
        assert_eq!(report.removed, vec!["system/spot"]);
        assert!(!path.exists());
        assert_eq!(
            resolve_target("system/home").unwrap(),
            flow_path(&store, "system/home")
        );
        assert!(resolve_target("system/spot")
            .unwrap_err()
            .to_string()
            .contains("not installed"));
    }

    #[tokio::test]
    async fn update_rejects_bad_checksums_without_touching_the_store() {
        let env = setup(&[("system/home", &flow_json("Home", ""))]);
        let index_path = env._source.path().join("index.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
        index["flows"][0]["sha256"] = serde_json::json!("00");
        fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
        let error = update().await.unwrap_err().to_string();
        assert!(error.contains("sha256"), "{error}");
        assert!(load_index().unwrap().is_none());
    }

    #[tokio::test]
    async fn update_rejects_an_invalid_flow_file() {
        let _env = setup(&[(
            "system/bad",
            r#"{"version":1,"name":"x","steps":[{"kind":"pause","ms":1}],"retry":true}"#,
        )]);
        let error = format!("{:#}", update().await.unwrap_err());
        assert!(
            error.contains("validate registry flow system/bad"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn local_flows_survive_updates_and_cannot_shadow_official_ids() {
        let _env = setup(&[("system/home", &flow_json("Home", ""))]);
        update().await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mine.json");
        fs::write(&file, flow_json("Mine", r#""risk":"side_effect","#)).unwrap();
        let added = add(&file, "myapp/daily").unwrap();
        assert_eq!(added["source"], "local");
        assert!(add(&file, "system/home")
            .unwrap_err()
            .to_string()
            .contains("official flow"));

        let report = update().await.unwrap();
        assert_eq!(report.kept_local, vec!["myapp/daily"]);
        let (entries, _) = list(&ListFilter::default()).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["myapp/daily", "system/home"]
        );

        let detail = info("myapp/daily").unwrap();
        assert_eq!(detail["risk"], "side_effect");
        assert_eq!(detail["verified"], false);
        assert_eq!(detail["step_count"], 1);

        remove("myapp/daily").unwrap();
        assert!(remove("myapp/daily").is_err());
    }

    #[test]
    fn resolve_target_prefers_paths() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            resolve_target("./x.json").unwrap(),
            PathBuf::from("./x.json")
        );
        assert_eq!(
            resolve_target("/tmp/flow.json").unwrap(),
            PathBuf::from("/tmp/flow.json")
        );
        assert!(resolve_target("NotAnId")
            .unwrap_err()
            .to_string()
            .contains("registry id"));
    }

    #[tokio::test]
    async fn elements_hint_matches_foreground_app_by_alias() {
        let _env = setup(&[(
            "health/open",
            &flow_json("Open Health", r#""category":"health","#),
        )]);
        // Give the app entry aliases through the source index.
        let source = std::env::var(SOURCE_ENV).unwrap();
        let index_path = Path::new(&source).join("index.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
        index["apps"] = serde_json::json!([{"id":"health","name":"Health","aliases":["健康"]}]);
        fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
        update().await.unwrap();

        let body = r#"{"snapshot":"s","elements":[{"kind":"Application","label":"健康","rect":[0,0,1,1]}]}"#;
        let hint = elements_hint(body).unwrap();
        assert_eq!(hint["flows"][0]["id"], "health/open");
        assert_eq!(hint["application"], "健康");
        let other =
            elements_hint(r#"{"elements":[{"kind":"Application","label":"Mail"}]}"#).unwrap();
        assert_eq!(other["flows"].as_array().unwrap().len(), 0);
        assert!(other["hint"]
            .as_str()
            .unwrap()
            .contains("phone_flow_publish"));

        let attached = attach_hint(body.to_string(), "registry", Some(hint));
        let value: serde_json::Value = serde_json::from_str(&attached).unwrap();
        assert_eq!(value["registry"]["installed"], 1);
        assert_eq!(value["snapshot"], "s");
        assert_eq!(
            attach_hint("not json".into(), "registry", Some(serde_json::json!(1))),
            "not json"
        );
    }

    #[test]
    fn rfc3339_formatting_is_sane() {
        let stamp = now_rfc3339();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.starts_with("20"));
        assert!(stamp.ends_with('Z'));
    }
}
