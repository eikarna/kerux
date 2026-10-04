//! # Kerux Core Library
//!
//! A high-performance Rust implementation of the Kerux-Agent orchestration loop.
//! Supports asynchronous tool execution, streaming-first architecture, and
//! dynamic JSON-schema generation.
//!
//! ## Key Features
//!
//! - **Streaming-First**: Detect and execute tool calls incrementally from partial LLM outputs
//! - **Tool System**: 17+ built-in tools for file ops, terminal, web, code execution, memory, and more
//! - **Self-Healing**: Re-prompt LLM with error context on tool execution failures
//! - **Context Compression**: Automatic compression of long conversations to fit context window
//! - **Memory System**: Persistent file-backed memory with MEMORY.md/USER.md storage
//! - **Trajectory Saving**: Export conversation trajectories for RL training
//! - **Multi-Platform Gateway**: Support for Telegram, Discord, Slack, and more
//! - **MCP Client**: Model Context Protocol client (HTTP + stdio) for extended capabilities
//! - **Skills System**: Skill discovery, loading, and management from SKILL.md directories
//! - **Cross-Platform**: Windows (PowerShell/cmd), macOS, Linux with automatic shell detection
//! - **Structured Logging**: Comprehensive observability via the `tracing` crate
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │ KeruxAgent │
//! │ ┌─────────────┐ ┌──────────────┐ ┌────────────────────┐ │
//! │ │ OpenAI │ │ XMLParser │ │ ToolRegistry │ │
//! │ │ Client │ │ (Tolerant) │ │ & 17+ Tools │ │
//! │ └─────────────┘ └──────────────┘ └────────────────────┘ │
//! │ ┌─────────────────────────────────────────────────────────┐│
//! │ │ Orchestration Loop (ReAct) ││
//! │ │ Think → Plan → Execute Tools → Observe → Respond ││
//! │ └─────────────────────────────────────────────────────────┘│
//! │ ┌───────────────┐ ┌──────────────┐ ┌────────────────────┐│
//! │ │ Context Mgr │ │ Memory Mgr │ │ Trajectory Mgr ││
//! │ └───────────────┘ └──────────────┘ └────────────────────┘│
//! └─────────────────────────────────────────────────────────────┘
//! │                     Gateway & MCP Support                  │
//! └─────────────────────────────────────────────────────────────┘
//! ```

#![allow(clippy::double_must_use)]

use std::sync::{Mutex, MutexGuard};

/// Locks a synchronous mutex and recovers its guarded data after poisoning.
///
/// Poisoning records that another thread panicked while holding the mutex; it
/// does not make the guarded value inaccessible. Recovering here prevents
/// security-sensitive state from silently becoming `None` or a default value.
pub(crate) fn lock_sync<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("recovering state from a poisoned mutex");
        poisoned.into_inner()
    })
}

pub mod agent;
pub mod approval;
pub mod auth;
pub mod capability;
pub mod capsule;
pub mod client;
pub mod config;
pub mod context;
pub mod context_files;
pub mod cost;
pub mod curator;
pub mod distillation;
pub mod edit_metrics;
pub mod error;
pub mod gateway;
pub mod githarness;
pub mod mcp;
pub mod memory;
pub mod parser;
pub mod persist;
pub mod platform;
pub mod probe;
pub mod redaction;
pub mod repomap;
pub mod run_journal;
pub mod scheduler;
pub mod schema;
pub mod security_guard;
pub mod session_store;
pub mod skills;
pub mod taste;
pub mod taste_extraction;
pub mod tools;
pub mod trajectory;
pub mod validation;
pub mod validators;
pub mod webui;

pub use agent::{AgentConfig, AgentEvent, AgentTelemetry, KeruxAgent};
pub use auth::{
    build_oauth_authorization_url, default_auth_store_path, exchange_oauth_authorization_code,
    generate_oauth_state, generate_pkce_challenge, parse_loopback_authorization_code, AuthMethod,
    AuthProfile, AuthStore, LoopbackOAuthReceiver, OAuthTokenResponse, PkceChallenge,
};
pub use capability::{
    classify, Capability, CapabilityEntry, CapabilityReport, CapabilitySource, CapabilityStatus,
};
pub use client::{
    build_provider_client, build_provider_for_kind, chat_streaming_with_provider,
    chat_with_provider, discover_models, discover_models_or_empty, resolve_provider_settings,
    AnthropicClient, ClientConfig, EditFormat, ImageContent, LLMProvider, Message, ModelCache,
    ModelInfo, OpenAIClient, ProviderCapabilities, ProviderClient, ProviderConfig, ProviderKind,
    ProviderSettings,
};
pub use config::{
    install_runtime_config, load_app_config, parse_config_file, runtime_config, AppConfig,
    AutonomousSettings, BehaviorSettings, ClientSettings, CodeExecutionSettings, GatewaySettings,
    HttpToolSettings, LoadedConfig, LoggingSettings, McpServerConfig, McpSettings, SkillsSettings,
    TasteSettings, TelemetrySettings, TerminalSettings, ToolSettings, TuiSettings, WebToolSettings,
};
pub use context::{estimate_tokens, ContextConfig, ContextManager};
pub use context_files::{
    load_context_dir, load_default_context_files, load_workspace_context, scan_context_content,
};
pub use cost::{BudgetAction, CostGuardrail, GuardrailVerdict};
pub use curator::{curate, CurationPolicy, CurationReport};
pub use distillation::{distill_session_to_memory, distill_session_with_provider};
pub use error::{Error, Result};
pub use gateway::{
    DiscordAdapter, Gateway, GatewayConfig, IncomingMessage, MessageHandler, OutgoingMessage,
    PlatformAdapter, SlackAdapter, SttConfig, TelegramAdapter, WhatsAppAdapter,
};
pub use githarness::{GitHarness, RepoSnapshot};
pub use mcp::{McpClient, McpStdioClient, McpTool, McpTransport};
pub use memory::{MemoryBlock, MemoryManager, MemorySource, Session, UserProfile};
pub use parser::ToolCallParser;
pub use platform::PlatformInfo;
pub use probe::{probe_model, probe_streaming, probe_tools, probe_vision, ProbeResult};
pub use repomap::{
    discover_source_files, extract_file_tags, rank_and_render, Language as RepoMapLanguage,
    MinimalRepoMap, RepoMapRenderer, RepoTag, TagKind,
};
pub use skills::{write_skill_metadata, Skill, SkillManager, SkillOrigin, ARCHIVE_DIR_NAME};
pub use taste::{
    compute_confidence, project_taste_path, FileTasteStore, PreferenceCategory,
    PreferenceExtractor, PreferenceObservation, PreferenceSource, TastePreference, TasteProfile,
    TasteStore, HALF_SATURATION, TASTE_SCHEMA_VERSION,
};
pub use taste_extraction::{TrajectoryPreferenceExtractor, DEFAULT_MAX_REPEATS};
pub use tools::{
    register_builtin_tools, register_builtin_tools_with_provider_sub_agent,
    register_builtin_tools_with_sub_agent, KeruxTool, ToolRegistry, ToolResult,
};
pub use trajectory::{Trajectory, TrajectoryBuilder, TrajectoryExporter};
