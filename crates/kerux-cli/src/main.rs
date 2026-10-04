//! Kerux CLI

#![allow(clippy::double_must_use)]

mod autonomous;
mod runs;
mod screenshot;
mod session;
mod tui;
mod wizard;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, Subcommand};
use kerux_core::agent::{AgentConfig, AgentEvent, AgentTelemetry, KeruxAgent};
use kerux_core::auth::{default_auth_store_path, AuthMethod, AuthStore};
use kerux_core::client::{build_provider_for_kind, ClientConfig, LLMProvider, ProviderKind};
use kerux_core::config::{
    install_runtime_config, load_app_config, runtime_config, AppConfig, BehaviorSettings,
    LoggingSettings, McpServerConfig, McpTransportKind, TelemetrySettings,
};
use kerux_core::mcp::McpManager;
use kerux_core::memory::MemoryManager;
use kerux_core::tools::{KeruxTool, ToolContext, ToolRegistry};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tracing::Level;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::tui::{LaunchMode, TuiApp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogTarget {
    Stderr,
    Sink,
    File,
}

#[derive(Debug, Parser)]
#[command(
    name = "kerux",
    about = "Kerux: A high-performance ReAct agent framework",
    version
)]
struct Cli {
    #[arg(short, long, global = true)]
    verbose: bool,

    #[arg(short, long, global = true)]
    log_level: Option<String>,

    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[arg(long, global = true, env = "OPENAI_API_KEY")]
    api_key: Option<String>,

    #[arg(long, global = true, env = "OPENAI_BASE_URL")]
    base_url: Option<String>,

    #[arg(long, global = true)]
    model: Option<String>,

    #[arg(long, global = true)]
    max_iterations: Option<usize>,

    #[arg(long, global = true)]
    tool_timeout: Option<u64>,

    #[arg(long, global = true)]
    request_timeout: Option<u64>,

    #[arg(long, global = true)]
    context_window: Option<usize>,

    #[arg(long, global = true)]
    max_healing_attempts: Option<usize>,

    #[arg(long, global = true, action = ArgAction::SetTrue, conflicts_with = "no_stream")]
    stream: bool,

    #[arg(long = "no-stream", global = true, action = ArgAction::SetTrue, conflicts_with = "stream")]
    no_stream: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run {
        #[arg(short, long)]
        system: Option<String>,

        #[arg(short, long)]
        query: Option<String>,

        #[arg(long, action = ArgAction::SetTrue)]
        autonomous: bool,
    },
    Autonomous {
        #[arg(short, long)]
        system: Option<String>,
    },
    Tools {
        #[arg(short, long)]
        verbose: bool,
    },
    Chat {
        #[arg(short, long)]
        system: Option<String>,
        /// Session ID to resume or save into (defaults to 'cli:default').
        #[arg(long)]
        session: Option<String>,
    },
    Serve {
        #[arg(short, long)]
        system: Option<String>,
    },
    Test {
        #[arg()]
        tool_name: String,

        #[arg(short, long)]
        args: Option<String>,
    },
    /// Run configured project validators once.
    Validate,
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Share learned coding-style profiles between projects.
    Taste {
        #[command(subcommand)]
        command: TasteCommands,
    },
    /// Inspect recorded run journals (read-only; never executes anything)
    Runs {
        #[command(subcommand)]
        command: runs::RunsCommands,
    },
    /// Manage and export conversation sessions.
    Session {
        #[command(subcommand)]
        command: session::SessionCommands,
    },
    /// Interactive setup: provider, API key, model picker, fallback, smoke test.
    #[command(alias = "onboarding")]
    Wizard,
    /// Switch the configured model (fuzzy picker over the live model list).
    Model {
        /// Model id to set directly; omit to open the picker.
        #[arg()]
        model_id: Option<String>,

        /// Bypass the model-list cache and force a fresh fetch.
        #[arg(long)]
        refresh: bool,
    },
    /// Render TUI screenshots headlessly (used by the docs preview workflow).
    #[command(hide = true)]
    Screenshot {
        /// Output directory for the PNG files.
        #[arg(short, long, default_value = "assets")]
        out: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommands {
    Providers,
    Login {
        #[arg()]
        provider: String,
    },
    SetApiKey {
        #[arg()]
        provider: String,

        #[arg(long)]
        name: Option<String>,

        #[arg(long = "env")]
        env_var: Option<String>,

        #[arg(long)]
        base_url: Option<String>,
    },
    SetBearerToken {
        #[arg()]
        provider: String,

        #[arg(long)]
        name: Option<String>,

        #[arg(long = "env")]
        env_var: Option<String>,

        #[arg(long)]
        base_url: Option<String>,
    },
    List,
    Logout {
        #[arg()]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum TasteCommands {
    /// Save this project's profile into the portable registry.
    Push {
        #[arg()]
        name: String,
    },
    /// Merge a registry profile into this project.
    Pull {
        #[arg()]
        name: String,
    },
}

fn init_logging(
    verbose: bool,
    cli_log_level: Option<&str>,
    logging: &LoggingSettings,
    rich_output: bool,
) {
    let env_filter = if verbose {
        EnvFilter::new(format!("{}", Level::DEBUG))
    } else if let Some(level) = cli_log_level {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(logging.level.clone()))
    };

    let subscriber = tracing_subscriber::registry().with(env_filter);
    let layer = fmt::layer()
        .with_target(logging.with_target)
        .with_thread_ids(logging.with_thread_ids)
        .with_file(logging.with_file)
        .with_line_number(logging.with_line_number);

    match select_log_target(logging, rich_output, io::stdout().is_terminal()) {
        LogTarget::File => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(logging.log_file.as_ref().expect("log file should exist"))
                .expect("failed to open log file");
            let writer = Mutex::new(file);
            match logging.format.as_str() {
                "json" => subscriber
                    .with(layer.with_writer(writer).with_ansi(false).json())
                    .init(),
                "compact" => subscriber
                    .with(layer.with_writer(writer).with_ansi(false).compact())
                    .init(),
                _ => subscriber
                    .with(layer.with_writer(writer).with_ansi(false).pretty())
                    .init(),
            }
        }
        LogTarget::Sink => match logging.format.as_str() {
            "json" => subscriber
                .with(layer.with_writer(io::sink).with_ansi(false).json())
                .init(),
            "compact" => subscriber
                .with(layer.with_writer(io::sink).with_ansi(false).compact())
                .init(),
            _ => subscriber
                .with(layer.with_writer(io::sink).with_ansi(false).pretty())
                .init(),
        },
        LogTarget::Stderr => match logging.format.as_str() {
            "json" => subscriber.with(layer.json()).init(),
            "compact" => subscriber.with(layer.compact()).init(),
            _ => subscriber.with(layer.pretty()).init(),
        },
    }
}

fn select_log_target(logging: &LoggingSettings, rich_output: bool, is_tty: bool) -> LogTarget {
    if logging.log_file.is_some() {
        LogTarget::File
    } else if rich_output && is_tty {
        LogTarget::Sink
    } else {
        // Headless (no TTY): the TUI sink would swallow every log line,
        // so fall back to stderr instead of going silent.
        LogTarget::Stderr
    }
}

fn apply_cli_overrides(cli: &Cli, config: &mut AppConfig) {
    if let Some(api_key) = &cli.api_key {
        config.client.api_key = Some(api_key.clone());
    }
    if let Some(base_url) = &cli.base_url {
        config.client.base_url = base_url.clone();
    }
    if let Some(model) = &cli.model {
        config.agent.model = model.clone();
    }
    if let Some(max_iterations) = cli.max_iterations {
        config.agent.max_iterations = max_iterations;
    }
    if let Some(timeout) = cli.tool_timeout {
        config.agent.tool_timeout_secs = timeout;
    }
    if let Some(timeout) = cli.request_timeout {
        config.agent.request_timeout_secs = timeout;
        config.client.timeout_secs = timeout;
    }
    if let Some(window) = cli.context_window {
        config.agent.context_window = window;
        config.client.max_context_length = window;
    }
    if let Some(healing) = cli.max_healing_attempts {
        config.agent.max_healing_attempts = healing;
    }
    if cli.stream {
        config.agent.stream = true;
    }
    if cli.no_stream {
        config.agent.stream = false;
    }
}

fn client_config(config: &AppConfig) -> Result<(ProviderKind, ClientConfig)> {
    let kind = ProviderKind::from_name(&config.client.provider).ok_or_else(|| {
        anyhow::anyhow!(
            "Unsupported client provider '{}'. Expected one of: openai, anthropic, ollama, openrouter, gemini, nous.",
            config.client.provider
        )
    })?;
    let resolved = kerux_core::client::resolve_provider_settings(&config.client)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let client = ClientConfig {
        base_url: resolved.config.base_url,
        api_key: resolved.config.api_key,
        timeout: resolved.config.timeout,
        max_context_length: resolved.max_context_length,
    };
    if let Some(auth_ref) = config.client.auth_ref.as_deref() {
        let store = AuthStore::load_default()?;
        let client = apply_auth_profile_to_client(client, &store, auth_ref)?;
        return Ok((infer_provider_from_base_url(&client.base_url), client));
    }
    Ok((kind, client))
}

/// Resolve the runtime provider, refreshing an OAuth profile's access token
/// (and persisting the rotated tokens) when needed.
async fn runtime_client_with_store(
    config: &AppConfig,
    mut store: AuthStore,
) -> Result<(ProviderKind, ClientConfig)> {
    let auth_ref = config
        .client
        .auth_ref
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Missing auth_ref for OAuth profile"))?;
    let access_token = store.resolve_oauth_token(auth_ref).await?;
    let profile = store
        .profiles
        .get(auth_ref)
        .ok_or_else(|| anyhow::anyhow!("Auth profile '{}' disappeared", auth_ref))?;
    let inference_base_url = profile
        .oauth
        .as_ref()
        .map(|t| t.inference_base_url.clone())
        .or_else(|| profile.base_url.clone())
        .unwrap_or_else(|| kerux_core::auth::NOUS_INFERENCE_URL.to_string());
    let client = ClientConfig {
        base_url: inference_base_url,
        api_key: Some(access_token),
        timeout: Duration::from_secs(config.client.timeout_secs.max(1)),
        max_context_length: config.client.max_context_length,
    };
    Ok((ProviderKind::Nous, client))
}

fn infer_provider_from_base_url(base_url: &str) -> ProviderKind {
    let normalized = base_url.trim_end_matches('/').to_ascii_lowercase();
    if normalized.contains(".anthropic.com") {
        ProviderKind::Anthropic
    } else if normalized.contains("openrouter.ai") {
        ProviderKind::Openrouter
    } else if normalized.contains("localhost:11434") || normalized.contains("127.0.0.1:11434") {
        ProviderKind::Ollama
    } else if normalized.contains("nousresearch.com") {
        ProviderKind::Nous
    } else {
        ProviderKind::Openai
    }
}

async fn runtime_client(config: &AppConfig) -> Result<Arc<dyn LLMProvider>> {
    // OAuth profiles need a live token refresh before the client is built.
    if let Some(auth_ref) = config.client.auth_ref.as_deref() {
        if let Ok(store) = AuthStore::load_default() {
            if store.profiles.get(auth_ref).map(|p| p.method) == Some(AuthMethod::Oauth) {
                let (kind, client) = runtime_client_with_store(config, store).await?;
                let primary = build_provider_for_kind(kind, client)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                return wrap_with_fallbacks(primary, config);
            }
        }
    }
    let (kind, client) = client_config(config)?;
    let primary = build_provider_for_kind(kind, client)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    wrap_with_fallbacks(primary, config)
}

/// Wrap the primary provider in a fallback chain when `[[client.fallback]]`
/// entries are configured. Opt-in only: an empty list returns the primary
/// untouched, so the client stays locked to the single configured provider
/// unless the user explicitly adds fallbacks.
fn wrap_with_fallbacks(
    primary: Arc<dyn LLMProvider>,
    config: &AppConfig,
) -> Result<Arc<dyn LLMProvider>> {
    if config.client.fallback.is_empty() {
        return Ok(primary);
    }

    let mut entries: Vec<kerux_core::client::FallbackEntry> = Vec::new();
    for fb in &config.client.fallback {
        let kind = ProviderKind::from_name(&fb.provider).ok_or_else(|| {
            anyhow::anyhow!(
                "Unsupported fallback provider '{}'. Expected one of: openai, anthropic, ollama, openrouter, gemini, nous.",
                fb.provider
            )
        })?;
        let client = ClientConfig {
            base_url: fb
                .base_url
                .clone()
                .unwrap_or_else(|| kerux_core::config::ClientSettings::default().base_url),
            api_key: fb.api_key.clone(),
            timeout: Duration::from_secs(fb.timeout_secs.unwrap_or(config.client.timeout_secs)),
            max_context_length: config.client.max_context_length,
        };
        let provider = build_provider_for_kind(kind, client)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        entries.push((provider, fb.model.clone()));
    }

    tracing::info!(fallbacks = entries.len(), "Fallback provider chain enabled");
    Ok(Arc::new(kerux_core::client::FallbackChainProvider::new(
        primary, entries,
    )))
}

fn apply_auth_profile_to_client(
    mut client: ClientConfig,
    store: &AuthStore,
    auth_ref: &str,
) -> Result<ClientConfig> {
    let profile = store.profiles.get(auth_ref).ok_or_else(|| {
        anyhow::anyhow!(
            "Missing required configuration: auth profile '{}'",
            auth_ref
        )
    })?;
    let trusted_base_url = profile
        .base_url
        .clone()
        .or_else(|| match profile.method {
            AuthMethod::ApiKey if is_openai_provider(&profile.provider) => {
                Some(kerux_core::config::ClientSettings::default().base_url)
            }
            AuthMethod::ApiKey => None,
            AuthMethod::BearerToken => None,
            AuthMethod::Oauth => profile.oauth.as_ref().map(|t| t.inference_base_url.clone()),
        })
        .ok_or_else(|| anyhow::anyhow!("Auth profile '{}' requires a base URL", auth_ref))?;
    let default_base_url = kerux_core::config::ClientSettings::default().base_url;
    if client.base_url != default_base_url && client.base_url != trusted_base_url {
        anyhow::bail!(
            "Auth profile '{}' is bound to '{}'; refusing to send credentials to '{}'",
            auth_ref,
            trusted_base_url,
            client.base_url
        );
    }
    match profile.method {
        AuthMethod::ApiKey => {
            client.api_key = Some(store.resolve_api_key(auth_ref)?);
        }
        AuthMethod::BearerToken => {
            client.api_key = Some(store.resolve_auth_token(auth_ref)?);
        }
        AuthMethod::Oauth => {
            // The live access token is minted/refreshed in runtime_client
            // before the provider is built; nothing to resolve here.
        }
    }
    client.base_url = trusted_base_url;
    Ok(client)
}

fn push_taste_profile(
    project_root: &std::path::Path,
    store: &dyn kerux_core::taste::TasteStore,
    name: &str,
) -> Result<()> {
    let path = kerux_core::taste::project_taste_path(project_root);
    let profile = kerux_core::persist::read_json::<kerux_core::taste::TasteProfile>(&path)
        .with_context(|| format!("No readable project taste profile at {}", path.display()))?;
    store
        .save(name, &profile)
        .with_context(|| format!("Failed to save taste profile '{name}'"))
}

fn pull_taste_profile(
    project_root: &std::path::Path,
    store: &dyn kerux_core::taste::TasteStore,
    name: &str,
) -> Result<()> {
    let remote = store
        .load(name)
        .with_context(|| format!("Taste profile '{name}' was not found or is unreadable"))?;
    let path = kerux_core::taste::project_taste_path(project_root);
    let mut local = kerux_core::persist::read_json::<kerux_core::taste::TasteProfile>(&path)
        .unwrap_or_else(|| kerux_core::taste::TasteProfile::new(name));
    local.merge(&remote);
    kerux_core::persist::write_json(&path, &local).with_context(|| {
        format!(
            "Failed to write project taste profile at {}",
            path.display()
        )
    })
}

fn agent_config(
    config: &AppConfig,
    behavior: &BehaviorSettings,
    system_prompt: Option<&str>,
) -> AgentConfig {
    let mut agent = AgentConfig::from(behavior);
    if let Some(prompt) = system_prompt {
        agent.system_prompt = Some(prompt.to_string());
    }
    agent.request_timeout = Duration::from_secs(config.agent.request_timeout_secs);
    agent.validation = config.validation.clone();
    agent.validation_workspace =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    agent
}

async fn run_configured_validation(
    config: &AppConfig,
    workspace: &std::path::Path,
) -> Result<kerux_core::validation::ValidationPassResult> {
    if !config.validation.enabled {
        anyhow::bail!("Project validation is disabled in [validation]");
    }
    config.validation.validate()?;
    kerux_core::validators::run_validation_pass(&config.validation, workspace, None)
        .await
        .map_err(Into::into)
}

pub(crate) async fn build_registry(
    config: &AppConfig,
    mcp_manager: &mut McpManager,
    client: &Arc<dyn LLMProvider>,
    model: &str,
) -> Result<ToolRegistry> {
    let registry = ToolRegistry::new(Duration::from_secs(config.tools.registry_timeout_secs));
    kerux_core::tools::register_builtin_tools(&registry).await?;
    if config.tools.delegation.enabled {
        registry
            .register(kerux_core::tools::SubAgentTool::with_concurrency(
                client.clone(),
                model,
                config.tools.delegation.max_concurrent,
            ))
            .await?;
    }
    registry.register(EchoTool::new()).await?;
    registry.register(CalculatorTool::new()).await?;

    if config.mcp.autoload {
        // First connect to explicitly configured servers
        for server in config.mcp.servers.iter().filter(|server| server.enabled) {
            if mcp_manager.get(&server.name).is_none() {
                connect_mcp_server(mcp_manager, server).await?;
            }
        }

        // Auto-discover servers from ~/.kerux/mcp.json and Claude Desktop config
        if let Err(e) = mcp_manager.auto_discover_and_connect().await {
            tracing::warn!(error = %e, "MCP auto-discovery encountered an issue");
        }

        for tool in mcp_manager.get_all_tools().await {
            registry.register(tool).await?;
        }
    }

    Ok(registry)
}

async fn connect_mcp_server(mcp_manager: &mut McpManager, server: &McpServerConfig) -> Result<()> {
    match server.transport {
        McpTransportKind::Http => {
            let url = server
                .url
                .clone()
                .context("Configured HTTP MCP server is missing a URL")?;
            mcp_manager
                .add_server(server.name.clone(), url, server.auth_token.clone())
                .await?;
        }
        McpTransportKind::Stdio => {
            let command = server
                .command
                .clone()
                .context("Configured stdio MCP server is missing a command")?;
            mcp_manager
                .add_stdio_server(
                    server.name.clone(),
                    command,
                    server.args.clone(),
                    server.env.clone(),
                )
                .await?;
        }
    }
    Ok(())
}

pub(crate) async fn create_runtime_agent(
    config: &AppConfig,
    behavior: &BehaviorSettings,
    system_prompt: Option<&str>,
    event_tx: mpsc::Sender<AgentEvent>,
    mcp_manager: &mut McpManager,
) -> Result<KeruxAgent> {
    let client = runtime_client(config).await?;
    let registry = build_registry(config, mcp_manager, &client, &behavior.model).await?;
    let agent_config = agent_config(config, behavior, system_prompt);
    let memory_manager = load_repo_memory_manager().await?;
    Ok(
        KeruxAgent::with_provider_events(agent_config, client, registry, event_tx)
            .with_memory_manager(memory_manager),
    )
}

async fn create_agent_without_events(
    config: &AppConfig,
    system_prompt: Option<&str>,
    mcp_manager: &mut McpManager,
) -> Result<KeruxAgent> {
    let client = runtime_client(config).await?;
    let registry = build_registry(config, mcp_manager, &client, &config.agent.model).await?;
    let agent_config = agent_config(config, &config.agent, system_prompt);
    let memory_manager = load_repo_memory_manager().await?;
    Ok(
        KeruxAgent::new_with_provider(agent_config, client, registry)
            .with_memory_manager(memory_manager),
    )
}

async fn load_repo_memory_manager() -> Result<MemoryManager> {
    let storage_dir = std::env::current_dir().context("Failed to determine current directory")?;
    let memory_manager = load_memory_manager(storage_dir).await?;
    // Curator pass runs in the background: decay/prune/distill without
    // blocking startup. Failures are logged and non-fatal. LLM-assisted skill
    // prose is enabled only when configured and a client can be built.
    let config = runtime_config();
    let policy = config.curator.clone();
    let skills_dir = config.skills.root_dir.clone();
    let curated = memory_manager.clone();
    let client_and_model = if policy.skill_distill_llm_summary {
        runtime_client(&config)
            .await
            .ok()
            .map(|client| (client, config.agent.model.clone()))
    } else {
        None
    };
    tokio::spawn(async move {
        let (client, model) = client_and_model
            .map(|(c, m)| (Some(c), Some(m)))
            .unwrap_or((None, None));
        match kerux_core::curator::curate_with_llm(&curated, &skills_dir, &policy, client, model)
            .await
        {
            Ok(report) if !report.is_empty() => {
                tracing::info!(?report, "Curator pass complete");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "Curator pass failed");
            }
        }
    });
    // Periodic mid-session passes when an interval is configured. Spawn once
    // per process: rebuild_agent re-invokes this loader.
    static PERIODIC_STARTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if !PERIODIC_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        kerux_core::curator::spawn_periodic_curator(
            memory_manager.clone(),
            config.skills.root_dir.clone(),
            config.curator.clone(),
        );
    }
    Ok(memory_manager)
}

async fn load_memory_manager(storage_dir: PathBuf) -> Result<MemoryManager> {
    let memory_manager = MemoryManager::with_storage_dir(storage_dir);
    memory_manager
        .load_from_disk()
        .await
        .context("Failed to load long-term memory")?;
    Ok(memory_manager)
}

async fn run_non_tui(config: &AppConfig, system_prompt: Option<&str>, query: &str) -> Result<()> {
    let mut mcp_manager = McpManager::new();
    let agent = create_agent_without_events(config, system_prompt, &mut mcp_manager).await?;
    let response = agent.run(query.to_string()).await?;
    println!("{}", response.content);
    Ok(())
}

async fn chat_non_tui(
    config: &AppConfig,
    system_prompt: Option<&str>,
    session_id: Option<&str>,
) -> Result<()> {
    let mut mcp_manager = McpManager::new();
    let agent = create_agent_without_events(config, system_prompt, &mut mcp_manager).await?;

    let channel_key = session_id.unwrap_or("cli:default");
    let store = kerux_core::session_store::SessionStore::new(
        kerux_core::session_store::SessionStore::default_dir(),
    );
    let session_data = store.load(channel_key);

    if !session_data.messages.is_empty() {
        println!(
            "Loaded {} message(s) from session '{}'.",
            session_data.messages.len(),
            channel_key
        );
        if let Some(summary) = &session_data.summary {
            agent
                .add_message(kerux_core::client::Message::system(format!(
                    "{}\n{}",
                    kerux_core::agent::CONTEXT_SUMMARY_MARKER,
                    summary
                )))
                .await;
        }
        for msg in &session_data.messages {
            agent.add_message(msg.clone()).await;
        }
    }

    loop {
        print!("You: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }
        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            let history = agent.conversation().await;
            let _ = store.save(channel_key, session_data.summary.as_deref(), &history);
            break;
        }
        if input.eq_ignore_ascii_case("clear") {
            agent.clear_history().await;
            store.clear(channel_key);
            println!("Conversation cleared.");
            continue;
        }

        match agent.run(input.to_string()).await {
            Ok(response) => {
                println!("Assistant: {}\n", response.content);
                let history = agent.conversation().await;
                let _ = store.save(channel_key, session_data.summary.as_deref(), &history);
            }
            Err(error) => eprintln!("Error: {}\n", error),
        }
    }

    Ok(())
}

/// Message handler that routes incoming platform messages to the Kerux agent
/// and streams live progress (status heartbeats, tool notifications) into the
/// channel while the run is in flight.
struct AgentMessageHandler {
    agent: Arc<KeruxAgent>,
    /// Stream model output live into the chat (SSE deltas → live message
    /// edits) instead of only showing a heartbeat until the reply is done.
    streaming_replies: bool,
    /// Require explicit approval before executing dangerous tools.
    tool_approval: bool,
    /// Seconds to wait for an approval decision before auto-denying.
    tool_approval_timeout_secs: u64,
    /// Rolling context compaction: summarize the oldest messages into a
    /// rolling summary instead of letting the message cap hard-truncate.
    context_compaction: bool,
    /// Recurring job scheduler for `/cron` commands.
    scheduler: Arc<kerux_core::scheduler::Scheduler>,
    /// Per-channel conversation persistence so the bot survives restarts.
    session_store: kerux_core::session_store::SessionStore,
    /// Channel key currently loaded into the shared agent's conversation.
    /// The agent has one conversation buffer, so switching channels swaps
    /// the history (clear + reload from the store).
    current_channel: tokio::sync::Mutex<Option<String>>,
    /// Serializes agent runs: the shared agent has one conversation buffer,
    /// so concurrent runs from different channels would corrupt each other.
    run_lock: tokio::sync::Mutex<()>,
}

/// Format an elapsed duration as a short human string ("45 sec", "3 min").
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{} sec", secs)
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        if rem == 0 {
            format!("{} min", mins)
        } else {
            format!("{} min {} sec", mins, rem)
        }
    }
}

fn compact_token_count(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn telemetry_cost(telemetry: &AgentTelemetry, settings: &TelemetrySettings) -> f64 {
    telemetry.estimated_cost_usd.unwrap_or_else(|| {
        (telemetry.prompt_tokens as f64 / 1_000_000.0) * settings.input_cost_per_million
            + (telemetry.completion_tokens as f64 / 1_000_000.0) * settings.output_cost_per_million
    })
}

fn currency_symbol(currency: &str) -> &str {
    match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        other => other,
    }
}

fn format_gateway_telemetry(telemetry: &AgentTelemetry, total_cost: f64, currency: &str) -> String {
    let throughput = telemetry
        .tokens_per_second
        .map(|value| format!("{value:.1} tok/s"))
        .unwrap_or_else(|| "measuring tok/s".to_string());
    let cache_rate = if telemetry.prompt_tokens == 0 {
        0.0
    } else {
        telemetry.cached_prompt_tokens as f64 / telemetry.prompt_tokens as f64 * 100.0
    };
    format!(
        "{throughput} · {} tok · {cache_rate:.1}% cache · {}{total_cost:.4} · turn {}",
        compact_token_count(telemetry.total_tokens),
        currency_symbol(currency),
        telemetry.turns_completed,
    )
}

/// Final outcome of a run, handed to the progress pump so it can replace
/// the live status message with the model's actual reply.
struct RunOutcome {
    /// The model's final reply, when the run succeeded.
    reply: Option<kerux_core::gateway::OutgoingMessage>,
    /// Whether the run was cancelled by the user.
    cancelled: bool,
    /// Error text, when the run failed for a non-cancel reason.
    error: Option<String>,
}

/// Live progress pump for one agent run.
///
/// Consumes [`AgentEvent`]s and reflects them into the channel via the sink:
/// a single editable status message carries the heartbeat ("⏳ Working — 3 min
/// — receiving stream response"), and each tool call gets its own message that
/// is edited in place when the tool completes. When the run finishes, the
/// status message is REPLACED by the model's actual reply (via `send_final`)
/// instead of leaving a "✅ Done in X" stub next to a separate answer — only
/// cancelled/failed runs keep a short indicator, since there is no reply to
/// show.
struct RunProgress {
    sink: Arc<dyn kerux_core::gateway::MessageSink>,
    channel_id: String,
    /// ID of the editable status message, once sent.
    status_msg_id: Option<String>,
    /// Human label for the current phase.
    phase: String,
    /// When the run started.
    started: Instant,
    /// Tool-call message IDs keyed by call_id, so ToolComplete can edit them.
    tool_msgs: HashMap<String, String>,
    /// Tool names keyed by call_id for the completion label.
    tool_names: HashMap<String, String>,
    /// Streaming mode: live-edit the reply into the chat as tokens arrive
    /// (SSE deltas) instead of only showing a heartbeat.
    streaming: bool,
    /// Accumulated text of the current stream segment (since the last tool
    /// call or run start).
    stream_buffer: String,
    /// Live message carrying the current stream segment.
    stream_msg_id: Option<String>,
    /// When the live message was last edited (Telegram rate-limit throttle).
    last_stream_edit: Instant,
    telemetry: AgentTelemetry,
    total_cost: f64,
    telemetry_settings: TelemetrySettings,
}

/// Minimum interval between live stream edits. Telegram rate-limits edits
/// per chat (~20 msgs/min); 1.5s keeps us safely under while still feeling
/// live.
const STREAM_EDIT_INTERVAL: Duration = Duration::from_millis(1500);

/// Telegram's hard message cap is 4096 chars; keep live views under this.
const LIVE_VIEW_MAX_CHARS: usize = 4000;

/// Build a live-view string: `text` + `suffix`, truncated at the head if it
/// would exceed the Telegram cap.
fn live_view(text: &str, suffix: &str) -> String {
    let suffix_chars = suffix.chars().count();
    if text.chars().count() + suffix_chars <= LIVE_VIEW_MAX_CHARS {
        return format!("{}{}", text, suffix);
    }
    let keep = LIVE_VIEW_MAX_CHARS.saturating_sub(suffix_chars + 1);
    let cut: String = text.chars().take(keep).collect();
    format!("{}…{}", cut, suffix)
}

impl RunProgress {
    fn new(
        sink: Arc<dyn kerux_core::gateway::MessageSink>,
        channel_id: String,
        streaming: bool,
        telemetry_settings: TelemetrySettings,
    ) -> Self {
        Self {
            sink,
            channel_id,
            status_msg_id: None,
            phase: "thinking".to_string(),
            started: Instant::now(),
            tool_msgs: HashMap::new(),
            tool_names: HashMap::new(),
            streaming,
            stream_buffer: String::new(),
            stream_msg_id: None,
            last_stream_edit: Instant::now()
                .checked_sub(STREAM_EDIT_INTERVAL)
                .unwrap_or_else(Instant::now),
            telemetry: AgentTelemetry::default(),
            total_cost: 0.0,
            telemetry_settings,
        }
    }

    /// Render the current status line.
    fn status_text(&self) -> String {
        let status = format!(
            "⏳ Working — {} — {}",
            format_elapsed(self.started.elapsed()),
            self.phase
        );
        if !self.telemetry_settings.enabled || self.telemetry.total_tokens == 0 {
            return status;
        }
        format!(
            "{status}\n{}",
            format_gateway_telemetry(
                &self.telemetry,
                self.total_cost,
                &self.telemetry_settings.currency,
            )
        )
    }

    fn stream_suffix(&self, cursor: &str) -> String {
        if !self.telemetry_settings.enabled || self.telemetry.total_tokens == 0 {
            return cursor.to_string();
        }
        format!(
            "\n\n{}{}",
            format_gateway_telemetry(
                &self.telemetry,
                self.total_cost,
                &self.telemetry_settings.currency,
            ),
            cursor,
        )
    }

    /// Send (or edit) the status message with the current heartbeat.
    async fn refresh_status(&mut self) {
        // In streaming mode the live message is owned by the stream view;
        // heartbeats must not clobber it.
        if self.streaming && self.stream_msg_id.is_some() {
            return;
        }
        let text = self.status_text();
        let msg = kerux_core::gateway::OutgoingMessage::new(&self.channel_id, text).no_markdown();
        match &self.status_msg_id {
            Some(id) => {
                // Best-effort edit; ignore "message is not modified" races.
                let _ = self.sink.edit(id, msg).await;
            }
            None => {
                if let Ok(id) = self.sink.send(msg).await {
                    self.status_msg_id = id;
                }
            }
        }
    }

    /// Streaming mode: append a content delta and refresh the live view when
    /// the throttle window allows. The live view is plain text (partial
    /// markdown would render broken) with a `▌` cursor.
    async fn on_content_delta(&mut self, text: String) {
        self.stream_buffer.push_str(&text);
        let now = Instant::now();
        if now.duration_since(self.last_stream_edit) < STREAM_EDIT_INTERVAL {
            return;
        }
        let suffix = self.stream_suffix(" ▌");
        self.render_stream_live(&suffix).await;
        self.last_stream_edit = now;
    }

    /// Send or edit the live stream message with `stream_buffer` + suffix.
    /// Reuses the heartbeat status message for the first segment so no extra
    /// message appears.
    async fn render_stream_live(&mut self, suffix: &str) {
        let view = live_view(&self.stream_buffer, suffix);
        let msg = kerux_core::gateway::OutgoingMessage::new(&self.channel_id, view).no_markdown();
        match &self.stream_msg_id {
            Some(id) => {
                let _ = self.sink.edit(id, msg).await;
            }
            None => {
                // Reuse the heartbeat message when one exists; otherwise send
                // a fresh live message (already carrying the current view).
                let id = match self.status_msg_id.take() {
                    Some(id) => {
                        let _ = self.sink.edit(&id, msg).await;
                        id
                    }
                    None => match self.sink.send(msg).await {
                        Ok(Some(id)) => id,
                        _ => return,
                    },
                };
                self.stream_msg_id = Some(id);
            }
        }
    }

    /// Freeze the current stream segment: drop the cursor and detach the live
    /// message so the next segment (after a tool call) starts a fresh one.
    async fn freeze_stream_segment(&mut self) {
        if self.stream_buffer.trim().is_empty() {
            self.stream_buffer.clear();
            return;
        }
        self.render_stream_live("").await;
        self.stream_buffer.clear();
        self.stream_msg_id = None;
    }

    /// Finalize the run: replace the live status message with the model's
    /// actual reply, or — when there is no reply (cancel/error) — with a
    /// short indicator. Tool-call messages are left as-is; they carry real
    /// information the user wants to keep.
    async fn finish(&mut self, outcome: RunOutcome) {
        // In streaming mode the live message is the stream message; fall back
        // to the heartbeat status message otherwise.
        let target_id = if self.streaming {
            self.stream_msg_id
                .clone()
                .or_else(|| self.status_msg_id.clone())
        } else {
            self.status_msg_id.clone()
        };

        if let Some(reply) = outcome.reply {
            // Replace the live message with the final MarkdownV2 reply.
            if let Err(e) = self
                .sink
                .send_final(target_id.as_deref(), reply.clone())
                .await
            {
                // MarkdownV2 is strict; retry without parse_mode, then as a
                // fresh message, so the reply is never lost.
                tracing::warn!(error = %e, "send_final failed; retrying without markdown");
                let plain = reply.no_markdown();
                if let Err(e2) = self
                    .sink
                    .send_final(target_id.as_deref(), plain.clone())
                    .await
                {
                    tracing::warn!(error = %e2, "plain send_final failed; sending as new message");
                    let _ = self.sink.send(plain).await;
                }
            }
            return;
        }

        // No reply to show: keep a short terminal indicator in place of the
        // heartbeat so the user knows what happened to the run.
        let text = if outcome.cancelled {
            format!(
                "🛑 Stopped after {}",
                format_elapsed(self.started.elapsed())
            )
        } else if let Some(err) = outcome.error {
            format!("❌ Error: {}", err)
        } else {
            "(no response)".to_string()
        };
        let msg = kerux_core::gateway::OutgoingMessage::new(&self.channel_id, text).no_markdown();
        match &target_id {
            Some(id) => {
                let _ = self.sink.edit(id, msg).await;
            }
            None => {
                let _ = self.sink.send(msg).await;
            }
        }
    }

    /// Handle one agent event, updating the live progress display.
    async fn on_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Thinking { .. } => {
                self.phase = "thinking".to_string();
                self.refresh_status().await;
            }
            AgentEvent::Reasoning { .. } => {
                self.phase = "reasoning".to_string();
            }
            AgentEvent::Content { text } => {
                self.phase = "receiving stream response".to_string();
                if self.streaming {
                    self.on_content_delta(text).await;
                }
            }
            AgentEvent::ToolStart {
                call_id,
                name,
                arguments,
            } => {
                self.phase = format!("running tool: {}", name);
                self.tool_names.insert(call_id.clone(), name.clone());
                // Streaming mode: freeze the text segment accumulated so far
                // into a permanent message before the tool notification.
                if self.streaming {
                    self.freeze_stream_segment().await;
                }
                // Char-safe truncation: byte slicing (&arguments[..120])
                // panics when the cut lands inside a multi-byte UTF-8
                // sequence — tool arguments routinely contain non-ASCII
                // text, and a panic here kills the progress pump.
                let preview = if arguments.chars().count() > 120 {
                    let cut: String = arguments.chars().take(120).collect();
                    format!("{}...", cut)
                } else {
                    arguments
                };
                let text = format!("🔧 Running tool: {}\n`{}`", name, preview);
                let msg = kerux_core::gateway::OutgoingMessage::new(&self.channel_id, text);
                if let Ok(Some(id)) = self.sink.send(msg).await {
                    self.tool_msgs.insert(call_id, id);
                }
                self.refresh_status().await;
            }
            AgentEvent::ToolComplete { result } => {
                let label = self
                    .tool_names
                    .get(&result.tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                let text = if result.success {
                    format!("✅ {} done", label)
                } else {
                    format!("❌ {} failed", label)
                };
                let msg =
                    kerux_core::gateway::OutgoingMessage::new(&self.channel_id, text).no_markdown();
                if let Some(id) = self.tool_msgs.remove(&result.tool_call_id) {
                    let _ = self.sink.edit(&id, msg).await;
                }
                self.phase = "thinking".to_string();
                self.refresh_status().await;
            }
            AgentEvent::ToolError { name, .. } => {
                self.phase = "thinking".to_string();
                let text = format!("❌ {} failed", name);
                let msg =
                    kerux_core::gateway::OutgoingMessage::new(&self.channel_id, text).no_markdown();
                let _ = self.sink.send(msg).await;
            }
            AgentEvent::IterationComplete { .. } => {
                self.phase = "thinking".to_string();
            }
            AgentEvent::Telemetry { telemetry } => {
                if self.telemetry_settings.enabled {
                    if telemetry.billable {
                        self.total_cost += telemetry_cost(&telemetry, &self.telemetry_settings);
                    }
                    self.telemetry = telemetry;
                    if self.streaming && self.stream_msg_id.is_some() {
                        let suffix = self.stream_suffix(" ▌");
                        self.render_stream_live(&suffix).await;
                    } else {
                        self.refresh_status().await;
                    }
                }
            }
            AgentEvent::BudgetAlert { reason, .. } => {
                self.phase = "budget alert".to_string();
                let text = format!("⚠️ Budget: {}", reason);
                let msg =
                    kerux_core::gateway::OutgoingMessage::new(&self.channel_id, text).no_markdown();
                let _ = self.sink.send(msg).await;
                self.refresh_status().await;
            }
            AgentEvent::Done { .. } | AgentEvent::Error { .. } => {
                // Terminal / metadata events are handled by the run loop.
            }
        }
    }
}

impl AgentMessageHandler {
    /// Handle `/cron` subcommands. Returns the reply text (plain, sent
    /// with markdown disabled so ids/intervals render literally).
    async fn handle_cron_command(&self, message: &kerux_core::gateway::IncomingMessage) -> String {
        use kerux_core::scheduler;

        let args = message
            .content
            .trim()
            .strip_prefix("/cron")
            .unwrap_or("")
            .trim();

        // /cron  (bare) or /cron list
        if args.is_empty() || args.eq_ignore_ascii_case("list") {
            let jobs = self.scheduler.list().await;
            if jobs.is_empty() {
                return "No cron jobs. Add one with:\n/cron add 30m <prompt>\n/cron add cron \"*/5 * * * *\" <prompt>\n/cron add once <timestamp> <prompt>".to_string();
            }
            let mut out = String::from("Cron jobs:\n");
            for job in &jobs {
                let state = if job.enabled { "on" } else { "paused" };
                let target_tag = match job.target {
                    scheduler::JobTarget::Agent => " [agent]",
                    scheduler::JobTarget::Prompt => "",
                };
                let sched_str = match &job.schedule {
                    scheduler::Schedule::Interval { seconds } => format!("every {}s", seconds),
                    scheduler::Schedule::Cron { expression } => format!("cron \"{}\"", expression),
                    scheduler::Schedule::Once { at } => format!("once @ {}", at),
                };
                out.push_str(&format!(
                    "  #{}{}[{}] {} -> \"{}\"\n",
                    job.id, target_tag, state, sched_str, job.prompt
                ));
            }
            out.push_str("\n/cron pause <id> | resume <id> | remove <id>");
            return out;
        }

        let mut parts = args.splitn(2, ' ');
        let sub = parts.next().unwrap_or("").to_lowercase();
        let rest = parts.next().unwrap_or("").trim();

        match sub.as_str() {
            "add" => {
                // /cron add [agent] <interval|cron|once> <prompt>
                let (target, rest_spec) = if rest.starts_with("agent ") {
                    (
                        scheduler::JobTarget::Agent,
                        rest.strip_prefix("agent ").unwrap_or("").trim(),
                    )
                } else {
                    (scheduler::JobTarget::Prompt, rest)
                };

                let (schedule, prompt) = match scheduler::parse_add_request(rest_spec) {
                    Ok(parsed) => parsed,
                    Err(e) => return format!("Bad schedule syntax: {}", e),
                };

                match self
                    .scheduler
                    .add_schedule(
                        prompt.to_string(),
                        schedule.clone(),
                        target,
                        message.platform.clone(),
                        message.channel_id.clone(),
                    )
                    .await
                {
                    Ok(job) => {
                        let sched_str = match &job.schedule {
                            scheduler::Schedule::Interval { seconds } => {
                                format!("every {}s", seconds)
                            }
                            scheduler::Schedule::Cron { expression } => {
                                format!("cron \"{}\"", expression)
                            }
                            scheduler::Schedule::Once { at } => format!("once @ {}", at),
                        };
                        format!(
                            "Scheduled job #{}: {} -> \"{}\"",
                            job.id, sched_str, job.prompt
                        )
                    }
                    Err(e) => format!("Failed to add job: {}", e),
                }
            }
            "pause" | "resume" => {
                let id: u64 = match rest.parse() {
                    Ok(v) => v,
                    Err(_) => return format!("Usage: /cron {} <id>", sub),
                };
                let enabled = sub == "resume";
                match self.scheduler.set_enabled(id, enabled).await {
                    Ok(true) => {
                        format!("Job #{} {}", id, if enabled { "resumed" } else { "paused" })
                    }
                    Ok(false) => format!("No job #{}", id),
                    Err(e) => format!("Failed: {}", e),
                }
            }
            "remove" | "rm" | "delete" => {
                let id: u64 = match rest.parse() {
                    Ok(v) => v,
                    Err(_) => return "Usage: /cron remove <id>".to_string(),
                };
                match self.scheduler.remove(id).await {
                    Ok(Some(job)) => format!("Removed job #{} (\"{}\")", job.id, job.prompt),
                    Ok(None) => format!("No job #{}", id),
                    Err(e) => format!("Failed: {}", e),
                }
            }
            other => format!(
                "Unknown subcommand '{}'. Use: add, list, pause, resume, remove",
                other
            ),
        }
    }
}

#[async_trait::async_trait]
impl kerux_core::gateway::MessageHandler for AgentMessageHandler {
    async fn handle(
        &self,
        message: kerux_core::gateway::IncomingMessage,
        sink: Arc<dyn kerux_core::gateway::MessageSink>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> kerux_core::error::Result<()> {
        tracing::info!(
            platform = %message.platform,
            user = %message.user_id,
            "Handling incoming message"
        );

        let channel_key = format!("{}:{}", message.platform, message.channel_id);

        // /cron — manage recurring jobs.
        //   /cron add <interval> <prompt>   schedule a job in this channel
        //   /cron list                      show all jobs
        //   /cron pause <id> | resume <id>  toggle a job
        //   /cron remove <id>               delete a job
        if message.content.trim().eq_ignore_ascii_case("/cron")
            || message.content.trim().starts_with("/cron ")
        {
            let reply = self.handle_cron_command(&message).await;
            let msg =
                kerux_core::gateway::OutgoingMessage::new(&message.channel_id, reply).no_markdown();
            let _ = sink.send(msg).await;
            return Ok(());
        }

        // /new — wipe this channel's session and start fresh.
        if message.content.trim().eq_ignore_ascii_case("/new") {
            // Hold the run lock so an in-flight run can't re-save the
            // session after we clear it.
            let _run_guard = self.run_lock.lock().await;
            self.session_store.clear(&channel_key);
            let mut current = self.current_channel.lock().await;
            if current.as_deref() == Some(channel_key.as_str()) {
                self.agent.clear_history().await;
                *current = None;
            }
            drop(current);
            drop(_run_guard);
            let msg = kerux_core::gateway::OutgoingMessage::new(
                &message.channel_id,
                "🧹 Session cleared. Starting fresh.",
            )
            .no_markdown();
            let _ = sink.send(msg).await;
            return Ok(());
        }

        // Serialize runs: the shared agent has one conversation buffer.
        let _run_guard = self.run_lock.lock().await;

        // Swap conversation history when the channel changed since the last
        // run (or after a restart, when nothing is loaded yet).
        {
            let mut current = self.current_channel.lock().await;
            if current.as_deref() != Some(channel_key.as_str()) {
                self.agent.clear_history().await;
                let data = self.session_store.load(&channel_key);
                // Re-embed the rolling summary as the marker system message
                // so the model sees it before the recent tail.
                if let Some(summary) = data.summary {
                    self.agent
                        .add_message(kerux_core::client::Message::system(format!(
                            "{}\n{}",
                            kerux_core::agent::CONTEXT_SUMMARY_MARKER,
                            summary
                        )))
                        .await;
                }
                for m in data.messages {
                    self.agent.add_message(m).await;
                }
                *current = Some(channel_key.clone());
            }
        }

        // Per-run event channel. The shared agent swaps its sink for this run.
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
        self.agent.set_event_sender(Some(event_tx));

        // Per-run approval gate: dangerous tools pause and prompt this
        // channel's chat with approve/deny buttons.
        if self.tool_approval {
            let gate = kerux_core::gateway::SinkApprovalGate::new(
                sink.clone(),
                Duration::from_secs(self.tool_approval_timeout_secs),
                Some(channel_key.clone()),
            );
            self.agent.set_approval_gate(Some(Arc::new(gate)));
        }

        // Oneshot carrying the final outcome so the pump can replace the
        // live status message with the model's actual reply.
        let (outcome_tx, mut outcome_rx) = oneshot::channel::<RunOutcome>();

        // Spawn the live progress pump. It owns the sink clone and runs until
        // it receives the final outcome (sent after the agent run completes).
        let pump_sink = sink.clone();
        let pump_channel = message.channel_id.clone();
        let pump_streaming = self.streaming_replies;
        let telemetry_settings = runtime_config().telemetry;
        let pump = tokio::spawn(async move {
            let mut progress =
                RunProgress::new(pump_sink, pump_channel, pump_streaming, telemetry_settings);
            // Initial status so the user sees activity immediately.
            progress.refresh_status().await;

            let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            heartbeat.tick().await; // consume the immediate first tick

            // The event channel closes when the handler detaches the agent's
            // event sender. That close must NOT end the pump: the final
            // outcome may still be in flight, and breaking here would drop
            // the reply silently. Track the channel state and keep waiting
            // for the outcome (and heartbeats) until it actually arrives.
            let mut events_open = true;
            loop {
                tokio::select! {
                    maybe_event = event_rx.recv(), if events_open => {
                        match maybe_event {
                            Some(event) => progress.on_event(event).await,
                            // Channel closed: stop polling events but keep
                            // waiting for the outcome below.
                            None => {
                                events_open = false;
                            }
                        }
                    }
                    _ = heartbeat.tick() => {
                        progress.refresh_status().await;
                    }
                    // The receiver is polled by mutable reference so it
                    // survives branches that handle events/heartbeats.
                    outcome = &mut outcome_rx => {
                        // Run finished: replace the status message with the
                        // model's reply (or a short cancel/error indicator).
                        let outcome = outcome.unwrap_or(RunOutcome {
                            reply: None,
                            cancelled: false,
                            error: Some("internal error: outcome lost".to_string()),
                        });
                        progress.finish(outcome).await;
                        break;
                    }
                }
            }
        });

        // Run the agent with the gateway's cancellation flag.
        let run_result = self
            .agent
            .run_with_cancel(message.content.clone(), cancel)
            .await;

        // Rolling context compaction: when the buffer grows past the
        // trigger, summarize the oldest messages into a rolling summary
        // instead of letting the 200-message cap hard-truncate them.
        // Fail-open: compact_history logs and keeps history as-is on error.
        if self.context_compaction {
            if let Some(_summary) = self.agent.compact_history().await {
                tracing::info!("Context compacted; rolling summary updated");
            }
        }

        // Persist the conversation for this channel so it survives restarts.
        // Save even on cancel/error: partial history is better than losing
        // the whole session.
        {
            let conv = self.agent.conversation().await;
            let summary = self.agent.context_summary().await;
            if let Err(e) = self
                .session_store
                .save(&channel_key, summary.as_deref(), &conv)
            {
                tracing::warn!(error = %e, "Failed to persist session");
            }
        }

        // Hand the outcome to the pump; it replaces the status message.
        // Send BEFORE detaching the event sender: the pump's select! may
        // observe the closed event channel first, and we never want the
        // outcome to arrive after the pump has stopped listening.
        let outcome = match run_result {
            Ok(response) => {
                let reply = if response.content.trim().is_empty() {
                    None
                } else {
                    Some(kerux_core::gateway::OutgoingMessage::new(
                        &message.channel_id,
                        response.content,
                    ))
                };
                RunOutcome {
                    reply,
                    cancelled: false,
                    error: None,
                }
            }
            Err(kerux_core::error::Error::Cancelled) => RunOutcome {
                reply: None,
                cancelled: true,
                error: None,
            },
            Err(e) => RunOutcome {
                reply: None,
                cancelled: false,
                error: Some(e.to_string()),
            },
        };
        let was_cancelled = outcome.cancelled;
        let had_error = outcome.error.is_some();
        let _ = outcome_tx.send(outcome);
        // Now that the outcome is queued, detach the event sink so no
        // further events reach the pump.
        self.agent.set_event_sender(None);
        // Detach the per-run approval gate too (it borrows this run's sink).
        self.agent.set_approval_gate(None);
        if let Err(e) = pump.await {
            // A panicked pump means the final reply may never have been
            // rendered — surface it instead of dying silently.
            tracing::error!(error = %e, "progress pump task failed");
        }

        if was_cancelled {
            // Propagate so the gateway treats this as an expected stop.
            Err(kerux_core::error::Error::Cancelled)
        } else if had_error {
            // The pump already showed the error in place of the status
            // message; report Ok so the gateway doesn't add a generic
            // "something went wrong" on top of it.
            Ok(())
        } else {
            Ok(())
        }
    }
}

/// Run the multi-platform gateway, routing incoming messages to the agent.
async fn run_gateway(config: &AppConfig, system_prompt: Option<&str>) -> Result<()> {
    let mut mcp_manager = McpManager::new();
    let agent = create_agent_without_events(config, system_prompt, &mut mcp_manager).await?;
    let gateway_config = kerux_core::gateway::GatewayConfig::default();
    let scheduler = Arc::new(kerux_core::scheduler::Scheduler::new(
        kerux_core::scheduler::Scheduler::default_path(),
    ));
    let handler = Arc::new(AgentMessageHandler {
        agent: Arc::new(agent),
        streaming_replies: gateway_config.streaming_replies,
        tool_approval: gateway_config.tool_approval,
        tool_approval_timeout_secs: gateway_config.tool_approval_timeout_secs,
        context_compaction: gateway_config.context_compaction,
        scheduler: scheduler.clone(),
        session_store: kerux_core::session_store::SessionStore::new(
            kerux_core::session_store::SessionStore::default_dir(),
        ),
        current_channel: tokio::sync::Mutex::new(None),
        run_lock: tokio::sync::Mutex::new(()),
    });
    let mut gateway = kerux_core::gateway::Gateway::new(gateway_config.clone())
        .with_handler(handler)
        .with_scheduler(scheduler);

    if gateway_config.telegram_enabled {
        let mut telegram =
            kerux_core::gateway::TelegramAdapter::new(gateway_config.telegram_token.clone());
        // Voice-note STT: reuse the primary client's endpoint + key so no
        // extra credentials are needed. Disabled unless stt_model is set.
        if let Some(model) = gateway_config.stt_model.clone() {
            let resolved = kerux_core::client::resolve_provider_settings(&config.client)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            telegram = telegram.with_stt(kerux_core::gateway::SttConfig {
                base_url: resolved.config.base_url,
                api_key: resolved.config.api_key,
                model,
            });
        }
        gateway = gateway.with_adapter(Arc::new(telegram));
    }
    if gateway_config.discord_enabled {
        gateway = gateway.with_adapter(Arc::new(kerux_core::gateway::DiscordAdapter::new(
            gateway_config.discord_token.clone(),
        )));
    }
    if gateway_config.slack_enabled {
        gateway = gateway.with_adapter(Arc::new(kerux_core::gateway::SlackAdapter::new(
            gateway_config.slack_token.clone(),
            gateway_config.slack_signing_secret.clone(),
        )));
    }
    if gateway_config.whatsapp_enabled {
        gateway = gateway.with_adapter(Arc::new(kerux_core::gateway::WhatsAppAdapter::new(
            gateway_config.whatsapp_bridge_url.clone(),
        )));
    }

    println!("Starting Kerux gateway...");
    gateway.run().await?;
    Ok(())
}

async fn list_tools(config: &AppConfig, verbose: bool) -> Result<()> {
    let mut mcp_manager = McpManager::new();
    let client = runtime_client(config).await?;
    let registry = build_registry(config, &mut mcp_manager, &client, &config.agent.model).await?;
    let tools = registry.get_schemas().await;

    for tool in tools {
        println!("{}: {}", tool.name, tool.description);
        if verbose {
            println!("{}", serde_json::to_string_pretty(&tool.parameters)?);
        }
    }

    Ok(())
}

async fn test_tool(config: &AppConfig, tool_name: &str, args: Option<&str>) -> Result<()> {
    let mut mcp_manager = McpManager::new();
    let client = runtime_client(config).await?;
    let registry = build_registry(config, &mut mcp_manager, &client, &config.agent.model).await?;
    let parsed_args: Value = if let Some(args) = args {
        serde_json::from_str(args).context("Failed to parse tool arguments as JSON")?
    } else {
        Value::Object(serde_json::Map::new())
    };

    let result = registry
        .execute(
            tool_name,
            &format!("test_{}", tool_name),
            parsed_args,
            ToolContext::default(),
        )
        .await?;

    println!("success: {}", result.success);
    println!("content: {}", result.content);
    if let Some(error) = result.error {
        println!("error: {}", error);
    }

    Ok(())
}

async fn handle_auth_command(command: &AuthCommands) -> Result<()> {
    match command {
        AuthCommands::Providers => print_auth_providers(),
        AuthCommands::Login { provider } => {
            let provider = canonical_provider(provider)?;
            if provider.slug == "nous" {
                return login_nous().await;
            }
            print_auth_login_guidance(provider);
            anyhow::bail!(
                "Kerux does not run '{}' login flows yet; use the listed external credential source and create an auth profile with set-api-key or set-bearer-token.",
                provider.name
            );
        }
        AuthCommands::SetApiKey {
            provider,
            name,
            env_var,
            base_url,
        } => {
            let provider = canonical_provider(provider)?;
            if provider.name != "OpenAI"
                && base_url.as_deref().map(str::trim).unwrap_or("").is_empty()
            {
                anyhow::bail!(
                    "Provider '{}' API-key profiles require --base-url so credentials are bound to the intended endpoint.",
                    provider.name
                );
            }
            let profile_name = name
                .clone()
                .unwrap_or_else(|| format!("{}-default", provider.slug));
            let env_var = env_var
                .clone()
                .unwrap_or_else(|| provider.api_key_env.to_string());
            let mut store = AuthStore::load_default()?;
            store.upsert_api_key_env_profile(
                profile_name.clone(),
                provider.name.to_string(),
                env_var.clone(),
                base_url.clone(),
            )?;
            store.save_default()?;
            println!(
                "Saved auth profile '{}' for provider '{}' using env:{}",
                profile_name, provider.name, env_var
            );
            println!("Auth metadata: {}", default_auth_store_path().display());
        }
        AuthCommands::SetBearerToken {
            provider,
            name,
            env_var,
            base_url,
        } => {
            let provider = canonical_provider(provider)?;
            let default_env_var = provider.bearer_env;
            let profile_name = name
                .clone()
                .unwrap_or_else(|| format!("{}-default", provider.slug));
            let env_var = match (env_var, default_env_var) {
                (Some(env_var), _) => env_var.clone(),
                (None, Some(default_env_var)) => default_env_var.to_string(),
                (None, None) => anyhow::bail!(
                    "Provider '{}' does not have a default bearer-token OAuth flow. Use --env with a documented bearer token source if you know what you are doing.",
                    provider.name
                ),
            };
            let mut store = AuthStore::load_default()?;
            store.upsert_bearer_token_env_profile(
                profile_name.clone(),
                provider.name.to_string(),
                env_var.clone(),
                base_url.clone(),
            )?;
            store.save_default()?;
            println!(
                "Saved bearer auth profile '{}' for provider '{}' using env:{}",
                profile_name, provider.name, env_var
            );
            println!("Auth metadata: {}", default_auth_store_path().display());
        }
        AuthCommands::List => {
            let store = AuthStore::load_default()?;
            if store.profiles.is_empty() {
                println!("No auth profiles configured.");
                return Ok(());
            }
            for (name, profile) in store.profiles {
                let secret_ref = profile
                    .resolved_env_var()
                    .map(|env_var| display_auth_field(&format!("env:{}", env_var)))
                    .unwrap_or_else(|| "unsupported".to_string());
                let base_url = profile
                    .base_url
                    .as_deref()
                    .map(display_auth_field)
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{}\tprovider={}\tmethod={:?}\tsecret={}\tbase_url={}",
                    display_auth_field(&name),
                    display_auth_field(&profile.provider),
                    profile.method,
                    secret_ref,
                    base_url
                );
            }
        }
        AuthCommands::Logout { name } => {
            let mut store = AuthStore::load_default()?;
            if !store.remove_profile(name) {
                anyhow::bail!("Auth profile '{}' was not found", name);
            }
            store.save_default()?;
            println!("Removed auth profile '{}'", name);
        }
    }

    Ok(())
}

/// Run the Nous Portal device-code OAuth login and persist the resulting
/// tokens as an `OAuth` auth profile (`nous-default`).
async fn login_nous() -> Result<()> {
    use kerux_core::auth::{
        oauth_profile_from_token_response, poll_nous_device_token, request_nous_device_code,
        AuthMethod, AuthStore, NOUS_CLIENT_ID, NOUS_INFERENCE_URL, NOUS_PORTAL_URL, NOUS_SCOPE,
    };

    println!("Starting Nous Portal device-code login...");
    let device = request_nous_device_code(NOUS_PORTAL_URL, NOUS_CLIENT_ID, NOUS_SCOPE).await?;

    println!();
    println!("To complete login, open this URL in your browser and approve the request:");
    println!("  {}", device.verification_uri_complete);
    if !device.user_code.is_empty() {
        println!("  Your code: {}", device.user_code);
    }
    println!();
    println!(
        "Waiting for approval (expires in {}s)...",
        device.expires_in
    );

    let token = poll_nous_device_token(
        NOUS_PORTAL_URL,
        NOUS_CLIENT_ID,
        &device.device_code,
        device.expires_in,
        device.interval,
    )
    .await?;

    let profile = oauth_profile_from_token_response(
        "nous-default",
        "nous",
        &token,
        NOUS_PORTAL_URL,
        NOUS_INFERENCE_URL,
        NOUS_CLIENT_ID,
        NOUS_SCOPE,
    );
    if profile.method != AuthMethod::Oauth {
        anyhow::bail!("Internal error: generated a non-OAuth Nous profile");
    }

    let mut store = AuthStore::load_default()?;
    store.profiles.insert("nous-default".to_string(), profile);
    store.save_default()?;

    println!();
    println!("Saved Nous Portal OAuth profile 'nous-default'.");
    println!("Auth metadata: {}", default_auth_store_path().display());
    println!("Next: set `provider = \"nous\"` and `auth_ref = \"nous-default\"` in kerux.toml, then run `kerux chat`.");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderInfo {
    name: &'static str,
    slug: &'static str,
    aliases: &'static [&'static str],
    api_key_env: &'static str,
    api_key_envs: &'static [&'static str],
    bearer_env: Option<&'static str>,
    documented_auth: &'static [&'static str],
    kerux_sources: &'static [&'static str],
    notes: &'static str,
    login_guidance: &'static [&'static str],
}

const AUTH_PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        name: "Google",
        slug: "google",
        aliases: &[
            "google",
            "gemini",
            "google-gemini",
            "google ai studio",
            "google-ai-studio",
            "vertex",
            "vertex-ai",
            "google vertex ai",
        ],
        api_key_env: "GOOGLE_API_KEY",
        api_key_envs: &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        bearer_env: Some("GOOGLE_OAUTH_ACCESS_TOKEN"),
        documented_auth: &["API key", "OAuth/ADC bearer token", "service account/ADC"],
        kerux_sources: &["GOOGLE_API_KEY", "GEMINI_API_KEY", "GOOGLE_OAUTH_ACCESS_TOKEN"],
        notes: "Direct OAuth requires a Google OAuth client ID; ADC can be managed by gcloud.",
        login_guidance: &[
            "For API-key auth, create a Google AI Studio API key and run `kerux auth set-api-key Google --env GOOGLE_API_KEY --base-url <google-endpoint>`.",
            "For OAuth/ADC, run `gcloud auth application-default login`, export an access token through your own refresh workflow, then run `kerux auth set-bearer-token Google --env GOOGLE_OAUTH_ACCESS_TOKEN --base-url <google-endpoint>`.",
        ],
    },
    ProviderInfo {
        name: "GitHub Copilot",
        slug: "github-copilot",
        aliases: &["github-copilot", "github copilot", "copilot", "github"],
        api_key_env: "COPILOT_GITHUB_TOKEN",
        api_key_envs: &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
        bearer_env: Some("COPILOT_GITHUB_TOKEN"),
        documented_auth: &["OAuth device flow", "supported GitHub token", "GitHub CLI fallback"],
        kerux_sources: &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
        notes: "Kerux can reference tokens now; running Copilot login directly is future work.",
        login_guidance: &[
            "Run the official Copilot or GitHub CLI login flow, or provide a supported token in COPILOT_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN.",
            "Then run `kerux auth set-bearer-token GitHub-Copilot --env COPILOT_GITHUB_TOKEN --base-url <copilot-compatible-endpoint>` only for endpoints documented to accept that token.",
        ],
    },
    ProviderInfo {
        name: "OpenAI",
        slug: "openai",
        aliases: &["openai"],
        api_key_env: "OPENAI_API_KEY",
        api_key_envs: &["OPENAI_API_KEY"],
        bearer_env: None,
        documented_auth: &[
            "API key",
            "ChatGPT/Codex browser login",
            "ChatGPT/Codex device login",
        ],
        kerux_sources: &["OPENAI_API_KEY"],
        notes: "Kerux supports API-key metadata now; Codex/ChatGPT OAuth is documented but not wired into Kerux runtime yet.",
        login_guidance: &[
            "For current Kerux runtime use, create an OpenAI API key and run `kerux auth set-api-key OpenAI --env OPENAI_API_KEY`.",
            "OpenAI Codex/ChatGPT browser and device login are documented provider flows, but Kerux does not consume Codex account tokens yet.",
        ],
    },
    ProviderInfo {
        name: "Anthropic",
        slug: "anthropic",
        aliases: &[
            "anthropic",
            "claude",
            "anthropic console",
        ],
        api_key_env: "ANTHROPIC_API_KEY",
        api_key_envs: &["ANTHROPIC_API_KEY"],
        bearer_env: None,
        documented_auth: &[
            "Claude account login",
            "Anthropic Console API key",
            "Team/Enterprise account",
            "Vertex AI",
            "Amazon Bedrock",
            "Microsoft Foundry",
        ],
        kerux_sources: &["ANTHROPIC_API_KEY"],
        notes: "Kerux supports API-key metadata now; Claude account and cloud-provider flows need provider-specific clients before runtime use.",
        login_guidance: &[
            "For current Kerux runtime use, create an Anthropic Console API key and run `kerux auth set-api-key Anthropic --env ANTHROPIC_API_KEY --base-url <anthropic-compatible-endpoint>`.",
            "Claude account login, Team/Enterprise, Vertex AI, Amazon Bedrock, and Microsoft Foundry need provider-specific clients before Kerux can use those credentials directly.",
        ],
    },
    ProviderInfo {
        name: "Nous Portal",
        slug: "nous",
        aliases: &["nous", "nous portal", "nous-portal", "nous research", "nous-research"],
        api_key_env: "NOUS_API_KEY",
        api_key_envs: &["NOUS_API_KEY"],
        bearer_env: None,
        documented_auth: &["OAuth device-code flow", "subscription proxy token"],
        kerux_sources: &["kerux auth login nous"],
        notes: "Kerux runs the device-code OAuth flow; the access token is a short-lived invoke JWT stored in the auth store.",
        login_guidance: &[
            "Run `kerux auth login nous`, open the printed URL in a browser, approve the device code, and Kerux stores the OAuth tokens.",
            "Then set `[client] provider = \"nous\"` and `auth_ref = \"nous-default\"` (or CLI `--auth-ref nous-default`), and run `kerux chat`.",
        ],
    },
];

fn canonical_provider(input: &str) -> Result<&'static ProviderInfo> {
    let normalized = input.trim().to_ascii_lowercase();
    AUTH_PROVIDERS
        .iter()
        .find(|provider| provider.aliases.contains(&normalized.as_str()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unsupported provider '{}'. Run `kerux auth providers` for supported names.",
                input
            )
        })
}

fn print_auth_providers() {
    println!("Supported auth providers:");
    for provider in AUTH_PROVIDERS {
        println!(
            "{}\tslug={}\taliases={}\tapi_key_envs={}\tbearer_env={}\tdocumented_auth={}\tkerux_sources={}\tnotes={}",
            provider.name,
            provider.slug,
            provider.aliases.join(","),
            provider.api_key_envs.join(","),
            provider.bearer_env.unwrap_or("-"),
            provider.documented_auth.join(","),
            provider.kerux_sources.join(","),
            provider.notes
        );
    }
}

fn print_auth_login_guidance(provider: &ProviderInfo) {
    println!("{} login is not enabled in Kerux yet.", provider.name);
    println!("Documented auth: {}", provider.documented_auth.join(", "));
    println!(
        "Kerux-supported sources: {}",
        provider.kerux_sources.join(", ")
    );
    for line in provider.login_guidance {
        println!("- {}", line);
    }
}

fn is_openai_provider(provider: &str) -> bool {
    provider.eq_ignore_ascii_case("OpenAI") || provider.eq_ignore_ascii_case("openai")
}

fn display_auth_field(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

struct EchoTool;

impl EchoTool {
    fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl KeruxTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo back the input message. Useful for testing."
    }

    fn schema(&self) -> kerux_core::schema::ToolSchema {
        use schemars::JsonSchema;

        #[derive(JsonSchema, Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[allow(dead_code)]
        struct EchoArgs {
            message: String,
        }

        kerux_core::schema::ToolSchema::from_type::<EchoArgs>("echo", "Echo back the input message")
    }

    async fn execute(&self, args: Value, _context: ToolContext) -> kerux_core::tools::ToolResult {
        if let Some(msg) = args.get("message").and_then(|value| value.as_str()) {
            kerux_core::tools::ToolResult::success("echo", serde_json::json!({ "echoed": msg }))
        } else {
            kerux_core::tools::ToolResult::error("echo", "Missing 'message' argument")
        }
    }
}

struct CalculatorTool;

impl CalculatorTool {
    fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl KeruxTool for CalculatorTool {
    fn name(&self) -> &str {
        "calculate"
    }

    fn description(&self) -> &str {
        "Perform a calculation. Supports add, subtract, multiply, and divide."
    }

    fn schema(&self) -> kerux_core::schema::ToolSchema {
        use schemars::JsonSchema;

        #[derive(JsonSchema, Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[allow(dead_code)]
        struct CalcArgs {
            operation: String,
            a: f64,
            b: f64,
        }

        kerux_core::schema::ToolSchema::from_type::<CalcArgs>("calculate", "Perform calculations")
    }

    async fn execute(&self, args: Value, _context: ToolContext) -> kerux_core::tools::ToolResult {
        let operation = args
            .get("operation")
            .and_then(|value| value.as_str())
            .unwrap_or("add");
        let a = args
            .get("a")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        let b = args
            .get("b")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);

        let result = match operation {
            "add" | "+" => a + b,
            "subtract" | "-" => a - b,
            "multiply" | "*" | "x" => a * b,
            "divide" | "/" => {
                if b == 0.0 {
                    return kerux_core::tools::ToolResult::error("calculate", "Division by zero");
                }
                a / b
            }
            _ => {
                return kerux_core::tools::ToolResult::error(
                    "calculate",
                    format!("Unknown operation: {}", operation),
                )
            }
        };

        kerux_core::tools::ToolResult::success(
            "calculate",
            serde_json::json!({
                "operation": operation,
                "operand_a": a,
                "operand_b": b,
                "result": result
            }),
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut loaded = load_app_config(cli.config.as_deref())?;
    loaded.config.apply_env_overrides()?;
    apply_cli_overrides(&cli, &mut loaded.config);
    install_runtime_config(loaded.config.clone());

    // Fresh install: no config file anywhere and no provider credentials in
    // the environment — offer the interactive wizard before commands that
    // need a working LLM. Cancelling continues with defaults (the run then
    // fails with the usual missing-key error).
    if loaded.source.is_none()
        && !wizard::env_has_credentials()
        && std::io::stdin().is_terminal()
        && matches!(
            cli.command,
            Commands::Run { .. }
                | Commands::Chat { .. }
                | Commands::Serve { .. }
                | Commands::Autonomous { .. }
                | Commands::Session {
                    command: session::SessionCommands::Resume { .. },
                }
        )
    {
        println!("No config file found — launching the setup wizard.");
        wizard::run_wizard().await?;
        // Reload so the freshly written config drives this run.
        loaded = load_app_config(cli.config.as_deref())?;
        loaded.config.apply_env_overrides()?;
        apply_cli_overrides(&cli, &mut loaded.config);
        install_runtime_config(loaded.config.clone());
    }

    init_logging(
        cli.verbose,
        cli.log_level.as_deref(),
        &loaded.config.logging,
        loaded.config.tui.rich_output,
    );

    match &cli.command {
        Commands::Run {
            system,
            query,
            autonomous,
        } => {
            if *autonomous {
                if query.is_some() {
                    anyhow::bail!(
                        "Do not combine 'run --autonomous' with '--query'. Autonomous mode reads TODO.md from the workspace."
                    );
                }
                autonomous::run_autonomous(loaded.config.clone(), system.clone()).await?;
                return Ok(());
            }
            let query = query
                .as_ref()
                .context("No query provided. Use --query or start chat mode.")?;
            if loaded.config.tui.rich_output {
                TuiApp::enter(
                    loaded.config.clone(),
                    system.clone(),
                    LaunchMode::Query(query.clone()),
                )
                .await?
                .run()
                .await?;
            } else {
                run_non_tui(&loaded.config, system.as_deref(), query).await?;
            }
        }
        Commands::Chat { system, session } => {
            if loaded.config.tui.rich_output {
                TuiApp::enter(loaded.config.clone(), system.clone(), LaunchMode::Landing)
                    .await?
                    .run()
                    .await?;
            } else {
                chat_non_tui(&loaded.config, system.as_deref(), session.as_deref()).await?;
            }
        }
        Commands::Serve { system } => {
            run_gateway(&loaded.config, system.as_deref()).await?;
        }
        Commands::Tools { verbose } => {
            list_tools(&loaded.config, *verbose).await?;
        }
        Commands::Autonomous { system } => {
            autonomous::run_autonomous(loaded.config.clone(), system.clone()).await?;
        }
        Commands::Test { tool_name, args } => {
            test_tool(&loaded.config, tool_name, args.as_deref()).await?;
        }
        Commands::Validate => {
            let workspace =
                std::env::current_dir().context("Failed to determine current project directory")?;
            let result = run_configured_validation(&loaded.config, &workspace).await?;
            for validator in &result.results {
                println!("{}: {}", validator.name, validator.outcome.as_str());
            }
            if !result.passed {
                anyhow::bail!("Project validation failed");
            }
        }
        Commands::Auth { command } => {
            handle_auth_command(command).await?;
        }
        Commands::Taste { command } => {
            let project_root =
                std::env::current_dir().context("Failed to determine current project directory")?;
            let store = kerux_core::taste::FileTasteStore::at_default_root();
            match command {
                TasteCommands::Push { name } => {
                    push_taste_profile(&project_root, &store, name)?;
                    println!("Pushed project taste profile as '{name}'.");
                }
                TasteCommands::Pull { name } => {
                    pull_taste_profile(&project_root, &store, name)?;
                    println!("Pulled taste profile '{name}' into this project.");
                }
            }
        }
        Commands::Runs { command } => {
            runs::handle(command)?;
        }
        Commands::Session { command } => {
            session::handle(&loaded.config, command, None).await?;
        }
        Commands::Wizard => {
            wizard::run_wizard().await?;
        }
        Commands::Model { model_id, refresh } => {
            wizard::switch_model(model_id.clone(), *refresh).await?;
        }
        Commands::Screenshot { out } => {
            screenshot::capture(&loaded.config, out)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kerux_core::auth::AuthProfile;

    #[test]
    fn validate_subcommand_parses() {
        assert!(Cli::try_parse_from(["kerux", "validate"]).is_ok());
    }

    #[test]
    fn chat_subcommand_parses() {
        assert!(Cli::try_parse_from(["kerux", "chat"]).is_ok());
        assert!(Cli::try_parse_from(["kerux", "chat", "--session", "custom_session"]).is_ok());
    }

    #[test]
    fn session_subcommand_parses() {
        assert!(Cli::try_parse_from(["kerux", "session", "list"]).is_ok());
        assert!(Cli::try_parse_from(["kerux", "session", "resume", "test_id"]).is_ok());
        assert!(Cli::try_parse_from(["kerux", "session", "resume", "test_id", "--cli"]).is_ok());
        assert!(Cli::try_parse_from(["kerux", "session", "export", "test_id"]).is_ok());
        assert!(Cli::try_parse_from([
            "kerux",
            "session",
            "export",
            "test_id",
            "--format",
            "json",
            "-o",
            "test.json"
        ])
        .is_ok());
    }

    #[tokio::test]
    async fn validate_command_runs_configured_policy() {
        let workspace = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.validation.enabled = true;
        config
            .validation
            .validators
            .push(kerux_core::validation::ValidatorSpec {
                name: "pass".to_string(),
                command: passing_command(),
                required: true,
                timeout_secs: 10,
                workdir: None,
                output_cap_bytes: kerux_core::validation::DEFAULT_OUTPUT_CAP_BYTES,
            });

        let result = run_configured_validation(&config, workspace.path())
            .await
            .unwrap();

        assert!(result.passed);
    }

    #[cfg(windows)]
    fn passing_command() -> String {
        "cmd /c exit 0".to_string()
    }

    #[cfg(unix)]
    fn passing_command() -> String {
        "true".to_string()
    }

    #[test]
    fn cli_parses_taste_push_and_pull() {
        let push = Cli::try_parse_from(["kerux", "taste", "push", "team"]).unwrap();
        assert!(matches!(
            push.command,
            Commands::Taste {
                command: TasteCommands::Push { ref name }
            } if name == "team"
        ));

        let pull = Cli::try_parse_from(["kerux", "taste", "pull", "team"]).unwrap();
        assert!(matches!(
            pull.command,
            Commands::Taste {
                command: TasteCommands::Pull { ref name }
            } if name == "team"
        ));
    }

    #[test]
    fn taste_push_saves_project_profile_to_registry() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let store = kerux_core::taste::FileTasteStore::new(dir.path().join("registry"));
        let mut profile = kerux_core::taste::TasteProfile::new("project");
        profile
            .preferences
            .push(kerux_core::taste::TastePreference {
                key: "formatter".to_string(),
                category: kerux_core::taste::PreferenceCategory::Tooling,
                value: "rustfmt".to_string(),
                positive: 10,
                negative: 0,
                confidence: 2.0 / 3.0,
                source: kerux_core::taste::PreferenceSource::Extracted,
                first_observed_at: 1,
                last_observed_at: 2,
            });
        kerux_core::persist::write_json(&kerux_core::taste::project_taste_path(&project), &profile)
            .unwrap();

        push_taste_profile(&project, &store, "team").unwrap();

        assert_eq!(
            kerux_core::taste::TasteStore::load(&store, "team"),
            Some(profile)
        );
    }

    #[test]
    fn taste_pull_merges_registry_profile_into_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let store = kerux_core::taste::FileTasteStore::new(dir.path().join("registry"));
        let mut local = kerux_core::taste::TasteProfile::new("project");
        local.apply_observation(&kerux_core::taste::PreferenceObservation {
            key: "formatter".to_string(),
            category: kerux_core::taste::PreferenceCategory::Tooling,
            value: "rustfmt".to_string(),
            supports: true,
            weight: 2,
            source: kerux_core::taste::PreferenceSource::Extracted,
            observed_at: 1,
        });
        kerux_core::persist::write_json(&kerux_core::taste::project_taste_path(&project), &local)
            .unwrap();

        let mut remote = kerux_core::taste::TasteProfile::new("team");
        remote.apply_observation(&kerux_core::taste::PreferenceObservation {
            key: "formatter".to_string(),
            category: kerux_core::taste::PreferenceCategory::Tooling,
            value: "rustfmt".to_string(),
            supports: true,
            weight: 3,
            source: kerux_core::taste::PreferenceSource::Extracted,
            observed_at: 2,
        });
        remote.apply_observation(&kerux_core::taste::PreferenceObservation {
            key: "test runner".to_string(),
            category: kerux_core::taste::PreferenceCategory::Testing,
            value: "cargo nextest".to_string(),
            supports: true,
            weight: 1,
            source: kerux_core::taste::PreferenceSource::Extracted,
            observed_at: 2,
        });
        kerux_core::taste::TasteStore::save(&store, "team", &remote).unwrap();

        pull_taste_profile(&project, &store, "team").unwrap();

        let merged: kerux_core::taste::TasteProfile =
            kerux_core::persist::read_json(&kerux_core::taste::project_taste_path(&project))
                .unwrap();
        assert_eq!(merged.find("formatter").unwrap().positive, 5);
        assert_eq!(merged.find("test runner").unwrap().value, "cargo nextest");
    }

    #[test]
    fn gateway_telemetry_formats_live_session_metrics() {
        let telemetry = AgentTelemetry {
            prompt_tokens: 1_000,
            completion_tokens: 250,
            total_tokens: 1_250,
            context_window: 10_000,
            tokens_per_second: Some(42.5),
            turns_completed: 3,
            cached_prompt_tokens: 400,
            ..AgentTelemetry::default()
        };

        let text = format_gateway_telemetry(&telemetry, 0.0045, "USD");

        assert!(text.contains("42.5 tok/s"));
        assert!(text.contains("1.2k tok"));
        assert!(text.contains("40.0% cache"));
        assert!(text.contains("$0.0045"));
        assert!(text.contains("turn 3"));
    }

    #[test]
    fn infers_provider_wire_format_from_auth_profile_endpoint() {
        assert_eq!(
            infer_provider_from_base_url("https://api.anthropic.com/v1"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            infer_provider_from_base_url("https://openrouter.ai/api/v1"),
            ProviderKind::Openrouter
        );
        assert_eq!(
            infer_provider_from_base_url("http://localhost:11434/v1"),
            ProviderKind::Ollama
        );
        assert_eq!(
            infer_provider_from_base_url("https://example.invalid/v1"),
            ProviderKind::Openai
        );
    }

    #[test]
    fn rich_tui_on_tty_uses_sink() {
        let logging = LoggingSettings::default();
        assert_eq!(select_log_target(&logging, true, true), LogTarget::Sink);
    }

    #[test]
    fn rich_tui_headless_falls_back_to_stderr() {
        // No TTY (systemd, cron, pipes): the TUI sink would swallow every
        // log line, so headless runs must log to stderr.
        let logging = LoggingSettings::default();
        assert_eq!(select_log_target(&logging, true, false), LogTarget::Stderr);
    }

    #[test]
    fn log_file_overrides_sink() {
        let logging = LoggingSettings {
            log_file: Some("kerux.log".to_string()),
            ..Default::default()
        };
        assert_eq!(select_log_target(&logging, true, true), LogTarget::File);
    }

    #[tokio::test]
    async fn load_memory_manager_reads_existing_memory_file() {
        let dir =
            std::env::temp_dir().join(format!("kerux_cli_memory_load_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let seed = MemoryManager::with_storage_dir(dir.clone());
        seed.store(
            kerux_core::memory::MemoryBlock::new("cli_fact", "fact", "Loaded memory fact")
                .importance(90),
        )
        .await;

        let loaded = load_memory_manager(dir.clone()).await.unwrap();

        assert_eq!(loaded.search("Loaded memory").await.len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn autonomous_subcommand_parses() {
        let cli = Cli::try_parse_from(["kerux", "autonomous"]).unwrap();
        assert!(matches!(cli.command, Commands::Autonomous { .. }));
    }

    #[test]
    fn run_autonomous_flag_parses() {
        let cli = Cli::try_parse_from(["kerux", "run", "--autonomous"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Run {
                autonomous: true,
                ..
            }
        ));
    }

    #[test]
    fn auth_set_api_key_subcommand_parses() {
        let cli = Cli::try_parse_from([
            "kerux",
            "auth",
            "set-api-key",
            "openai",
            "--name",
            "openai-default",
            "--env",
            "OPENAI_API_KEY",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Commands::Auth {
                command: AuthCommands::SetApiKey { .. }
            }
        ));
    }

    #[test]
    fn auth_set_bearer_token_subcommand_parses() {
        let cli = Cli::try_parse_from([
            "kerux",
            "auth",
            "set-bearer-token",
            "google-gemini",
            "--name",
            "google-default",
            "--env",
            "GOOGLE_OAUTH_ACCESS_TOKEN",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Commands::Auth {
                command: AuthCommands::SetBearerToken { .. }
            }
        ));
    }

    #[test]
    fn auth_providers_subcommand_parses() {
        let cli = Cli::try_parse_from(["kerux", "auth", "providers"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Auth {
                command: AuthCommands::Providers
            }
        ));
    }

    #[test]
    fn auth_login_subcommand_parses() {
        let cli = Cli::try_parse_from(["kerux", "auth", "login", "OpenAI"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::Auth {
                command: AuthCommands::Login { .. }
            }
        ));
    }

    #[tokio::test]
    async fn auth_login_guidance_does_not_create_profile() {
        let result = handle_auth_command(&AuthCommands::Login {
            provider: "OpenAI".to_string(),
        })
        .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not run 'OpenAI' login flows yet"));
    }

    #[test]
    fn canonical_provider_names_and_defaults_are_supported() {
        let google = canonical_provider("gemini").unwrap();
        assert_eq!(google.name, "Google");
        assert_eq!(google.slug, "google");
        assert_eq!(google.api_key_env, "GOOGLE_API_KEY");
        assert!(google.api_key_envs.contains(&"GEMINI_API_KEY"));
        assert_eq!(google.bearer_env, Some("GOOGLE_OAUTH_ACCESS_TOKEN"));
        assert!(google.documented_auth.contains(&"OAuth/ADC bearer token"));
        assert!(google
            .login_guidance
            .iter()
            .any(|line| line.contains("gcloud")));

        let copilot = canonical_provider("copilot").unwrap();
        assert_eq!(copilot.name, "GitHub Copilot");
        assert_eq!(copilot.bearer_env, Some("COPILOT_GITHUB_TOKEN"));
        assert!(copilot.api_key_envs.contains(&"GH_TOKEN"));
        assert_eq!(
            canonical_provider("GitHub Copilot").unwrap().slug,
            "github-copilot"
        );

        let openai = canonical_provider("openai").unwrap();
        assert_eq!(openai.name, "OpenAI");
        assert!(openai
            .documented_auth
            .contains(&"ChatGPT/Codex device login"));
        assert!(openai
            .login_guidance
            .iter()
            .any(|line| line.contains("does not consume Codex account tokens")));
        assert!(canonical_provider("codex").is_err());
        assert!(!openai.kerux_sources.contains(&"CODEX_ACCESS_TOKEN"));
        assert_eq!(openai.bearer_env, None);

        let anthropic = canonical_provider("claude").unwrap();
        assert_eq!(anthropic.name, "Anthropic");
        assert_eq!(anthropic.api_key_env, "ANTHROPIC_API_KEY");
        assert!(anthropic.documented_auth.contains(&"Amazon Bedrock"));
        assert!(canonical_provider("claude-code").is_err());
        assert!(canonical_provider("microsoft foundry").is_err());
        assert_eq!(anthropic.bearer_env, None);
    }

    #[test]
    fn auth_profile_takes_precedence_over_configured_api_key() {
        let mut store = AuthStore::default();
        let old_profile_key = std::env::var("KERUX_TEST_PROFILE_KEY_PRECEDENCE").ok();
        std::env::set_var("KERUX_TEST_PROFILE_KEY_PRECEDENCE", "profile-key");
        store
            .upsert_api_key_env_profile(
                "openai-default",
                "openai",
                "KERUX_TEST_PROFILE_KEY_PRECEDENCE",
                None,
            )
            .unwrap();
        let client = ClientConfig {
            api_key: Some("configured-key".to_string()),
            ..ClientConfig::default()
        };

        let resolved = apply_auth_profile_to_client(client, &store, "openai-default").unwrap();

        assert_eq!(resolved.api_key.as_deref(), Some("profile-key"));

        match old_profile_key {
            Some(value) => std::env::set_var("KERUX_TEST_PROFILE_KEY_PRECEDENCE", value),
            None => std::env::remove_var("KERUX_TEST_PROFILE_KEY_PRECEDENCE"),
        }
    }

    #[test]
    fn auth_profile_base_url_applies_when_default_base_url_is_unset() {
        let mut store = AuthStore::default();
        let old_profile_key = std::env::var("KERUX_TEST_PROFILE_KEY_BASE_URL").ok();
        std::env::set_var("KERUX_TEST_PROFILE_KEY_BASE_URL", "profile-key");
        store
            .upsert_api_key_env_profile(
                "local-default",
                "local",
                "KERUX_TEST_PROFILE_KEY_BASE_URL",
                Some("http://127.0.0.1:11434/v1".to_string()),
            )
            .unwrap();
        let client = ClientConfig {
            api_key: Some("configured-key".to_string()),
            ..ClientConfig::default()
        };

        let resolved = apply_auth_profile_to_client(client, &store, "local-default").unwrap();

        assert_eq!(resolved.api_key.as_deref(), Some("profile-key"));
        assert_eq!(resolved.base_url, "http://127.0.0.1:11434/v1");

        match old_profile_key {
            Some(value) => std::env::set_var("KERUX_TEST_PROFILE_KEY_BASE_URL", value),
            None => std::env::remove_var("KERUX_TEST_PROFILE_KEY_BASE_URL"),
        }
    }

    #[test]
    fn auth_profile_rejects_untrusted_base_url_override() {
        let mut store = AuthStore::default();
        store
            .upsert_api_key_env_profile("openai-default", "openai", "OPENAI_API_KEY", None)
            .unwrap();
        let client = ClientConfig {
            base_url: "https://attacker.example/v1".to_string(),
            api_key: Some("configured-key".to_string()),
            ..ClientConfig::default()
        };

        let result = apply_auth_profile_to_client(client, &store, "openai-default");

        assert!(result.is_err());
    }

    #[test]
    fn non_openai_api_key_profile_requires_base_url() {
        let mut store = AuthStore::default();
        store
            .upsert_api_key_env_profile("google-default", "Google", "GOOGLE_API_KEY", None)
            .unwrap();
        let client = ClientConfig {
            api_key: Some("configured-key".to_string()),
            ..ClientConfig::default()
        };

        let result = apply_auth_profile_to_client(client, &store, "google-default");

        assert!(result.is_err());
    }

    #[test]
    fn bearer_profile_overrides_ambient_api_key_for_bound_endpoint() {
        let mut store = AuthStore::default();
        store
            .upsert_bearer_token_env_profile(
                "google-default",
                "google-gemini",
                "GOOGLE_OAUTH_ACCESS_TOKEN",
                Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
            )
            .unwrap();
        let old_token = std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN").ok();
        std::env::set_var("GOOGLE_OAUTH_ACCESS_TOKEN", "google-token");
        let client = ClientConfig {
            api_key: Some("openai-key".to_string()),
            ..ClientConfig::default()
        };

        let resolved = apply_auth_profile_to_client(client, &store, "google-default").unwrap();

        assert_eq!(resolved.api_key.as_deref(), Some("google-token"));
        assert_eq!(
            resolved.base_url,
            "https://generativelanguage.googleapis.com/v1beta"
        );
        if let Some(old_token) = old_token {
            std::env::set_var("GOOGLE_OAUTH_ACCESS_TOKEN", old_token);
        } else {
            std::env::remove_var("GOOGLE_OAUTH_ACCESS_TOKEN");
        }
    }

    #[test]
    fn bearer_profile_without_base_url_is_rejected_even_if_loaded_from_disk() {
        let mut store = AuthStore::default();
        store.profiles.insert(
            "broken-bearer".to_string(),
            AuthProfile {
                provider: "google-gemini".to_string(),
                method: AuthMethod::BearerToken,
                base_url: None,
                secret_ref: "env:GOOGLE_OAUTH_ACCESS_TOKEN".to_string(),
                disabled: false,
                oauth: None,
            },
        );
        let client = ClientConfig {
            api_key: Some("openai-key".to_string()),
            ..ClientConfig::default()
        };

        let result = apply_auth_profile_to_client(client, &store, "broken-bearer");

        assert!(result.is_err());
    }

    #[test]
    fn auth_list_fields_escape_control_characters() {
        assert_eq!(display_auth_field("good\nspoof"), "good?spoof");
    }
}
