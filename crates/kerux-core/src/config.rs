use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

static RUNTIME_CONFIG: OnceLock<RwLock<AppConfig>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub client: ClientSettings,
    pub agent: BehaviorSettings,
    pub autonomous: AutonomousSettings,
    pub logging: LoggingSettings,
    pub recorder: RecorderSettings,
    pub validation: crate::validation::ValidationPolicy,
    pub tui: TuiSettings,
    pub telemetry: TelemetrySettings,
    pub budget: BudgetSettings,
    pub memory: MemorySettings,
    pub mcp: McpSettings,
    pub skills: SkillsSettings,
    pub taste: TasteSettings,
    pub gateway: GatewaySettings,
    pub tools: ToolSettings,
    pub curator: crate::curator::CurationPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TasteSettings {
    pub enabled: bool,
    pub min_confidence: f32,
    pub max_items: usize,
}

impl Default for TasteSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            min_confidence: 0.5,
            max_items: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetrySettings {
    pub enabled: bool,
    pub currency: String,
    pub input_cost_per_million: f64,
    pub output_cost_per_million: f64,
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            currency: "USD".to_string(),
            input_cost_per_million: 0.0,
            output_cost_per_million: 0.0,
        }
    }
}

/// Cost guardrails: estimated-spend ceilings applied on top of the
/// `[telemetry]` cost rates. Off by default. Enforcement (auto-pause,
/// gateway notification, model downgrade) consumes this surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetSettings {
    /// Master switch for the cost guardrails. Default `false`: no ceiling
    /// is applied regardless of the limits below.
    pub enabled: bool,
    /// Estimated-spend ceiling for a single agent run, in the telemetry
    /// currency. `0.0` disables the per-run ceiling.
    pub per_run_limit: f64,
    /// Estimated-spend ceiling across all runs in a rolling day, in the
    /// telemetry currency. `0.0` disables the daily ceiling.
    pub daily_limit: f64,
    /// Emit a gateway warning once estimated spend crosses this percentage
    /// of a configured limit, before the hard ceiling trips.
    pub warn_threshold_pct: u8,
    /// Action taken when a limit is hit: `pause` (halt and await the
    /// operator), `downgrade` (switch to `downgrade_model`), or `stop`
    /// (end the run).
    pub on_limit: String,
    /// Cheaper model to switch to when `on_limit = "downgrade"`.
    pub downgrade_model: Option<String>,
}

impl Default for BudgetSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            per_run_limit: 0.0,
            daily_limit: 0.0,
            warn_threshold_pct: 80,
            on_limit: "pause".to_string(),
            downgrade_model: None,
        }
    }
}

impl BudgetSettings {
    /// Validate the guardrail policy. Called at config load so a
    /// misconfigured ceiling fails fast instead of silently doing nothing.
    pub fn validate(&self) -> Result<()> {
        if self.warn_threshold_pct > 100 {
            return Err(Error::Config(format!(
                "[budget] warn_threshold_pct must be <= 100 (got {})",
                self.warn_threshold_pct
            )));
        }
        if self.per_run_limit < 0.0 || self.daily_limit < 0.0 {
            return Err(Error::Config(
                "[budget] limits must be non-negative".to_string(),
            ));
        }
        match self.on_limit.as_str() {
            "pause" | "downgrade" | "stop" => {}
            other => {
                return Err(Error::Config(format!(
                    "[budget] on_limit must be 'pause', 'downgrade', or 'stop' (got '{}')",
                    other
                )));
            }
        }
        if self.on_limit == "downgrade" && self.downgrade_model.is_none() {
            return Err(Error::Config(
                "[budget] downgrade_model is required when on_limit = 'downgrade'".to_string(),
            ));
        }
        Ok(())
    }
}

/// Memory firewall & storage policy settings
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemorySettings {
    /// Minimum trust score (0-100) required for a memory to be injected
    /// into prompt context or accepted without quarantine. Default 50.
    pub trust_threshold: u8,
    /// Automatically redact secrets/credentials before saving memories,
    /// and reject memories that contain unmasked credentials. Default true.
    pub mask_secrets: bool,
    /// Quarantine low-trust memories or memories with failed validation. Default true.
    pub quarantine_low_trust: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            trust_threshold: 50,
            mask_secrets: true,
            quarantine_low_trust: true,
        }
    }
}

impl MemorySettings {
    pub fn validate(&self) -> Result<()> {
        if self.trust_threshold > 100 {
            return Err(Error::Config(format!(
                "[memory] trust_threshold must be <= 100 (got {})",
                self.trust_threshold
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSettings {
    pub base_url: String,
    pub api_key: Option<String>,
    pub auth_ref: Option<String>,
    pub timeout_secs: u64,
    /// Timeout for model-list discovery requests (`list_models`).
    /// Gateways can return 1000+ models slowly; default 60s.
    pub model_list_timeout_secs: u64,
    pub max_context_length: usize,
    /// LLM provider backend: "openai" (default), "anthropic", "ollama", "openrouter", "gemini"
    pub provider: String,
    /// Optional per-provider connection overrides.
    pub openai: ProviderEndpointSettings,
    pub anthropic: ProviderEndpointSettings,
    pub ollama: ProviderEndpointSettings,
    pub openrouter: ProviderEndpointSettings,
    pub gemini: ProviderEndpointSettings,
    /// Fallback providers tried in order when the primary provider fails
    /// (rate limit, timeout, connection error). Empty by default: the client
    /// stays locked to the single configured provider unless the user
    /// explicitly opts in — a silent fallback could downgrade to a worse
    /// model without the user noticing.
    pub fallback: Vec<FallbackProviderSettings>,
}

/// One fallback provider entry: same shape as the primary `[client]` block,
/// minus the fields that only make sense at the top level.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackProviderSettings {
    pub provider: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Model override for this fallback (defaults to the primary model).
    pub model: Option<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderEndpointSettings {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout_secs: Option<u64>,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: None,
            auth_ref: None,
            // Per-read deadline (see client builders): reasoning models can
            // take minutes before the first token, so keep this generous.
            timeout_secs: 300,
            model_list_timeout_secs: 60,
            max_context_length: 128_000,
            provider: "openai".to_string(),
            fallback: Vec::new(),
            openai: ProviderEndpointSettings::default(),
            anthropic: ProviderEndpointSettings::default(),
            ollama: ProviderEndpointSettings::default(),
            openrouter: ProviderEndpointSettings::default(),
            gemini: ProviderEndpointSettings::default(),
        }
    }
}

impl ClientSettings {
    fn endpoint_settings(
        &self,
        kind: crate::client::ProviderKind,
    ) -> Option<&ProviderEndpointSettings> {
        match kind {
            crate::client::ProviderKind::Openai => Some(&self.openai),
            crate::client::ProviderKind::Ollama => Some(&self.ollama),
            crate::client::ProviderKind::Openrouter => Some(&self.openrouter),
            crate::client::ProviderKind::Anthropic => Some(&self.anthropic),
            crate::client::ProviderKind::Gemini => Some(&self.gemini),
            crate::client::ProviderKind::Nous => None,
        }
    }

    pub(crate) fn resolved_base_url_for(
        &self,
        kind: crate::client::ProviderKind,
    ) -> Option<String> {
        let configured = self
            .endpoint_settings(kind)
            .and_then(|settings| settings.base_url.clone());
        let value = configured.or(match kind {
            crate::client::ProviderKind::Openai => Some(self.base_url.clone()),
            crate::client::ProviderKind::Ollama => non_empty_env("OLLAMA_BASE_URL"),
            crate::client::ProviderKind::Openrouter => non_empty_env("OPENROUTER_BASE_URL"),
            crate::client::ProviderKind::Anthropic => non_empty_env("ANTHROPIC_BASE_URL"),
            crate::client::ProviderKind::Gemini => non_empty_env("GEMINI_BASE_URL"),
            crate::client::ProviderKind::Nous => None,
        });
        value.filter(|v| !v.trim().is_empty())
    }

    pub(crate) fn resolved_api_key_for(&self, kind: crate::client::ProviderKind) -> Option<String> {
        let configured = self
            .endpoint_settings(kind)
            .and_then(|settings| settings.api_key.clone());
        configured
            .or(match kind {
                crate::client::ProviderKind::Openai => self
                    .api_key
                    .clone()
                    .or_else(|| non_empty_env("OPENAI_API_KEY")),
                crate::client::ProviderKind::Ollama => non_empty_env("OLLAMA_API_KEY"),
                crate::client::ProviderKind::Openrouter => non_empty_env("OPENROUTER_API_KEY"),
                crate::client::ProviderKind::Anthropic => non_empty_env("ANTHROPIC_API_KEY"),
                crate::client::ProviderKind::Gemini => non_empty_env("GEMINI_API_KEY"),
                crate::client::ProviderKind::Nous => None,
            })
            .filter(|v| !v.trim().is_empty())
    }

    pub(crate) fn resolved_timeout_secs_for(
        &self,
        kind: crate::client::ProviderKind,
    ) -> Option<u64> {
        let configured = self
            .endpoint_settings(kind)
            .and_then(|settings| settings.timeout_secs);
        configured.or(match kind {
            crate::client::ProviderKind::Openai => Some(self.timeout_secs),
            crate::client::ProviderKind::Ollama => {
                non_empty_env("OLLAMA_TIMEOUT_SECS").and_then(|value| value.parse().ok())
            }
            crate::client::ProviderKind::Openrouter => {
                non_empty_env("OPENROUTER_TIMEOUT_SECS").and_then(|value| value.parse().ok())
            }
            crate::client::ProviderKind::Anthropic => {
                non_empty_env("ANTHROPIC_TIMEOUT_SECS").and_then(|value| value.parse().ok())
            }
            crate::client::ProviderKind::Gemini => {
                non_empty_env("GEMINI_TIMEOUT_SECS").and_then(|value| value.parse().ok())
            }
            crate::client::ProviderKind::Nous => None,
        })
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BehaviorSettings {
    pub model: String,
    pub max_iterations: usize,
    pub tool_timeout_secs: u64,
    pub request_timeout_secs: u64,
    pub system_prompt: Option<String>,
    pub stream: bool,
    pub context_window: usize,
    pub max_healing_attempts: usize,
    pub show_reasoning: bool,
    /// Token budget for the `<repo_map>` context block. `0` disables
    /// repository mapping entirely.
    pub repo_map_tokens: usize,
    /// Maximum files discovered for repo map scoring (cap huge repos).
    /// Defaults to 500; increase only if needed for very small repos.
    pub repo_map_max_files: usize,
    /// Force the edit-format prompt hint (`search_replace`, `patch`, or
    /// `full_file`) regardless of what the capability table guesses for the
    /// configured model. `None` keeps capability-table behavior.
    pub edit_format_override: Option<crate::client::EditFormat>,
    /// After each successful interactive run, auto-commit working-tree
    /// changes with a Conventional Commit derived from the staged diff.
    /// Default `false`: keep `/undo`-only behavior. Agent-authored edits
    /// inside a transaction are only committed when the run succeeds.
    pub auto_commit: bool,
    /// Task 2.3 bounded repair policy: after a path exhausts its per-run edit
    /// repair budget, subsequent failed attempts on it are recorded as
    /// `repair_allowed=false` instead of feeding another repair round.
    /// `None` falls back to `max_healing_attempts`.
    #[serde(default)]
    pub max_repair_attempts: Option<usize>,
}

impl Default for BehaviorSettings {
    fn default() -> Self {
        Self {
            model: "gpt-4".to_string(),
            max_iterations: 20,
            tool_timeout_secs: 30,
            request_timeout_secs: 120,
            system_prompt: None,
            stream: true,
            context_window: 128_000,
            max_healing_attempts: 3,
            show_reasoning: true,
            repo_map_tokens: 0,
            repo_map_max_files: 500,
            edit_format_override: None,
            auto_commit: false,
            max_repair_attempts: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutonomousSettings {
    pub interval_secs: u64,
    pub todo_path: PathBuf,
    pub status_path: PathBuf,
    pub test_command: String,
    pub git_remote: String,
    pub git_branch: String,
    pub commit_message: String,
    pub command_timeout_secs: u64,
    pub max_failures_per_state: usize,
}

impl Default for AutonomousSettings {
    fn default() -> Self {
        Self {
            interval_secs: 300,
            todo_path: PathBuf::from("TODO.md"),
            status_path: PathBuf::from("autonomous-status.toml"),
            test_command: "cargo test --workspace".to_string(),
            git_remote: "origin".to_string(),
            git_branch: "agent-dev".to_string(),
            commit_message: "Auto-commit by kerux".to_string(),
            command_timeout_secs: 900,
            max_failures_per_state: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingSettings {
    pub level: String,
    pub format: String,
    pub log_file: Option<String>,
    pub with_target: bool,
    pub with_thread_ids: bool,
    pub with_file: bool,
    pub with_line_number: bool,
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "pretty".to_string(),
            log_file: None,
            with_target: false,
            with_thread_ids: false,
            with_file: false,
            with_line_number: false,
        }
    }
}

/// Flight-recorder policy.
///
/// Bounded by design: no retention policy, remote upload, signing key, or
/// compression in v1. Payloads are redacted and truncated to
/// `max_payload_bytes` before they touch disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecorderSettings {
    /// Whether the flight recorder journals runs at all.
    pub enabled: bool,
    /// Maximum bytes of a single redacted event payload.
    pub max_payload_bytes: usize,
    /// Record assistant/tool content bodies (still redacted + bounded).
    pub record_content: bool,
    /// Record model reasoning/thinking bodies (metadata only when false).
    pub record_reasoning: bool,
    /// "warn" (log and continue) or "fail" (abort the run) on journal I/O error.
    pub failure_mode: String,
}

impl Default for RecorderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_payload_bytes: 65536,
            record_content: true,
            record_reasoning: false,
            failure_mode: "warn".to_string(),
        }
    }
}

impl RecorderSettings {
    /// Map `failure_mode` to the recorder's enum, defaulting to warn.
    pub fn recorder_failure_mode(&self) -> crate::run_journal::RecorderFailureMode {
        match self.failure_mode.as_str() {
            "fail" => crate::run_journal::RecorderFailureMode::Fail,
            _ => crate::run_journal::RecorderFailureMode::Warn,
        }
    }

    /// Serialize this policy into the `recorder_policy` manifest value.
    pub fn to_policy_value(&self) -> serde_json::Value {
        serde_json::json!({
            "max_payload_bytes": self.max_payload_bytes,
            "record_content": self.record_content,
            "record_reasoning": self.record_reasoning,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiSettings {
    pub theme: String,
    pub rich_output: bool,
    pub show_tool_calls: bool,
    pub show_iterations: bool,
    pub landing_title: String,
    pub prompt_placeholder: String,
    pub refresh_rate_ms: u64,
    pub compact_width: u16,
    pub medium_width: u16,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            theme: "opencode".to_string(),
            rich_output: true,
            show_tool_calls: true,
            show_iterations: true,
            landing_title: "KERUX".to_string(),
            prompt_placeholder: "Ask anything... \"Fix a TODO in the codebase\"".to_string(),
            refresh_rate_ms: 80,
            compact_width: 96,
            medium_width: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    #[default]
    Http,
    Stdio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransportKind,
    pub url: Option<String>,
    pub auth_token: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub enabled: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: McpTransportKind::Http,
            url: None,
            auth_token: None,
            command: None,
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettings {
    pub autoload: bool,
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            autoload: true,
            servers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsSettings {
    pub root_dir: PathBuf,
    pub autoload: bool,
    pub template_name: String,
    pub template_description: String,
}

impl Default for SkillsSettings {
    fn default() -> Self {
        let root_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kerux")
            .join("skills");

        Self {
            root_dir,
            autoload: true,
            template_name: "new-skill".to_string(),
            template_description: "Describe what this skill does.".to_string(),
        }
    }
}

fn default_webui_addr() -> String {
    "127.0.0.1:8787".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewaySettings {
    pub telegram_enabled: bool,
    pub telegram_token: Option<String>,
    pub telegram_api_base: String,
    pub discord_enabled: bool,
    pub discord_token: Option<String>,
    pub discord_api_base: String,
    pub slack_enabled: bool,
    pub slack_token: Option<String>,
    pub slack_signing_secret: Option<String>,
    pub slack_api_base: String,
    pub whatsapp_enabled: bool,
    /// Base URL of the Baileys WhatsApp bridge (e.g. "http://127.0.0.1:3000").
    pub whatsapp_bridge_url: Option<String>,
    pub webhooks_enabled: bool,
    pub webhooks_addr: Option<String>,
    pub webui_enabled: bool,
    #[serde(default = "default_webui_addr")]
    pub webui_addr: String,
    pub webui_password: String,
    pub admins: Vec<String>,
    /// Stream model output live into the chat (message edited as tokens
    /// arrive) instead of showing a heartbeat until the full reply is ready.
    pub streaming_replies: bool,
    /// Require explicit approval (Telegram inline keyboard ✅/❌) before
    /// executing dangerous tools (terminal, file writes, code execution).
    pub tool_approval: bool,
    /// Seconds to wait for an approval decision before auto-denying.
    pub tool_approval_timeout_secs: u64,
    /// Model used to transcribe incoming voice notes via the OpenAI-compatible
    /// `/audio/transcriptions` endpoint (e.g. "gemini/gemini-2.5-pro").
    /// `None` disables voice transcription.
    pub stt_model: Option<String>,
    /// Roll the oldest messages into a summary when a session grows past the
    /// compaction threshold, instead of silently dropping them at the cap.
    pub context_compaction: bool,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            telegram_enabled: false,
            telegram_token: None,
            telegram_api_base: "https://api.telegram.org".to_string(),
            discord_enabled: false,
            discord_token: None,
            discord_api_base: "https://discord.com/api/v10".to_string(),
            slack_enabled: false,
            slack_token: None,
            slack_signing_secret: None,
            slack_api_base: "https://slack.com/api".to_string(),
            whatsapp_enabled: false,
            whatsapp_bridge_url: None,
            webhooks_enabled: false,
            webhooks_addr: None,
            webui_enabled: false,
            webui_addr: default_webui_addr(),
            webui_password: String::new(),
            admins: Vec::new(),
            streaming_replies: false,
            tool_approval: true,
            tool_approval_timeout_secs: 300,
            stt_model: None,
            context_compaction: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolSettings {
    pub registry_timeout_secs: u64,
    pub event_channel_size: usize,
    pub web: WebToolSettings,
    pub http: HttpToolSettings,
    pub terminal: TerminalSettings,
    pub code_execution: CodeExecutionSettings,
    pub delegation: DelegationSettings,
}

/// Sub-agent delegation (`delegate_to_sub_agent` tool) settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DelegationSettings {
    /// Register the delegation tool at all. Default ON: the tool only costs
    /// tokens when the model actively chooses to call it.
    pub enabled: bool,
    /// Max concurrent sub-agent runs (shared semaphore across the process).
    pub max_concurrent: usize,
}

impl Default for DelegationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: 3,
        }
    }
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            registry_timeout_secs: 30,
            event_channel_size: 100,
            web: WebToolSettings::default(),
            http: HttpToolSettings::default(),
            terminal: TerminalSettings::default(),
            code_execution: CodeExecutionSettings::default(),
            delegation: DelegationSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebToolSettings {
    pub search_url: String,
    pub search_timeout_secs: u64,
    pub fetch_timeout_secs: u64,
    pub user_agent: String,
    pub default_results: usize,
    pub max_results: usize,
}

impl Default for WebToolSettings {
    fn default() -> Self {
        Self {
            search_url: "https://lite.duckduckgo.com/lite/?q={query}".to_string(),
            search_timeout_secs: 15,
            fetch_timeout_secs: 30,
            user_agent: "Mozilla/5.0 (compatible; KeruxAgent/0.1)".to_string(),
            default_results: 10,
            max_results: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpToolSettings {
    pub timeout_secs: u64,
}

impl Default for HttpToolSettings {
    fn default() -> Self {
        Self { timeout_secs: 30 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalSettings {
    pub max_timeout_secs: u64,
    pub max_output_bytes: usize,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            max_timeout_secs: 300,
            max_output_bytes: 1_000_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodeExecutionSettings {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
}

impl Default for CodeExecutionSettings {
    fn default() -> Self {
        Self {
            default_timeout_secs: 60,
            max_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub source: Option<PathBuf>,
}

pub fn install_runtime_config(config: AppConfig) {
    let store = RUNTIME_CONFIG.get_or_init(|| RwLock::new(AppConfig::default()));
    if let Ok(mut current) = store.write() {
        *current = config;
    }
}

pub fn runtime_config() -> AppConfig {
    let store = RUNTIME_CONFIG.get_or_init(|| RwLock::new(AppConfig::default()));
    store
        .read()
        .map(|config| config.clone())
        .unwrap_or_else(|_| AppConfig::default())
}

pub fn load_app_config(explicit: Option<&Path>) -> Result<LoadedConfig> {
    if let Some(path) = explicit {
        if !path.exists() {
            return Err(Error::Config(format!(
                "Config file '{}' was not found. Pass a valid --config path or create kerux.toml.",
                path.display()
            )));
        }

        return Ok(LoadedConfig {
            config: parse_config_file(path)?,
            source: Some(path.to_path_buf()),
        });
    }

    for path in default_config_paths() {
        if path.exists() {
            return Ok(LoadedConfig {
                config: parse_config_file(&path)?,
                source: Some(path),
            });
        }
    }

    Ok(LoadedConfig {
        config: AppConfig::default(),
        source: None,
    })
}

pub fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("kerux.toml"), PathBuf::from(".kerux.toml")];

    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("kerux").join("config.toml"));
    }

    paths
}

pub fn parse_config_file(path: &Path) -> Result<AppConfig> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        Error::Config(format!(
            "Failed to read config file '{}': {}",
            path.display(),
            error
        ))
    })?;

    parse_config_str(&raw, path)
}

pub fn parse_config_str(raw: &str, source: &Path) -> Result<AppConfig> {
    let config: AppConfig = toml::from_str(raw).map_err(|error| {
        let message = match error.span() {
            Some(span) => format!(
                "Invalid TOML in '{}': {} (bytes {}..{})",
                source.display(),
                error,
                span.start,
                span.end
            ),
            None => format!("Invalid TOML in '{}': {}", source.display(), error),
        };

        Error::Config(message)
    })?;
    config.budget.validate()?;
    config.memory.validate()?;
    Ok(config)
}

impl AppConfig {
    pub fn apply_env_overrides(&mut self) -> Result<()> {
        apply_string_value_override("KERUX_PROVIDER", &mut self.client.provider);
        apply_string_option_override("OPENAI_API_KEY", &mut self.client.api_key)?;
        apply_string_value_override("OPENAI_BASE_URL", &mut self.client.base_url);
        apply_string_option_override("KERUX_AUTH_REF", &mut self.client.auth_ref)?;
        apply_string_value_override("KERUX_MODEL", &mut self.agent.model);
        apply_usize_override("KERUX_MAX_ITERATIONS", &mut self.agent.max_iterations)?;
        apply_u64_override("KERUX_TOOL_TIMEOUT", &mut self.agent.tool_timeout_secs)?;
        apply_u64_override(
            "KERUX_REQUEST_TIMEOUT",
            &mut self.agent.request_timeout_secs,
        )?;
        apply_usize_override("KERUX_CONTEXT_WINDOW", &mut self.agent.context_window)?;
        apply_usize_override(
            "KERUX_MAX_HEALING_ATTEMPTS",
            &mut self.agent.max_healing_attempts,
        )?;
        apply_bool_override("KERUX_STREAM", &mut self.agent.stream)?;
        apply_string_option_override("KERUX_SYSTEM_PROMPT", &mut self.agent.system_prompt)?;
        apply_u64_override(
            "KERUX_AUTONOMOUS_INTERVAL",
            &mut self.autonomous.interval_secs,
        )?;
        apply_path_override("KERUX_AUTONOMOUS_TODO", &mut self.autonomous.todo_path)?;
        apply_path_override("KERUX_AUTONOMOUS_STATUS", &mut self.autonomous.status_path)?;
        apply_string_value_override(
            "KERUX_AUTONOMOUS_TEST_COMMAND",
            &mut self.autonomous.test_command,
        );
        apply_string_value_override(
            "KERUX_AUTONOMOUS_GIT_REMOTE",
            &mut self.autonomous.git_remote,
        );
        apply_string_value_override(
            "KERUX_AUTONOMOUS_GIT_BRANCH",
            &mut self.autonomous.git_branch,
        );
        apply_string_value_override(
            "KERUX_AUTONOMOUS_COMMIT_MESSAGE",
            &mut self.autonomous.commit_message,
        );
        apply_u64_override(
            "KERUX_AUTONOMOUS_COMMAND_TIMEOUT",
            &mut self.autonomous.command_timeout_secs,
        )?;
        apply_usize_override(
            "KERUX_AUTONOMOUS_MAX_FAILURES",
            &mut self.autonomous.max_failures_per_state,
        )?;
        apply_string_value_override("KERUX_LOG_LEVEL", &mut self.logging.level);
        apply_path_override("KERUX_SKILLS_DIR", &mut self.skills.root_dir)?;
        apply_bool_override("KERUX_WEBUI_ENABLED", &mut self.gateway.webui_enabled)?;
        apply_string_value_override("KERUX_WEBUI_ADDR", &mut self.gateway.webui_addr);
        if let Ok(val) = std::env::var("KERUX_WEBUI_PASSWORD") {
            if !val.trim().is_empty() {
                self.gateway.webui_password = val;
            }
        } else if let Ok(val) = std::env::var("HERMES_WEBUI_PASSWORD") {
            if !val.trim().is_empty() {
                self.gateway.webui_password = val;
            }
        }
        Ok(())
    }
}

fn read_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn apply_string_option_override(name: &str, target: &mut Option<String>) -> Result<()> {
    if let Some(value) = read_env(name) {
        *target = Some(value);
    }
    Ok(())
}

fn apply_string_value_override(name: &str, target: &mut String) {
    if let Some(value) = read_env(name) {
        *target = value;
    }
}

fn apply_u64_override(name: &str, target: &mut u64) -> Result<()> {
    if let Some(value) = read_env(name) {
        *target = value.parse().map_err(|_| {
            Error::Config(format!(
                "Environment variable '{}' must be an unsigned integer.",
                name
            ))
        })?;
    }
    Ok(())
}

fn apply_usize_override(name: &str, target: &mut usize) -> Result<()> {
    if let Some(value) = read_env(name) {
        *target = value.parse().map_err(|_| {
            Error::Config(format!(
                "Environment variable '{}' must be an unsigned integer.",
                name
            ))
        })?;
    }
    Ok(())
}

fn apply_bool_override(name: &str, target: &mut bool) -> Result<()> {
    if let Some(value) = read_env(name) {
        *target = match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => {
                return Err(Error::Config(format!(
                    "Environment variable '{}' must be a boolean.",
                    name
                )))
            }
        };
    }
    Ok(())
}

fn apply_path_override(name: &str, target: &mut PathBuf) -> Result<()> {
    if let Some(value) = read_env(name) {
        *target = PathBuf::from(value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> &'static Mutex<()> {
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kerux_config_test_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn with_current_dir<T>(path: &Path, f: impl FnOnce() -> T) -> T {
        let current = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        let result = f();
        std::env::set_current_dir(current).unwrap();
        result
    }

    fn set_env(name: &str, value: &str) -> Option<OsString> {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        previous
    }

    fn restore_env(name: &str, previous: Option<OsString>) {
        if let Some(value) = previous {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn example_toml_parses() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("kerux.example.toml");
        let raw = std::fs::read_to_string(&root).unwrap();
        let config = parse_config_str(&raw, &root).unwrap();
        assert_eq!(config.client.provider, "openai");
        assert_eq!(config.agent.model, "gpt-4");
        assert!(config.tui.rich_output);
        assert_eq!(config.autonomous.git_branch, "agent-dev");
        assert_eq!(
            config.autonomous.status_path,
            PathBuf::from("autonomous-status.toml")
        );
        assert_eq!(config.curator.memory_decay_days, 14);
        assert_eq!(config.curator.skill_distill_min_facts, 3);
        // Recorder section parses with documented defaults.
        assert!(config.recorder.enabled);
        assert_eq!(config.recorder.max_payload_bytes, 65536);
        assert!(config.recorder.record_content);
        assert!(!config.recorder.record_reasoning);
        assert_eq!(config.recorder.failure_mode, "warn");
        // Validation section parses with documented defaults.
        assert!(!config.validation.enabled);
        assert!(!config.validation.fail_fast);
        assert!(config.validation.validators.is_empty());
        config.validation.validate().unwrap();
        // Budget guardrails parse with documented defaults.
        assert!(!config.budget.enabled);
        assert_eq!(config.budget.per_run_limit, 0.0);
        assert_eq!(config.budget.daily_limit, 0.0);
        assert_eq!(config.budget.warn_threshold_pct, 80);
        assert_eq!(config.budget.on_limit, "pause");
        assert!(config.budget.downgrade_model.is_none());
        config.budget.validate().unwrap();
    }

    #[test]
    fn budget_defaults_are_compatible() {
        // An existing config with no [budget] section keeps working.
        let settings: BudgetSettings = toml::from_str("").unwrap();
        assert!(!settings.enabled);
        assert_eq!(settings.per_run_limit, 0.0);
        assert_eq!(settings.daily_limit, 0.0);
        assert_eq!(settings.warn_threshold_pct, 80);
        assert_eq!(settings.on_limit, "pause");
        assert!(settings.downgrade_model.is_none());
        settings.validate().unwrap();
    }

    #[test]
    fn client_model_list_timeout_defaults_and_parses() {
        let settings: ClientSettings = toml::from_str("").unwrap();
        assert_eq!(settings.model_list_timeout_secs, 60);

        let settings: ClientSettings = toml::from_str("model_list_timeout_secs = 120").unwrap();
        assert_eq!(settings.model_list_timeout_secs, 120);
    }

    #[test]
    fn budget_validation_rejects_bad_policies() {
        let path = Path::new("test.toml");
        let error = parse_config_str("[budget]\nwarn_threshold_pct = 150\n", path).unwrap_err();
        assert!(error.to_string().contains("warn_threshold_pct"));

        let error = parse_config_str("[budget]\nper_run_limit = -1.0\n", path).unwrap_err();
        assert!(error.to_string().contains("non-negative"));

        let error = parse_config_str("[budget]\non_limit = \"explode\"\n", path).unwrap_err();
        assert!(error.to_string().contains("on_limit"));

        let error = parse_config_str("[budget]\non_limit = \"downgrade\"\n", path).unwrap_err();
        assert!(error.to_string().contains("downgrade_model"));

        // A complete downgrade policy passes.
        let config = parse_config_str(
            "[budget]\nenabled = true\nper_run_limit = 2.5\ndaily_limit = 10.0\non_limit = \"downgrade\"\ndowngrade_model = \"gpt-4o-mini\"\n",
            path,
        )
        .unwrap();
        assert!(config.budget.enabled);
        assert_eq!(
            config.budget.downgrade_model.as_deref(),
            Some("gpt-4o-mini")
        );
    }

    #[test]
    fn taste_defaults_are_compatible_and_overridable() {
        let defaults: TasteSettings = toml::from_str("").unwrap();
        assert!(defaults.enabled);
        assert_eq!(defaults.min_confidence, 0.5);
        assert_eq!(defaults.max_items, 10);

        let configured: TasteSettings =
            toml::from_str("enabled = false\nmin_confidence = 0.75\nmax_items = 4\n").unwrap();
        assert!(!configured.enabled);
        assert_eq!(configured.min_confidence, 0.75);
        assert_eq!(configured.max_items, 4);
    }

    #[test]
    fn validation_defaults_are_compatible() {
        // An existing config with no [validation] section keeps working.
        let policy: crate::validation::ValidationPolicy = toml::from_str("").unwrap();
        assert!(!policy.enabled);
        assert!(policy.validators.is_empty());
        assert!(!policy.fail_fast);
        policy.validate().unwrap();
    }

    #[test]
    fn recorder_defaults_are_compatible() {
        // An existing config with no [recorder] section keeps working.
        let settings: RecorderSettings = toml::from_str("").unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.max_payload_bytes, 65536);
        assert!(settings.record_content);
        assert!(!settings.record_reasoning);
        assert_eq!(settings.failure_mode, "warn");
        assert!(matches!(
            settings.recorder_failure_mode(),
            crate::run_journal::RecorderFailureMode::Warn
        ));
    }

    #[test]
    fn recorder_failure_mode_maps_warn_and_fail() {
        let warn: RecorderSettings = toml::from_str("failure_mode = \"warn\"\n").unwrap();
        assert!(matches!(
            warn.recorder_failure_mode(),
            crate::run_journal::RecorderFailureMode::Warn
        ));
        let fail: RecorderSettings = toml::from_str("failure_mode = \"fail\"\n").unwrap();
        assert!(matches!(
            fail.recorder_failure_mode(),
            crate::run_journal::RecorderFailureMode::Fail
        ));
        // Unknown values fall back to warn rather than aborting runs.
        let unknown: RecorderSettings = toml::from_str("failure_mode = \"explode\"\n").unwrap();
        assert!(matches!(
            unknown.recorder_failure_mode(),
            crate::run_journal::RecorderFailureMode::Warn
        ));
    }

    #[test]
    fn recorder_policy_value_carries_payload_bounds() {
        let settings: RecorderSettings =
            toml::from_str("max_payload_bytes = 1024\nrecord_content = false\n").unwrap();
        let policy = settings.to_policy_value();
        assert_eq!(policy["max_payload_bytes"], 1024);
        assert_eq!(policy["record_content"], false);
        assert_eq!(policy["record_reasoning"], false);
    }

    #[test]
    fn agent_edit_format_override_parses() {
        let settings: BehaviorSettings =
            toml::from_str("model = \"gpt-4\"\nedit_format_override = \"search_replace\"\n")
                .unwrap();
        assert_eq!(
            settings.edit_format_override,
            Some(crate::client::EditFormat::SearchReplace)
        );

        let settings: BehaviorSettings = toml::from_str("model = \"gpt-4\"\n").unwrap();
        assert_eq!(settings.edit_format_override, None);
    }

    #[test]
    fn client_gemini_provider_resolves() {
        let settings: ClientSettings =
            toml::from_str("provider = \"gemini\"\n[gemini]\napi_key = \"k\"\n").unwrap();
        let resolved = crate::client::resolve_provider_settings(&settings).unwrap();
        assert_eq!(resolved.kind, crate::client::ProviderKind::Gemini);
        assert_eq!(resolved.config.api_key.as_deref(), Some("k"));
        assert_eq!(
            resolved.config.base_url,
            "https://generativelanguage.googleapis.com/v1beta"
        );
    }

    #[test]
    fn agent_auto_commit_defaults_off_and_parses() {
        let settings: BehaviorSettings = toml::from_str("model = \"gpt-4\"\n").unwrap();
        assert!(!settings.auto_commit);

        let settings: BehaviorSettings =
            toml::from_str("model = \"gpt-4\"\nauto_commit = true\n").unwrap();
        assert!(settings.auto_commit);
    }

    #[test]
    fn default_path_discovery_prefers_local_kerux_toml() {
        let _guard = env_lock().lock().unwrap();
        let dir = temp_dir("default_path");
        std::fs::write(
            dir.join("kerux.toml"),
            "[agent]\nmodel = \"gpt-4.1-mini\"\n",
        )
        .unwrap();

        let loaded = with_current_dir(&dir, || load_app_config(None)).unwrap();
        assert_eq!(loaded.source.unwrap().file_name().unwrap(), "kerux.toml");
        assert_eq!(loaded.config.agent.model, "gpt-4.1-mini");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_path_overrides_defaults() {
        let _guard = env_lock().lock().unwrap();
        let dir = temp_dir("explicit_path");
        let explicit = dir.join("custom.toml");
        std::fs::write(dir.join("kerux.toml"), "[agent]\nmodel = \"wrong\"\n").unwrap();
        std::fs::write(&explicit, "[agent]\nmodel = \"right\"\n").unwrap();

        let loaded = with_current_dir(&dir, || load_app_config(Some(&explicit))).unwrap();
        assert_eq!(loaded.config.agent.model, "right");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_provider_endpoint_overrides() {
        let config = parse_config_str(
            r##"
            [client]
            provider = "anthropic"

            [client.anthropic]
            base_url = "https://anthropic.local/v1"
            api_key = "anthropic-key"
            timeout_secs = 33
            "##,
            Path::new("providers.toml"),
        )
        .unwrap();

        assert_eq!(config.client.provider, "anthropic");
        assert_eq!(
            config.client.anthropic.base_url.as_deref(),
            Some("https://anthropic.local/v1")
        );
        assert_eq!(
            config
                .client
                .resolved_api_key_for(crate::client::ProviderKind::Anthropic)
                .as_deref(),
            Some("anthropic-key")
        );
    }

    #[test]
    fn invalid_toml_returns_field_aware_error() {
        let path = PathBuf::from("broken.toml");
        let error = parse_config_str("[agent]\nmax_iterations = \"many\"\n", &path).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("Invalid TOML"));
        assert!(text.contains("expected"));
    }

    #[test]
    fn env_overrides_apply_after_file_values() {
        let _guard = env_lock().lock().unwrap();
        let previous_model = set_env("KERUX_MODEL", "gpt-4.1");
        let previous_provider = set_env("KERUX_PROVIDER", "ollama");
        let previous_stream = set_env("KERUX_STREAM", "false");
        let previous_interval = set_env("KERUX_AUTONOMOUS_INTERVAL", "120");
        let previous_status = set_env("KERUX_AUTONOMOUS_STATUS", "runtime/autonomous-status.toml");

        let mut config = parse_config_str(
            "[agent]\nmodel = \"gpt-4o-mini\"\nstream = true\n",
            Path::new("env.toml"),
        )
        .unwrap();
        config.apply_env_overrides().unwrap();

        assert_eq!(config.agent.model, "gpt-4.1");
        assert_eq!(config.client.provider, "ollama");
        assert!(!config.agent.stream);
        assert_eq!(config.autonomous.interval_secs, 120);
        assert_eq!(
            config.autonomous.status_path,
            PathBuf::from("runtime/autonomous-status.toml")
        );

        restore_env("KERUX_MODEL", previous_model);
        restore_env("KERUX_PROVIDER", previous_provider);
        restore_env("KERUX_STREAM", previous_stream);
        restore_env("KERUX_AUTONOMOUS_INTERVAL", previous_interval);
        restore_env("KERUX_AUTONOMOUS_STATUS", previous_status);
    }
}
