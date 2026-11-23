//! Interactive onboarding wizard (`kerux wizard`) and model picker
//! (`kerux model`).
//!
//! The wizard walks a first-time user through provider detection, API key
//! validation (via a live model-list call, not format checks), model
//! selection with capability badges, optional live probing, an optional
//! fallback model, config writing, and a smoke test. Every step can be
//! skipped or revisited; no endpoint failure dead-ends the flow (manual
//! model entry is always available).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use console::style;
use dialoguer::{Confirm, FuzzySelect, Input, Select};
use indicatif::ProgressBar;
use kerux_core::capability::classify;
use kerux_core::client::{
    build_provider_client, build_provider_for_kind, discover_models_or_empty,
    resolve_provider_settings, ClientConfig, LLMProvider, Message, ModelCache, ModelInfo,
    ProviderKind,
};
use kerux_core::config::load_app_config;
use kerux_core::probe::{probe_model, ProbeResult};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

/// Timeout for the validation/model-list fetch during onboarding.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-probe timeout for the live capability probe step.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for the smoke-test round trip.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Environment detection
// ---------------------------------------------------------------------------

/// A provider credential/host discovered in the environment.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvDetection {
    pub kind: ProviderKind,
    pub env_var: String,
    /// API key, or host URL for Ollama.
    pub value: String,
    /// `true` when `value` is a host URL (OLLAMA_HOST), not an API key.
    pub is_host: bool,
}

/// Sniff well-known environment variables for a usable provider.
pub fn detect_env_provider() -> Option<EnvDetection> {
    detect_env_provider_from(&|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

/// Pure variant of [`detect_env_provider`] for tests. Priority order:
/// OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, OPENROUTER_API_KEY,
/// OLLAMA_HOST (then OLLAMA_BASE_URL).
pub fn detect_env_provider_from(lookup: &dyn Fn(&str) -> Option<String>) -> Option<EnvDetection> {
    if let Some(value) = lookup("OPENAI_API_KEY") {
        return Some(EnvDetection {
            kind: ProviderKind::Openai,
            env_var: "OPENAI_API_KEY".into(),
            value,
            is_host: false,
        });
    }
    if let Some(value) = lookup("ANTHROPIC_API_KEY") {
        return Some(EnvDetection {
            kind: ProviderKind::Anthropic,
            env_var: "ANTHROPIC_API_KEY".into(),
            value,
            is_host: false,
        });
    }
    if let Some(value) = lookup("GEMINI_API_KEY") {
        return Some(EnvDetection {
            kind: ProviderKind::Gemini,
            env_var: "GEMINI_API_KEY".into(),
            value,
            is_host: false,
        });
    }
    if let Some(value) = lookup("OPENROUTER_API_KEY") {
        return Some(EnvDetection {
            kind: ProviderKind::Openrouter,
            env_var: "OPENROUTER_API_KEY".into(),
            value,
            is_host: false,
        });
    }
    if let Some(value) = lookup("OLLAMA_HOST").or_else(|| lookup("OLLAMA_BASE_URL")) {
        return Some(EnvDetection {
            kind: ProviderKind::Ollama,
            env_var: "OLLAMA_HOST".into(),
            value,
            is_host: true,
        });
    }
    None
}

/// Mask a secret for display: keep a short prefix and suffix.
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.trim().chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

// ---------------------------------------------------------------------------
// Provider metadata
// ---------------------------------------------------------------------------

/// Provider choices offered by the wizard picker (Nous is OAuth-only and
/// configured through `kerux auth login nous`).
const PROVIDER_CHOICES: &[(&str, ProviderKind)] = &[
    (
        "OpenAI / OpenAI-compatible (custom base URL)",
        ProviderKind::Openai,
    ),
    ("Anthropic", ProviderKind::Anthropic),
    ("Google Gemini", ProviderKind::Gemini),
    ("Ollama (local)", ProviderKind::Ollama),
    ("OpenRouter", ProviderKind::Openrouter),
];

/// Default endpoint for each provider kind (mirrors
/// `kerux_core::client::resolve_provider_settings`).
pub fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Openai => "https://api.openai.com/v1",
        ProviderKind::Ollama => "http://localhost:11434/v1",
        ProviderKind::Openrouter => "https://openrouter.ai/api/v1",
        ProviderKind::Anthropic => "https://api.anthropic.com/v1",
        ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        ProviderKind::Nous => kerux_core::auth::NOUS_INFERENCE_URL,
    }
}

/// Whether an HTTP failure message indicates a rejected credential.
pub fn is_auth_error(message: &str) -> bool {
    message.contains("HTTP 401") || message.contains("HTTP 403")
}

// ---------------------------------------------------------------------------
// Model list rendering
// ---------------------------------------------------------------------------

/// Compact context-window formatting: 128000 -> "128k", 2000000 -> "2M".
pub fn format_context_window(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens % 1_000_000 == 0 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 && tokens % 1_000 == 0 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// One fuzzy-select row: model id, capability badges, context window.
pub fn model_entry_label(info: &ModelInfo) -> String {
    let report = classify(info);
    let badges = report.badges();
    let mut label = info.id.clone();
    if !badges.is_empty() {
        label.push(' ');
        label.push_str(&badges.iter().map(|b| format!("[{b}]")).collect::<String>());
    }
    if let Some(ctx) = info.context_window {
        label.push_str(&format!("  ctx {}", format_context_window(ctx)));
    }
    label
}

/// Render all picker rows. Safe for thousands of models: FuzzySelect filters
/// as the user types.
pub fn model_entry_labels(models: &[ModelInfo]) -> Vec<String> {
    models.iter().map(model_entry_label).collect()
}

// ---------------------------------------------------------------------------
// Probe + smoke rendering
// ---------------------------------------------------------------------------

fn verdict_mark(verdict: Option<bool>) -> String {
    match verdict {
        Some(true) => style("✓ verified").green().to_string(),
        Some(false) => style("✗ unsupported").red().to_string(),
        None => style("- not tested").dim().to_string(),
    }
}

/// Human-readable probe result lines.
pub fn probe_summary_lines(result: &ProbeResult) -> Vec<String> {
    let ttft = result
        .ttft_ms
        .map(|ms| format!(" (TTFT {ms}ms)"))
        .unwrap_or_default();
    vec![
        format!("  Streaming  {}{ttft}", verdict_mark(result.streaming)),
        format!("  Tools      {}", verdict_mark(result.tools)),
        format!("  Vision     {}", verdict_mark(result.vision)),
    ]
}

/// Smoke-test round-trip outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct SmokeOutcome {
    pub reply: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Render the smoke-test result.
pub fn format_smoke(outcome: &SmokeOutcome) -> String {
    format!(
        "{}\n  tokens: {} prompt + {} completion = {} total",
        outcome.reply.trim(),
        outcome.prompt_tokens,
        outcome.completion_tokens,
        outcome.total_tokens
    )
}

/// Run a cheap round trip ("Say hi") against the chosen model.
pub async fn smoke_test(provider: &dyn LLMProvider, model: &str) -> Result<SmokeOutcome> {
    let messages = [Message::user("Say hi")];
    let response = tokio::time::timeout(SMOKE_TIMEOUT, provider.chat(model, &messages, None))
        .await
        .context("smoke test timed out")?
        .map_err(|error| anyhow!(error.to_string()))?;
    let reply = response
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone())
        .unwrap_or_default();
    Ok(SmokeOutcome {
        reply,
        prompt_tokens: response.usage.prompt_tokens,
        completion_tokens: response.usage.completion_tokens,
        total_tokens: response.usage.total_tokens,
    })
}

// ---------------------------------------------------------------------------
// Config path + writing
// ---------------------------------------------------------------------------

/// Where the wizard writes the config: `$KERUX_HOME/config.toml` when
/// KERUX_HOME is set, otherwise the global config path kerux reads by
/// default (`<config_dir>/kerux/config.toml`).
pub fn wizard_config_path() -> PathBuf {
    wizard_config_path_from(
        std::env::var("KERUX_HOME").ok().as_deref(),
        dirs::config_dir(),
    )
}

/// Pure variant of [`wizard_config_path`] for tests.
pub fn wizard_config_path_from(kerux_home: Option<&str>, config_dir: Option<PathBuf>) -> PathBuf {
    if let Some(home) = kerux_home.map(str::trim).filter(|home| !home.is_empty()) {
        return PathBuf::from(home).join("config.toml");
    }
    config_dir
        .map(|dir| dir.join("kerux").join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("kerux.toml"))
}

/// Everything the wizard collected and is about to persist.
#[derive(Debug, Clone)]
pub struct WizardPlan {
    pub provider: ProviderKind,
    pub base_url: String,
    /// Pasted key. `None` means the key comes from the environment (or is
    /// not needed for Ollama) and must NOT be written to the file.
    pub api_key: Option<String>,
    pub model: String,
    pub fallback: Option<FallbackChoice>,
    pub toolset_preset: Option<String>,
    pub web_search_url: Option<String>,
    pub budget_daily_limit: Option<f64>,
}

/// One `[[client.fallback]]` entry picked in the wizard.
#[derive(Debug, Clone)]
pub struct FallbackChoice {
    pub provider: ProviderKind,
    /// Always written: fallback resolution does not apply per-provider
    /// default URLs the way the primary `[client]` block does.
    pub base_url: String,
    /// Always written when present: fallback entries never read env vars.
    pub api_key: Option<String>,
    pub model: String,
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("moving {} into place", tmp.display()))?;
    Ok(())
}

/// Write (or update) the config file from a wizard plan. Existing content is
/// preserved and edited in place via TOML parsing; a missing file starts
/// from an empty document.
pub fn write_wizard_config(path: &Path, plan: &WizardPlan, replace_fallback: bool) -> Result<()> {
    let mut doc = load_document(path)?;

    let client = ensure_table(&mut doc, "client")?;
    client["provider"] = value(plan.provider.as_str());
    if plan.provider == ProviderKind::Openai {
        client["base_url"] = value(&plan.base_url);
        if let Some(key) = &plan.api_key {
            client["api_key"] = value(key);
        }
    } else {
        // Per-provider subtable: top-level base_url/api_key only feed the
        // OpenAI-compatible path in ClientSettings resolution.
        let sub = ensure_subtable(client, plan.provider.as_str())?;
        sub["base_url"] = value(&plan.base_url);
        if let Some(key) = &plan.api_key {
            sub["api_key"] = value(key);
        }
    }
    if let Some(fallback) = &plan.fallback {
        upsert_fallback(client, fallback, replace_fallback);
    }

    let agent = ensure_table(&mut doc, "agent")?;
    agent["model"] = value(&plan.model);

    if let Some(preset) = &plan.toolset_preset {
        let tools = ensure_table(&mut doc, "tools")?;
        match preset.as_str() {
            "safe" => {
                let term = ensure_subtable(tools, "terminal")?;
                term["max_timeout_secs"] = value(0);
                let code = ensure_subtable(tools, "code_execution")?;
                code["default_timeout_secs"] = value(0);
                code["max_timeout_secs"] = value(0);
            }
            "coding" => {
                let term = ensure_subtable(tools, "terminal")?;
                term["max_timeout_secs"] = value(300);
            }
            _ => {}
        }
    }

    if let Some(search_url) = &plan.web_search_url {
        let tools = ensure_table(&mut doc, "tools")?;
        let web = ensure_subtable(tools, "web")?;
        web["search_url"] = value(search_url);
    }

    if let Some(daily) = plan.budget_daily_limit {
        let budget = ensure_table(&mut doc, "budget")?;
        budget["enabled"] = value(true);
        budget["daily_limit"] = value(daily);
    }

    atomic_write(path, &doc.to_string())
}

fn load_document(path: &Path) -> Result<DocumentMut> {
    if path.exists() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        raw.parse::<DocumentMut>()
            .with_context(|| format!("parsing config {}", path.display()))
    } else {
        Ok(DocumentMut::new())
    }
}

/// Get or create a standard (non-inline) root table. Inline tables are
/// converted in place so entries keep rendering as `[section]` headers.
fn ensure_table<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table> {
    let item = doc.entry(key).or_insert_with(|| Item::Table(Table::new()));
    inline_to_standard_table(item);
    item.as_table_mut()
        .ok_or_else(|| anyhow!("config key '{key}' is not a table"))
}

/// Get or create a standard subtable under an existing table.
fn ensure_subtable<'a>(parent: &'a mut Table, key: &str) -> Result<&'a mut Table> {
    let item = parent
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()));
    inline_to_standard_table(item);
    item.as_table_mut()
        .ok_or_else(|| anyhow!("config key '{key}' is not a table"))
}

fn inline_to_standard_table(item: &mut Item) {
    if let Item::Value(value) = item {
        if let Some(inline) = value.as_inline_table() {
            let mut table = Table::new();
            for (key, entry) in inline.iter() {
                table.insert(key, Item::Value(entry.clone()));
            }
            *item = Item::Table(table);
        }
    }
}

fn upsert_fallback(client: &mut Table, fallback: &FallbackChoice, replace: bool) {
    let existing = client.get("fallback").and_then(Item::as_array_of_tables);
    let keep_existing = !replace && existing.is_some();
    if !keep_existing {
        client["fallback"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let Some(arr) = client["fallback"].as_array_of_tables_mut() else {
        return;
    };

    let mut entry = Table::new();
    entry["provider"] = value(fallback.provider.as_str());
    entry["base_url"] = value(&fallback.base_url);
    if let Some(key) = &fallback.api_key {
        entry["api_key"] = value(key);
    }
    entry["model"] = value(&fallback.model);
    arr.push(entry);
}

/// Update only `[agent] model`, preserving everything else in the file.
pub fn write_model_to_config(path: &Path, model: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let agent = ensure_table(&mut doc, "agent")?;
    agent["model"] = value(model);
    atomic_write(path, &doc.to_string())
}

// ---------------------------------------------------------------------------
// Discovery helpers
// ---------------------------------------------------------------------------

pub fn build_plan_provider(
    kind: ProviderKind,
    base_url: &str,
    api_key: Option<String>,
) -> Result<Arc<dyn LLMProvider>> {
    let config = ClientConfig {
        base_url: base_url.to_string(),
        api_key,
        timeout: Duration::from_secs(60),
        max_context_length: 128_000,
    };
    build_provider_for_kind(kind, config).map_err(|error| anyhow!(error.to_string()))
}

/// Await a future while an indicatif spinner animates on its own thread.
async fn with_spinner<T>(message: &str, future: impl std::future::Future<Output = T>) -> T {
    let pb = ProgressBar::new_spinner();
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    let output = future.await;
    pb.finish_and_clear();
    output
}

async fn discover(
    provider: &dyn LLMProvider,
    kind: ProviderKind,
    endpoint: &str,
    timeout: Duration,
    force_refresh: bool,
) -> (Vec<ModelInfo>, Option<String>) {
    let cache = ModelCache::default_location();
    with_spinner(
        "Fetching model list…",
        discover_models_or_empty(provider, &cache, kind, endpoint, force_refresh, timeout),
    )
    .await
}

const MANUAL_ENTRY: &str = "✍ Enter a model id manually (type instead of picking)";

/// Fuzzy model picker over a discovered list. Returns `None` when the user
/// backs out (Esc).
fn pick_model(models: &[ModelInfo], prompt: &str, current: Option<&str>) -> Result<Option<String>> {
    let mut labels = model_entry_labels(models);
    labels.push(MANUAL_ENTRY.to_string());
    let default = current
        .and_then(|id| models.iter().position(|model| model.id == *id))
        .unwrap_or(0);
    let selection = FuzzySelect::new()
        .with_prompt(format!("{prompt} (type to filter, Esc to skip)"))
        .items(&labels)
        .default(default)
        .interact_opt()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    let Some(index) = selection else {
        return Ok(None);
    };
    if index >= models.len() {
        return manual_model_input();
    }
    Ok(Some(models[index].id.clone()))
}

fn manual_model_input() -> Result<Option<String>> {
    let input: String = Input::new()
        .with_prompt("Model id (blank = back)")
        .allow_empty(true)
        .interact_text()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    Ok(Some(input.trim().to_string()).filter(|v| !v.is_empty()))
}

/// Whether the environment already carries provider credentials, so a
/// missing config file does not force the wizard on env-only setups.
pub fn env_has_credentials() -> bool {
    detect_env_provider().is_some()
        || std::env::var("KERUX_AUTH_REF")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .is_some()
}

// ---------------------------------------------------------------------------
// Wizard flow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    Provider,
    Endpoint,
    ApiKey,
    Model,
    Probe,
    Fallback,
    Write,
    Smoke,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Move {
    Forward,
    Back,
    Abort,
}

fn after_provider(kind: ProviderKind) -> Step {
    match kind {
        ProviderKind::Openai | ProviderKind::Ollama => Step::Endpoint,
        _ => Step::ApiKey,
    }
}

fn after_endpoint(kind: ProviderKind) -> Step {
    if kind == ProviderKind::Ollama {
        Step::Model
    } else {
        Step::ApiKey
    }
}

fn retreat(from: Step, kind: ProviderKind) -> Step {
    match from {
        Step::Endpoint => Step::Provider,
        Step::ApiKey => {
            if matches!(kind, ProviderKind::Openai) {
                Step::Endpoint
            } else {
                Step::Provider
            }
        }
        Step::Model => {
            // Reverse of the forward flow: Ollama skips the API key step.
            if kind == ProviderKind::Ollama {
                Step::Endpoint
            } else {
                Step::ApiKey
            }
        }
        Step::Probe => Step::Model,
        Step::Fallback => Step::Probe,
        Step::Write => Step::Fallback,
        Step::Smoke => Step::Write,
        Step::Provider | Step::Done => Step::Provider,
    }
}

/// Mutable state gathered while walking the wizard.
#[derive(Debug, Clone, Default)]
struct WizardState {
    kind: Option<ProviderKind>,
    base_url: Option<String>,
    api_key: Option<String>,
    key_source: Option<String>,
    model: Option<String>,
    fallback: Option<FallbackChoice>,
}

/// Run the full onboarding wizard. Returns `true` when a config was written,
/// `false` when the user cancelled before writing.
pub async fn run_wizard() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("The wizard needs an interactive terminal. Copy kerux.example.toml to kerux.toml and edit it manually.");
    }

    let target = wizard_config_path();
    if target.exists() {
        let again = Confirm::new()
            .with_prompt(format!(
                "Found existing config at {}. Re-run setup?",
                target.display()
            ))
            .default(false)
            .interact_opt()
            .map_err(|error| anyhow!("prompt failed: {error}"))?;
        if !again.unwrap_or(false) {
            println!("Leaving the existing config untouched.");
            return Ok(false);
        }
    }

    let mut wizard = crate::tui_wizard::FullscreenWizard::enter()?;
    let res = wizard.run().await;
    let _ = wizard.exit();
    res
}

/// Fallback legacy CLI-based wizard if TUI is not requested.
#[allow(dead_code)]
pub async fn run_wizard_cli() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("The wizard needs an interactive terminal. Copy kerux.example.toml to kerux.toml and edit it manually.");
    }

    let target = wizard_config_path();
    let mut state = WizardState::default();
    let mut step = Step::Provider;

    loop {
        let kind = state.kind.unwrap_or(ProviderKind::Openai);
        let move_ = match step {
            Step::Provider => provider_step(&mut state).await?,
            Step::Endpoint => endpoint_step(&mut state).await?,
            Step::ApiKey => api_key_step(&mut state).await?,
            Step::Model => model_step(&mut state).await?,
            Step::Probe => probe_step(&mut state).await?,
            Step::Fallback => fallback_step(&mut state).await?,
            Step::Write => write_step(&mut state, &target).await?,
            Step::Smoke => smoke_step(&mut state).await?,
            Step::Done => break,
        };
        step = match move_ {
            Move::Forward => match step {
                Step::Provider => after_provider(kind),
                Step::Endpoint => after_endpoint(kind),
                Step::ApiKey => Step::Model,
                Step::Model => Step::Probe,
                Step::Probe => Step::Fallback,
                Step::Fallback => Step::Write,
                Step::Write => Step::Smoke,
                Step::Smoke => Step::Done,
                Step::Done => Step::Done,
            },
            Move::Back => retreat(step, kind),
            Move::Abort => {
                println!(
                    "{}",
                    style("Setup cancelled. Run `kerux wizard` anytime to restart.").yellow()
                );
                return Ok(false);
            }
        };
    }

    println!();
    println!(
        "{} Setup complete. Try: {}",
        style("✓").green(),
        style("kerux chat").cyan()
    );
    Ok(true)
}

fn step_header(number: u8, title: &str) {
    println!();
    println!("{}", style(format!("Step {number}/6 — {title}")).bold());
}

async fn provider_step(state: &mut WizardState) -> Result<Move> {
    step_header(1, "Provider");

    if let Some(detected) = detect_env_provider() {
        let shown = if detected.is_host {
            detected.value.clone()
        } else {
            mask_secret(&detected.value)
        };
        let prompt = format!(
            "Detected {} ({}) from environment. Use it?",
            detected.env_var, shown
        );
        match Confirm::new()
            .with_prompt(prompt)
            .default(true)
            .interact_opt()
            .map_err(|error| anyhow!("prompt failed: {error}"))?
        {
            Some(true) => {
                state.kind = Some(detected.kind);
                state.base_url = Some(if detected.is_host {
                    normalize_ollama_host(&detected.value)
                } else {
                    default_base_url(detected.kind).to_string()
                });
                if detected.is_host {
                    state.key_source = Some(format!("{} (no key needed)", detected.env_var));
                } else {
                    state.api_key = Some(detected.value.clone());
                    state.key_source = Some(format!("environment ({})", detected.env_var));
                }
                println!(
                    "  {} {}",
                    style("→").green(),
                    style(detected.kind.as_str()).cyan()
                );
                return Ok(Move::Forward);
            }
            Some(false) => {}
            None => return Ok(Move::Abort),
        }
    }

    let labels: Vec<&str> = PROVIDER_CHOICES.iter().map(|(label, _)| *label).collect();
    let selection = Select::new()
        .with_prompt("Which provider do you want to use?")
        .items(&labels)
        .default(0)
        .interact_opt()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    let Some(index) = selection else {
        return Ok(Move::Abort);
    };
    let kind = PROVIDER_CHOICES[index].1;
    state.kind = Some(kind);
    state.base_url = Some(default_base_url(kind).to_string());
    state.api_key = None;
    state.key_source = None;
    Ok(Move::Forward)
}

/// OLLAMA_HOST may be a bare `host:port`; the client expects a full URL with
/// the `/v1` suffix.
fn normalize_ollama_host(host: &str) -> String {
    let trimmed = host.trim().trim_end_matches('/');
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    if with_scheme.ends_with("/v1") {
        with_scheme
    } else {
        format!("{with_scheme}/v1")
    }
}

async fn endpoint_step(state: &mut WizardState) -> Result<Move> {
    step_header(1, "Provider endpoint");
    let kind = state.kind.unwrap_or(ProviderKind::Openai);
    let current = state.base_url.clone().unwrap_or_default();
    let prompt = match kind {
        ProviderKind::Openai => "Base URL (change this for OpenAI-compatible/custom servers)",
        ProviderKind::Ollama => "Ollama server URL",
        _ => "Base URL",
    };
    let input: String = Input::new()
        .with_prompt(format!("{prompt} (blank = back)"))
        .default(current)
        .allow_empty(true)
        .interact_text()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    if input.trim().is_empty() {
        return Ok(Move::Back);
    }
    let value = input.trim().trim_end_matches('/').to_string();
    state.base_url = Some(match kind {
        ProviderKind::Ollama => normalize_ollama_host(&value),
        _ => value,
    });
    Ok(Move::Forward)
}

async fn api_key_step(state: &mut WizardState) -> Result<Move> {
    let kind = state.kind.unwrap_or(ProviderKind::Openai);

    // A key from the environment for THIS provider was already confirmed in
    // the provider step (or the provider needs no key at all).
    if state.api_key.is_some() || kind == ProviderKind::Ollama {
        return Ok(Move::Forward);
    }

    step_header(2, "API key");

    if let Some(detected) = detect_env_provider() {
        if detected.kind == kind && !detected.is_host {
            let prompt = format!(
                "Use {} ({}) from environment?",
                detected.env_var,
                mask_secret(&detected.value)
            );
            match Confirm::new()
                .with_prompt(prompt)
                .default(true)
                .interact_opt()
                .map_err(|error| anyhow!("prompt failed: {error}"))?
            {
                Some(true) => {
                    state.api_key = Some(detected.value.clone());
                    state.key_source = Some(format!("environment ({})", detected.env_var));
                    return Ok(Move::Forward);
                }
                Some(false) => {}
                None => return Ok(Move::Back),
            }
        }
    }

    let input: String = Input::new()
        .with_prompt(format!("API key for {} (blank = back)", kind.as_str()))
        .allow_empty(true)
        .interact_text()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    match Some(input.trim().to_string()).filter(|v| !v.is_empty()) {
        Some(key) => {
            state.api_key = Some(key);
            state.key_source = Some("pasted (will be stored in config)".to_string());
            Ok(Move::Forward)
        }
        None => Ok(Move::Back),
    }
}

async fn model_step(state: &mut WizardState) -> Result<Move> {
    step_header(3, "Model");
    let kind = state.kind.unwrap_or(ProviderKind::Openai);
    let base_url = state.base_url.clone().unwrap_or_default();

    loop {
        let provider = build_plan_provider(kind, &base_url, state.api_key.clone())?;
        let (models, error) = discover(&*provider, kind, &base_url, DISCOVER_TIMEOUT, false).await;

        if models.is_empty() {
            let detail = error.clone().unwrap_or_else(|| "empty model list".into());
            println!(
                "{} could not fetch the model list: {}",
                style("⚠").yellow(),
                detail
            );
            if is_auth_error(&detail) {
                println!(
                    "{}",
                    style("The API key was rejected. Go back and check it.").red()
                );
                return Ok(Move::Back);
            }
            let retry = Confirm::new()
                .with_prompt("Try fetching again?")
                .default(false)
                .interact_opt()
                .map_err(|error| anyhow!("prompt failed: {error}"))?;
            if retry.unwrap_or(false) {
                continue;
            }
            match manual_model_input()? {
                Some(model) => {
                    state.model = Some(model);
                    return Ok(Move::Forward);
                }
                None => return Ok(Move::Back),
            }
        }

        println!(
            "  {} {} models available.",
            style("✓").green(),
            models.len()
        );
        match pick_model(&models, "Pick a model", state.model.as_deref())? {
            Some(model) => {
                state.model = Some(model);
                return Ok(Move::Forward);
            }
            None => match manual_model_input()? {
                Some(model) => {
                    state.model = Some(model);
                    return Ok(Move::Forward);
                }
                None => return Ok(Move::Back),
            },
        }
    }
}

async fn probe_step(state: &mut WizardState) -> Result<Move> {
    step_header(4, "Probe capabilities (optional)");
    let model = state.model.clone().unwrap_or_default();
    let want = Confirm::new()
        .with_prompt(format!(
            "Probe '{model}' now? Sends 3 tiny requests (streaming, tool call, vision) to verify what it really supports"
        ))
        .default(true)
        .interact_opt()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    if !want.unwrap_or(false) {
        return Ok(Move::Forward);
    }

    let kind = state.kind.unwrap_or(ProviderKind::Openai);
    let base_url = state.base_url.clone().unwrap_or_default();
    let provider = build_plan_provider(kind, &base_url, state.api_key.clone())?;
    let result = with_spinner(
        &format!("Probing {model}…"),
        probe_model(&*provider, &model, PROBE_TIMEOUT),
    )
    .await;
    for line in probe_summary_lines(&result) {
        println!("{line}");
    }
    Ok(Move::Forward)
}

async fn fallback_step(state: &mut WizardState) -> Result<Move> {
    step_header(5, "Fallback model (optional)");
    let want = Confirm::new()
        .with_prompt("Add a fallback model? Kerux switches to it when the primary provider fails (rate limit, outage)")
        .default(false)
        .interact_opt()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    if !want.unwrap_or(false) {
        state.fallback = None;
        return Ok(Move::Forward);
    }

    match pick_fallback().await? {
        Some(choice) => {
            state.fallback = Some(choice);
            Ok(Move::Forward)
        }
        None => Ok(Move::Back),
    }
}

async fn pick_fallback() -> Result<Option<FallbackChoice>> {
    let labels: Vec<&str> = PROVIDER_CHOICES.iter().map(|(label, _)| *label).collect();
    let selection = Select::new()
        .with_prompt("Fallback provider [Esc = skip]")
        .items(&labels)
        .default(0)
        .interact_opt()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    let Some(index) = selection else {
        return Ok(None);
    };
    let kind = PROVIDER_CHOICES[index].1;
    let mut base_url = default_base_url(kind).to_string();

    if kind == ProviderKind::Openai {
        let url: String = Input::new()
            .with_prompt("Fallback base URL (blank = skip)")
            .default(base_url.clone())
            .allow_empty(true)
            .interact_text()
            .map_err(|error| anyhow!("prompt failed: {error}"))?;
        if url.trim().is_empty() {
            return Ok(None);
        }
        base_url = url.trim().trim_end_matches('/').to_string();
    }

    // Fallback entries never read env vars at runtime, so a key must end up
    // in the config file for the fallback to work.
    let api_key = if kind == ProviderKind::Ollama {
        None
    } else {
        let env_match = detect_env_provider().filter(|d| d.kind == kind && !d.is_host);
        if let Some(detected) = env_match {
            let prompt = format!(
                "Write the key from {} ({}) into the config for the fallback? (fallbacks don't read env vars)",
                detected.env_var,
                mask_secret(&detected.value)
            );
            match Confirm::new()
                .with_prompt(prompt)
                .default(true)
                .interact_opt()
                .map_err(|error| anyhow!("prompt failed: {error}"))?
            {
                Some(true) => Some(detected.value.clone()),
                Some(false) => {
                    let input: String = Input::new()
                        .with_prompt("Fallback API key (blank = skip)")
                        .allow_empty(true)
                        .interact_text()
                        .map_err(|error| anyhow!("prompt failed: {error}"))?;
                    match Some(input.trim().to_string()).filter(|v| !v.is_empty()) {
                        Some(key) => Some(key),
                        None => return Ok(None),
                    }
                }
                None => return Ok(None),
            }
        } else {
            let input: String = Input::new()
                .with_prompt(format!(
                    "Fallback API key for {} (blank = skip)",
                    kind.as_str()
                ))
                .allow_empty(true)
                .interact_text()
                .map_err(|error| anyhow!("prompt failed: {error}"))?;
            match Some(input.trim().to_string()).filter(|v| !v.is_empty()) {
                Some(key) => Some(key),
                None => return Ok(None),
            }
        }
    };

    let provider = build_plan_provider(kind, &base_url, api_key.clone())?;
    let (models, error) = discover(&*provider, kind, &base_url, DISCOVER_TIMEOUT, false).await;
    let model = if models.is_empty() {
        if let Some(detail) = error {
            println!(
                "{} fallback model list unavailable: {detail}",
                style("⚠").yellow()
            );
        }
        match manual_model_input()? {
            Some(model) => model,
            None => return Ok(None),
        }
    } else {
        match pick_model(&models, "Pick a fallback model", None)? {
            Some(model) => model,
            None => match manual_model_input()? {
                Some(model) => model,
                None => return Ok(None),
            },
        }
    };

    Ok(Some(FallbackChoice {
        provider: kind,
        base_url,
        api_key,
        model,
    }))
}

async fn write_step(state: &mut WizardState, target: &Path) -> Result<Move> {
    step_header(6, "Write config");
    let kind = state.kind.unwrap_or(ProviderKind::Openai);
    let key_shown = match (&state.api_key, &state.key_source) {
        (Some(_), Some(source)) if source.starts_with("environment") => {
            format!("from {source} — not written to file")
        }
        (Some(key), _) => format!("{} (stored in config)", mask_secret(key)),
        (None, _) => "not needed".to_string(),
    };
    println!("  Provider : {}", style(kind.as_str()).cyan());
    println!(
        "  Endpoint : {}",
        state.base_url.as_deref().unwrap_or_default()
    );
    println!("  API key  : {key_shown}");
    println!(
        "  Model    : {}",
        style(state.model.as_deref().unwrap_or_default()).cyan()
    );
    if let Some(fallback) = &state.fallback {
        println!(
            "  Fallback : {} / {}",
            fallback.provider.as_str(),
            fallback.model
        );
    }
    println!("  Config   : {}", target.display());

    let mut replace_fallback = false;
    if state.fallback.is_some() && target.exists() {
        if let Ok(raw) = tokio::fs::read_to_string(target).await {
            if let Ok(doc) = raw.parse::<DocumentMut>() {
                let count = doc
                    .get("client")
                    .and_then(Item::as_table)
                    .and_then(|t| t.get("fallback"))
                    .and_then(Item::as_array_of_tables)
                    .map(|arr| arr.len())
                    .unwrap_or(0);
                if count > 0 {
                    replace_fallback = Confirm::new()
                        .with_prompt(format!(
                            "Config already has {count} fallback entr{}. Replace with the one picked here?",
                            if count == 1 { "y" } else { "ies" }
                        ))
                        .default(false)
                        .interact_opt()
                        .map_err(|error| anyhow!("prompt failed: {error}"))?
                        .unwrap_or(false);
                }
            }
        }
    }

    let confirm = Confirm::new()
        .with_prompt(format!("Write config to {}?", target.display()))
        .default(true)
        .interact_opt()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    if !confirm.unwrap_or(false) {
        return Ok(Move::Back);
    }

    let plan = WizardPlan {
        provider: kind,
        base_url: state.base_url.clone().unwrap_or_default(),
        api_key: match &state.key_source {
            Some(source) if source.starts_with("environment") => None,
            _ => state.api_key.clone(),
        },
        model: state.model.clone().unwrap_or_default(),
        fallback: state.fallback.clone(),
        toolset_preset: None,
        web_search_url: None,
        budget_daily_limit: None,
    };
    write_wizard_config(target, &plan, replace_fallback)?;
    println!(
        "{} Config written to {}",
        style("✓").green(),
        target.display()
    );
    Ok(Move::Forward)
}

async fn smoke_step(state: &mut WizardState) -> Result<Move> {
    let model = state.model.clone().unwrap_or_default();
    let want = Confirm::new()
        .with_prompt(format!("Run a smoke test ('say hi') against {model}?"))
        .default(true)
        .interact_opt()
        .map_err(|error| anyhow!("prompt failed: {error}"))?;
    if !want.unwrap_or(false) {
        return Ok(Move::Forward);
    }

    let kind = state.kind.unwrap_or(ProviderKind::Openai);
    let base_url = state.base_url.clone().unwrap_or_default();
    let provider = build_plan_provider(kind, &base_url, state.api_key.clone())?;
    match smoke_test(&*provider, &model).await {
        Ok(outcome) => {
            println!("{} Smoke test passed:", style("✓").green());
            println!("  {}", format_smoke(&outcome).replace('\n', "\n  "));
        }
        Err(error) => {
            println!(
                "{} Smoke test failed: {error:#}. The config is written; check the model id or key.",
                style("⚠").yellow()
            );
        }
    }
    Ok(Move::Forward)
}

// ---------------------------------------------------------------------------
// kerux model
// ---------------------------------------------------------------------------

/// Quick model switch: fuzzy picker (or direct set) against the currently
/// configured provider, writing only `[agent] model` back to the config.
pub async fn switch_model(model_id: Option<String>, refresh: bool) -> Result<()> {
    let loaded = load_app_config(None).map_err(|error| anyhow!(error.to_string()))?;
    let settings =
        resolve_provider_settings(&loaded.config.client).map_err(|e| anyhow!(e.to_string()))?;
    let kind = settings.kind;
    let endpoint = settings.config.base_url.clone();

    let provider: Arc<dyn LLMProvider> = if loaded.config.client.auth_ref.is_some() {
        build_provider_client(&loaded.config.client).map_err(|e| anyhow!(e.to_string()))?
    } else {
        let config = ClientConfig {
            base_url: settings.config.base_url.clone(),
            api_key: settings.config.api_key.clone(),
            timeout: settings.config.timeout,
            max_context_length: settings.max_context_length,
        };
        build_provider_for_kind(kind, config).map_err(|e| anyhow!(e.to_string()))?
    };

    let model = match model_id {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            if !std::io::stdin().is_terminal() {
                bail!("Pass a model id ('kerux model <id>') — the picker needs an interactive terminal.");
            }
            let timeout = Duration::from_secs(loaded.config.client.model_list_timeout_secs);
            let (models, error) = discover(&*provider, kind, &endpoint, timeout, refresh).await;
            if models.is_empty() {
                let detail = error.unwrap_or_else(|| "empty model list".into());
                bail!(
                    "Could not fetch the model list for '{}': {detail}",
                    kind.as_str()
                );
            }
            println!(
                "{} models on {} ({})",
                models.len(),
                kind.as_str(),
                endpoint
            );
            match pick_model(&models, "Pick a model", Some(&loaded.config.agent.model))? {
                Some(model) => model,
                None => {
                    println!("No model selected; config unchanged.");
                    return Ok(());
                }
            }
        }
    };

    let path = loaded.source.clone().unwrap_or_else(wizard_config_path);
    write_model_to_config(&path, &model)?;
    println!(
        "{} Model set to {}",
        style("✓").green(),
        style(&model).cyan()
    );
    println!("  config: {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kerux_core::config::parse_config_file;

    fn fixture_model(index: usize) -> ModelInfo {
        ModelInfo {
            id: format!("model-{index:04}"),
            display_name: format!("Model {index}"),
            context_window: Some(128_000),
            input_modalities: vec!["text".into()],
            output_modalities: vec!["text".into()],
            pricing: None,
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn env_detection_follows_priority_order() {
        let all = |name: &str| -> Option<String> { Some(format!("value-{name}")) };
        let detected = detect_env_provider_from(&all).unwrap();
        assert_eq!(detected.kind, ProviderKind::Openai);
        assert_eq!(detected.env_var, "OPENAI_API_KEY");
        assert!(!detected.is_host);

        let only_anthropic =
            |name: &str| (name == "ANTHROPIC_API_KEY").then(|| "sk-ant".to_string());
        let detected = detect_env_provider_from(&only_anthropic).unwrap();
        assert_eq!(detected.kind, ProviderKind::Anthropic);

        let only_gemini = |name: &str| (name == "GEMINI_API_KEY").then(|| "g".to_string());
        assert_eq!(
            detect_env_provider_from(&only_gemini).unwrap().kind,
            ProviderKind::Gemini
        );

        let only_openrouter = |name: &str| (name == "OPENROUTER_API_KEY").then(|| "or".to_string());
        assert_eq!(
            detect_env_provider_from(&only_openrouter).unwrap().kind,
            ProviderKind::Openrouter
        );
    }

    #[test]
    fn env_detection_ollama_host_marks_host_value() {
        let lookup = |name: &str| (name == "OLLAMA_HOST").then(|| "10.0.0.5:11434".to_string());
        let detected = detect_env_provider_from(&lookup).unwrap();
        assert_eq!(detected.kind, ProviderKind::Ollama);
        assert!(detected.is_host);
        assert_eq!(detected.value, "10.0.0.5:11434");
    }

    #[test]
    fn env_detection_none_when_nothing_set() {
        let none = |_name: &str| None;
        assert!(detect_env_provider_from(&none).is_none());
    }

    #[test]
    fn mask_secret_keeps_prefix_and_suffix() {
        assert_eq!(mask_secret("sk-ant-abcdef123456"), "sk-…3456");
        assert_eq!(mask_secret("short"), "****");
        assert_eq!(mask_secret("12345678"), "****");
    }

    #[test]
    fn format_context_window_scales() {
        assert_eq!(format_context_window(128_000), "128k");
        assert_eq!(format_context_window(2_000_000), "2M");
        assert_eq!(format_context_window(999), "999");
    }

    #[test]
    fn renders_1000_model_entries_for_picker() {
        let models: Vec<ModelInfo> = (0..1000).map(fixture_model).collect();
        let labels = model_entry_labels(&models);
        assert_eq!(labels.len(), 1000);
        assert!(labels[0].starts_with("model-0000"));
        assert!(labels[0].contains("ctx 128k"));
        // Every row stays unique so FuzzySelect can address each model.
        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(unique.len(), 1000);
    }

    #[test]
    fn label_includes_capability_badges() {
        let mut model = fixture_model(1);
        model.input_modalities = vec!["image".into(), "text".into()];
        model.raw = serde_json::json!({ "supported_parameters": ["tools"] });
        let label = model_entry_label(&model);
        assert!(label.contains("[vision]"), "missing vision badge: {label}");
        assert!(label.contains("[tools]"), "missing tools badge: {label}");
        assert!(
            label.contains("[streaming]"),
            "missing streaming badge: {label}"
        );
    }

    #[test]
    fn classifies_auth_errors() {
        assert!(is_auth_error("HTTP 401: invalid api key"));
        assert!(is_auth_error("HTTP 403: forbidden"));
        assert!(!is_auth_error("HTTP 429: rate limited"));
        assert!(!is_auth_error("model list timed out after 60s"));
    }

    #[test]
    fn config_path_prefers_kerux_home() {
        let path = wizard_config_path_from(Some("/custom/kerux"), Some(PathBuf::from("/cfg")));
        assert_eq!(path, PathBuf::from("/custom/kerux/config.toml"));
        // Empty/whitespace KERUX_HOME falls through.
        let path = wizard_config_path_from(Some("  "), Some(PathBuf::from("/cfg")));
        assert_eq!(path, PathBuf::from("/cfg/kerux/config.toml"));
    }

    #[test]
    fn config_path_falls_back_to_config_dir() {
        let path = wizard_config_path_from(None, Some(PathBuf::from("/cfg")));
        assert_eq!(path, PathBuf::from("/cfg/kerux/config.toml"));
        let path = wizard_config_path_from(None, None);
        assert_eq!(path, PathBuf::from("kerux.toml"));
    }

    #[test]
    fn writes_tools_and_budget_guardrails_to_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let plan = WizardPlan {
            provider: ProviderKind::Openai,
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some("sk-test".into()),
            model: "gpt-4o".into(),
            fallback: None,
            toolset_preset: Some("full".into()),
            web_search_url: Some("https://lite.duckduckgo.com/lite/?q={query}".into()),
            budget_daily_limit: Some(15.0),
        };
        write_wizard_config(&path, &plan, false).unwrap();

        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.client.provider, "openai");
        assert_eq!(parsed.agent.model, "gpt-4o");
        assert!(parsed.budget.enabled);
        assert_eq!(parsed.budget.daily_limit, 15.0);
        assert_eq!(
            parsed.tools.web.search_url,
            "https://lite.duckduckgo.com/lite/?q={query}"
        );
    }

    #[test]
    fn writes_fresh_config_that_roundtrips_through_parser() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let plan = WizardPlan {
            provider: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: Some("sk-ant-test".into()),
            model: "claude-sonnet-4-20250514".into(),
            fallback: None,
            toolset_preset: None,
            web_search_url: None,
            budget_daily_limit: None,
        };
        write_wizard_config(&path, &plan, false).unwrap();

        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.client.provider, "anthropic");
        assert_eq!(
            parsed.client.anthropic.base_url.as_deref(),
            Some("https://api.anthropic.com/v1")
        );
        assert_eq!(
            parsed.client.anthropic.api_key.as_deref(),
            Some("sk-ant-test")
        );
        assert_eq!(parsed.agent.model, "claude-sonnet-4-20250514");
        assert!(parsed.client.fallback.is_empty());
    }

    #[test]
    fn openai_plan_writes_top_level_client_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let plan = WizardPlan {
            provider: ProviderKind::Openai,
            base_url: "https://my-llm.example.com/v1".into(),
            api_key: Some("sk-custom".into()),
            model: "my-model".into(),
            fallback: None,
            toolset_preset: None,
            web_search_url: None,
            budget_daily_limit: None,
        };
        write_wizard_config(&path, &plan, false).unwrap();

        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.client.provider, "openai");
        assert_eq!(parsed.client.base_url, "https://my-llm.example.com/v1");
        assert_eq!(parsed.client.api_key.as_deref(), Some("sk-custom"));
        assert_eq!(parsed.agent.model, "my-model");
    }

    #[test]
    fn env_key_is_not_written_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let plan = WizardPlan {
            provider: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: None, // key stays in the environment
            model: "claude-sonnet-4-20250514".into(),
            fallback: None,
            toolset_preset: None,
            web_search_url: None,
            budget_daily_limit: None,
        };
        write_wizard_config(&path, &plan, false).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("api_key"),
            "env key leaked into config: {raw}"
        );
        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.client.anthropic.api_key, None);
    }

    #[test]
    fn fallback_entry_wires_into_client_fallback_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let plan = WizardPlan {
            provider: ProviderKind::Openai,
            base_url: "https://api.openai.com/v1".into(),
            api_key: Some("sk-primary".into()),
            model: "gpt-4o".into(),
            fallback: Some(FallbackChoice {
                provider: ProviderKind::Openrouter,
                base_url: "https://openrouter.ai/api/v1".into(),
                api_key: Some("sk-or".into()),
                model: "openai/gpt-4o-mini".into(),
            }),
            toolset_preset: None,
            web_search_url: None,
            budget_daily_limit: None,
        };
        write_wizard_config(&path, &plan, false).unwrap();

        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.client.fallback.len(), 1);
        let fb = &parsed.client.fallback[0];
        assert_eq!(fb.provider, "openrouter");
        assert_eq!(fb.base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(fb.api_key.as_deref(), Some("sk-or"));
        assert_eq!(fb.model.as_deref(), Some("openai/gpt-4o-mini"));
    }

    #[test]
    fn fallback_replace_vs_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let plan = WizardPlan {
            provider: ProviderKind::Openai,
            base_url: "https://api.openai.com/v1".into(),
            api_key: None,
            model: "gpt-4o".into(),
            fallback: Some(FallbackChoice {
                provider: ProviderKind::Ollama,
                base_url: "http://localhost:11434/v1".into(),
                api_key: None,
                model: "llama3.1".into(),
            }),
            toolset_preset: None,
            web_search_url: None,
            budget_daily_limit: None,
        };
        write_wizard_config(&path, &plan, false).unwrap();

        // Append: second run with replace=false keeps both entries.
        write_wizard_config(&path, &plan, false).unwrap();
        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.client.fallback.len(), 2);

        // Replace: replace=true resets to the single new entry.
        write_wizard_config(&path, &plan, true).unwrap();
        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.client.fallback.len(), 1);
        assert_eq!(parsed.client.fallback[0].provider, "ollama");
    }

    #[test]
    fn write_model_preserves_rest_of_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# user comment\n[client]\nprovider = \"openai\"\n\n[agent]\nmodel = \"gpt-4\"\nmax_iterations = 7\n",
        )
        .unwrap();

        write_model_to_config(&path, "gpt-4o-mini").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# user comment"), "comment lost: {raw}");
        let parsed = parse_config_file(&path).unwrap();
        assert_eq!(parsed.agent.model, "gpt-4o-mini");
        assert_eq!(parsed.agent.max_iterations, 7);
        assert_eq!(parsed.client.provider, "openai");
    }

    #[test]
    fn probe_lines_mark_verified_unsupported_and_untested() {
        let result = ProbeResult {
            streaming: Some(true),
            tools: Some(false),
            vision: None,
            ttft_ms: Some(240),
        };
        let lines = probe_summary_lines(&result);
        assert!(lines[0].contains("verified"));
        assert!(lines[0].contains("TTFT 240ms"));
        assert!(lines[1].contains("unsupported"));
        assert!(lines[2].contains("not tested"));
    }

    #[test]
    fn smoke_format_includes_reply_and_usage() {
        let outcome = SmokeOutcome {
            reply: "Hi there!".into(),
            prompt_tokens: 8,
            completion_tokens: 3,
            total_tokens: 11,
        };
        let rendered = format_smoke(&outcome);
        assert!(rendered.contains("Hi there!"));
        assert!(rendered.contains("8 prompt"));
        assert!(rendered.contains("11 total"));
    }

    #[test]
    fn ollama_host_normalization() {
        assert_eq!(
            normalize_ollama_host("10.0.0.5:11434"),
            "http://10.0.0.5:11434/v1"
        );
        assert_eq!(
            normalize_ollama_host("http://localhost:11434/v1"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_ollama_host("https://ollama.example.com/"),
            "https://ollama.example.com/v1"
        );
    }
}
