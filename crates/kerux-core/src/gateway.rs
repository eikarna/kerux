//! Multi-platform gateway for Kerux
//!
//! Provides unified messaging interface across multiple platforms including
//! Telegram, Discord, Slack, WhatsApp, and more.

use async_trait::async_trait;
use reqwest::header::HeaderValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::runtime_config;
use crate::error::Result;

/// Configuration for the gateway
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Enable Telegram bot
    pub telegram_enabled: bool,
    /// Telegram bot token
    pub telegram_token: Option<String>,
    /// Enable Discord bot
    pub discord_enabled: bool,
    /// Discord bot token
    pub discord_token: Option<String>,
    /// Enable Slack bot
    pub slack_enabled: bool,
    /// Slack bot token
    pub slack_token: Option<String>,
    /// Slack signing secret used to verify Events API requests
    pub slack_signing_secret: Option<String>,
    /// Enable WhatsApp via the Baileys bridge
    pub whatsapp_enabled: bool,
    /// Base URL of the Baileys WhatsApp bridge
    pub whatsapp_bridge_url: Option<String>,
    /// Enable webhooks
    pub webhooks_enabled: bool,
    /// Webhook listen address
    pub webhooks_addr: Option<String>,
    /// Enable Hermes WebUI / Hermex HTTP API listener
    pub webui_enabled: bool,
    /// WebUI listen address (e.g. "127.0.0.1:8787")
    pub webui_addr: String,
    /// Optional password for WebUI access (required if non-loopback)
    pub webui_password: String,
    /// Default admin users (user IDs that can access admin commands)
    pub admins: Vec<String>,
    /// Stream model output live into the chat (edit message as tokens arrive)
    pub streaming_replies: bool,
    /// Require explicit approval before executing dangerous tools.
    pub tool_approval: bool,
    /// Seconds to wait for an approval decision before auto-denying.
    pub tool_approval_timeout_secs: u64,
    /// Model for voice-note transcription (`None` disables STT).
    pub stt_model: Option<String>,
    /// Roll oldest messages into a summary instead of dropping them at cap.
    pub context_compaction: bool,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        let settings = runtime_config().gateway;
        Self {
            telegram_enabled: settings.telegram_enabled,
            telegram_token: settings.telegram_token,
            discord_enabled: settings.discord_enabled,
            discord_token: settings.discord_token,
            slack_enabled: settings.slack_enabled,
            slack_token: settings.slack_token,
            slack_signing_secret: settings.slack_signing_secret,
            whatsapp_enabled: settings.whatsapp_enabled,
            whatsapp_bridge_url: settings.whatsapp_bridge_url,
            webhooks_enabled: settings.webhooks_enabled,
            webhooks_addr: settings.webhooks_addr,
            webui_enabled: settings.webui_enabled,
            webui_addr: settings.webui_addr,
            webui_password: settings.webui_password,
            admins: settings.admins,
            streaming_replies: settings.streaming_replies,
            tool_approval: settings.tool_approval,
            tool_approval_timeout_secs: settings.tool_approval_timeout_secs,
            stt_model: settings.stt_model,
            context_compaction: settings.context_compaction,
        }
    }
}

/// Incoming message from a platform
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// Platform source (e.g., "telegram", "discord", "slack")
    pub platform: String,
    /// User ID on the platform
    pub user_id: String,
    /// Username or display name
    pub username: String,
    /// Channel/chat ID
    pub channel_id: String,
    /// Message content
    pub content: String,
    /// Original raw message (platform-specific)
    pub raw: serde_json::Value,
    /// Timestamp
    pub timestamp: i64,
}

impl IncomingMessage {
    /// Create a new incoming message
    pub fn new(
        platform: impl Into<String>,
        user_id: impl Into<String>,
        username: impl Into<String>,
        channel_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            platform: platform.into(),
            user_id: user_id.into(),
            username: username.into(),
            channel_id: channel_id.into(),
            content: content.into(),
            raw: serde_json::json!({}),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }
    }

    /// Set the raw message
    pub fn with_raw(mut self, raw: serde_json::Value) -> Self {
        self.raw = raw;
        self
    }
}

/// Outgoing message to a platform
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    /// Target channel/chat ID
    pub channel_id: String,
    /// Message content (markdown or plain text)
    pub content: String,
    /// Whether to parse markdown
    pub parse_markdown: bool,
    /// Reply to message ID (if any)
    pub reply_to: Option<String>,
}

impl OutgoingMessage {
    /// Create a new outgoing message
    pub fn new(channel_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            content: content.into(),
            parse_markdown: true,
            reply_to: None,
        }
    }

    /// Disable markdown parsing
    pub fn no_markdown(mut self) -> Self {
        self.parse_markdown = false;
        self
    }

    /// Set reply-to message ID
    pub fn with_reply_to(mut self, message_id: impl Into<String>) -> Self {
        self.reply_to = Some(message_id.into());
        self
    }
}

/// Trait for platform adapters
///
/// Implement this trait to add support for a new messaging platform.
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    /// Get the platform name (e.g., "telegram", "discord")
    fn name(&self) -> &str;

    /// Check if the adapter is enabled and configured
    fn is_enabled(&self) -> bool;

    /// Start the adapter (e.g., start polling or webhooks)
    async fn start(&self) -> Result<()>;

    /// Stop the adapter
    async fn stop(&self) -> Result<()>;

    /// Send a message through the platform
    async fn send_message(&self, message: OutgoingMessage) -> Result<()>;

    /// Send a message and return the platform's message ID when available.
    ///
    /// Platforms that expose a message ID on send (Telegram) return it so
    /// callers can edit the message in place later (status heartbeats,
    /// tool progress updates). The default implementation falls back to
    /// [`Self::send_message`] with no ID.
    async fn send_message_tracked(&self, message: OutgoingMessage) -> Result<Option<String>> {
        self.send_message(message).await?;
        Ok(None)
    }

    /// Edit a previously sent message in place.
    ///
    /// Used for live status updates (elapsed-time heartbeats, tool progress)
    /// without flooding the chat. Platforms without edit support keep the
    /// default no-op.
    async fn edit_message(
        &self,
        _channel_id: &str,
        _message_id: &str,
        _message: OutgoingMessage,
    ) -> Result<()> {
        Ok(())
    }

    /// Deliver the final reply of a run, replacing the live status message
    /// (`message_id`) in place when provided.
    ///
    /// Default: edit the status message, or send fresh when there is none.
    /// Platforms with message-size limits (Telegram) override this to chunk
    /// the reply: first chunk edits the status message, the rest are sent as
    /// new messages.
    async fn send_final(
        &self,
        channel_id: &str,
        message_id: Option<&str>,
        message: OutgoingMessage,
    ) -> Result<()> {
        match message_id {
            Some(id) => self.edit_message(channel_id, id, message).await,
            None => self.send_message(message).await,
        }
    }

    /// Handle an incoming update (webhook or poll result)
    async fn handle_update(&self, update: serde_json::Value) -> Result<Option<IncomingMessage>>;

    /// Handle an interactive callback (e.g. Telegram inline-keyboard button
    /// presses for tool approval). Default: ignore.
    async fn handle_callback_query(&self, _update: serde_json::Value) -> Result<()> {
        Ok(())
    }

    /// Present a tool-approval prompt with approve/deny buttons and return
    /// `(request_id, decision_receiver)`. [`Self::handle_callback_query`]
    /// resolves the receiver when the human presses a button.
    ///
    /// Default: platforms without interactive buttons auto-approve so the
    /// agent never hangs waiting for input that can never arrive.
    async fn send_approval_prompt(
        &self,
        _channel_id: &str,
        _tool_name: &str,
        _arguments_preview: &str,
    ) -> Result<(
        u64,
        tokio::sync::oneshot::Receiver<crate::approval::ApprovalChoice>,
    )> {
        let (id, rx) = register_pending_approval(_tool_name, _arguments_preview);
        resolve_pending_approval(id, crate::approval::ApprovalChoice::AllowOnce, None);
        Ok((id, rx))
    }

    /// Poll for new updates (long-polling for platforms that support it).
    ///
    /// Returns a batch of raw updates. The default implementation returns an
    /// empty batch for event-based platforms (e.g. Slack) that rely on
    /// webhooks instead of polling.
    async fn poll_updates(&self) -> Result<Vec<serde_json::Value>> {
        Ok(Vec::new())
    }

    /// Get the adapter's specific configuration as JSON
    fn config_json(&self) -> serde_json::Value;
}

/// A sink for sending/replying into one specific channel.
///
/// Handed to the message handler so it can emit progress updates (status
/// heartbeats, tool notifications) mid-run, not just the final reply.
#[async_trait]
pub trait MessageSink: Send + Sync {
    /// Send a message; returns the platform message ID when available.
    async fn send(&self, message: OutgoingMessage) -> Result<Option<String>>;

    /// Edit a previously sent message in place (best-effort).
    async fn edit(&self, message_id: &str, message: OutgoingMessage) -> Result<()>;

    /// Deliver the final reply of a run, reusing the live status message
    /// (`status_msg_id`) when one exists so the "⏳ Working…" placeholder is
    /// replaced by the actual response instead of leaving a "✅ Done" stub
    /// plus a separate reply. Falls back to a plain send when there is no
    /// status message to reuse.
    async fn send_final(&self, status_msg_id: Option<&str>, message: OutgoingMessage)
        -> Result<()>;

    /// Present a tool-approval prompt into this channel and return
    /// `(request_id, decision_receiver)`. Default: auto-approve (platforms
    /// without interactive buttons must never stall the agent).
    async fn request_approval(
        &self,
        _tool_name: &str,
        _arguments_preview: &str,
    ) -> Result<(
        u64,
        tokio::sync::oneshot::Receiver<crate::approval::ApprovalChoice>,
    )> {
        let (id, rx) = register_pending_approval(_tool_name, _arguments_preview);
        resolve_pending_approval(id, crate::approval::ApprovalChoice::AllowOnce, None);
        Ok((id, rx))
    }
}

/// Sink bound to one adapter + channel pair.
pub struct ChannelSink {
    adapter: Arc<dyn PlatformAdapter>,
    channel_id: String,
}

impl ChannelSink {
    /// Create a sink targeting one channel on one adapter.
    pub fn new(adapter: Arc<dyn PlatformAdapter>, channel_id: impl Into<String>) -> Self {
        Self {
            adapter,
            channel_id: channel_id.into(),
        }
    }
}

#[async_trait]
impl MessageSink for ChannelSink {
    async fn send(&self, message: OutgoingMessage) -> Result<Option<String>> {
        self.adapter.send_message_tracked(message).await
    }

    async fn edit(&self, message_id: &str, message: OutgoingMessage) -> Result<()> {
        self.adapter
            .edit_message(&self.channel_id, message_id, message)
            .await
    }

    async fn send_final(
        &self,
        status_msg_id: Option<&str>,
        message: OutgoingMessage,
    ) -> Result<()> {
        self.adapter
            .send_final(&self.channel_id, status_msg_id, message)
            .await
    }

    async fn request_approval(
        &self,
        tool_name: &str,
        arguments_preview: &str,
    ) -> Result<(
        u64,
        tokio::sync::oneshot::Receiver<crate::approval::ApprovalChoice>,
    )> {
        self.adapter
            .send_approval_prompt(&self.channel_id, tool_name, arguments_preview)
            .await
    }
}

/// Approval gate bound to one channel's sink.
///
/// Installed per-run on the agent by the gateway handler: dangerous tools
/// pause execution, the prompt appears in the originating chat, and the run
/// resumes when the human presses a button — or auto-denies after `timeout`.
pub struct SinkApprovalGate {
    sink: Arc<dyn MessageSink>,
    timeout: std::time::Duration,
    channel_key: Option<String>,
}

impl SinkApprovalGate {
    /// Create a gate that prompts through `sink` and auto-denies after
    /// `timeout` without a decision.
    pub fn new(
        sink: Arc<dyn MessageSink>,
        timeout: std::time::Duration,
        channel_key: Option<String>,
    ) -> Self {
        Self {
            sink,
            timeout,
            channel_key,
        }
    }
}

#[async_trait]
impl crate::approval::ToolApprovalGate for SinkApprovalGate {
    async fn request_approval(
        &self,
        request: crate::approval::ApprovalRequest,
    ) -> crate::approval::ApprovalDecision {
        // Fast-path: Check pattern-based allow rules (session or persistent) first
        let rule_store = crate::approval::global_rule_store();
        if rule_store.is_allowed(
            self.channel_key.as_deref(),
            &request.tool_name,
            &request.arguments_preview,
        ) {
            return crate::approval::ApprovalDecision::Approved;
        }

        let (id, rx) = match self
            .sink
            .request_approval(&request.tool_name, &request.arguments_preview)
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                // Fail closed: a dangerous tool must never run just because
                // the prompt couldn't be delivered.
                return crate::approval::ApprovalDecision::Denied {
                    reason: format!("Approval prompt could not be delivered: {e}"),
                    outcome: crate::approval::ApprovalOutcome::PromptFailed,
                };
            }
        };

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(crate::approval::ApprovalChoice::AllowOnce))
            | Ok(Ok(crate::approval::ApprovalChoice::Session))
            | Ok(Ok(crate::approval::ApprovalChoice::AlwaysAllow)) => {
                crate::approval::ApprovalDecision::Approved
            }
            Ok(Ok(crate::approval::ApprovalChoice::Reject)) => {
                crate::approval::ApprovalDecision::Denied {
                    reason: "Tool execution denied by the user.".to_string(),
                    outcome: crate::approval::ApprovalOutcome::Denied,
                }
            }
            Ok(Err(_)) => crate::approval::ApprovalDecision::Denied {
                reason: "Approval channel closed unexpectedly.".to_string(),
                outcome: crate::approval::ApprovalOutcome::ChannelClosed,
            },
            Err(_) => {
                drop_pending_approval(id);
                crate::approval::ApprovalDecision::Denied {
                    reason: "Approval timed out; tool execution denied.".to_string(),
                    outcome: crate::approval::ApprovalOutcome::Timeout,
                }
            }
        }
    }
}

/// State of the currently active agent run in one channel.
///
/// Tracked by the gateway so an incoming message can interrupt the run
/// (set `cancel`) and wait for it to wind down (`done`) before the new
/// message is processed.
struct ActiveRun {
    /// Cooperative cancellation flag handed to the handler/agent.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Set to `true` when the run finishes (success, error, or cancelled).
    /// Watch channel so waiters never race the notification.
    done: tokio::sync::watch::Sender<bool>,
}

struct PendingApprovalEntry {
    sender: tokio::sync::oneshot::Sender<crate::approval::ApprovalChoice>,
    tool_name: String,
    arguments_preview: String,
}

/// Pending tool-approval requests: request ID → decision channel and request details.
///
/// Process-wide because the approval prompt is sent by the adapter while the
/// decision is awaited inside the agent's execute loop (different tasks).
static PENDING_APPROVALS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<u64, PendingApprovalEntry>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

static NEXT_APPROVAL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Register a fresh approval request and return `(id, decision_receiver)`.
/// The adapter resolves the receiver when the human presses a button; the
/// gate awaits the receiver (bounded by its own timeout).
pub fn register_pending_approval(
    tool_name: &str,
    arguments_preview: &str,
) -> (
    u64,
    tokio::sync::oneshot::Receiver<crate::approval::ApprovalChoice>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let id = NEXT_APPROVAL_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let entry = PendingApprovalEntry {
        sender: tx,
        tool_name: tool_name.to_string(),
        arguments_preview: arguments_preview.to_string(),
    };
    crate::lock_sync(&PENDING_APPROVALS).insert(id, entry);
    (id, rx)
}

/// Resolve a pending approval request. Returns `false` when the ID is
/// unknown (stale/duplicate button press).
pub(crate) fn resolve_pending_approval(
    id: u64,
    choice: crate::approval::ApprovalChoice,
    session_key: Option<&str>,
) -> bool {
    let entry = crate::lock_sync(&PENDING_APPROVALS).remove(&id);
    match entry {
        Some(entry) => {
            // Apply rule to store if Session or AlwaysAllow
            let rule_store = crate::approval::global_rule_store();
            let escaped_preview = regex::escape(&entry.arguments_preview);
            let pattern = if escaped_preview.is_empty() {
                ".*".to_string()
            } else {
                format!("^{}$", escaped_preview)
            };

            match choice {
                crate::approval::ApprovalChoice::Session => {
                    if let Some(key) = session_key {
                        rule_store.add_session_rule(key, &entry.tool_name, &pattern);
                    }
                }
                crate::approval::ApprovalChoice::AlwaysAllow => {
                    rule_store.add_persistent_rule(&entry.tool_name, &pattern);
                }
                _ => {}
            }

            entry.sender.send(choice).is_ok()
        }
        None => false,
    }
}

/// Drop a pending approval request (e.g. the gate timed out).
pub(crate) fn drop_pending_approval(id: u64) {
    crate::lock_sync(&PENDING_APPROVALS).remove(&id);
}

/// Gateway for routing messages between platforms and the agent
pub struct Gateway {
    config: GatewayConfig,
    adapters: HashMap<String, Arc<dyn PlatformAdapter>>,
    message_handler: Option<Arc<dyn MessageHandler>>,
    running: Arc<RwLock<bool>>,
    /// Active agent runs keyed by "platform:channel_id".
    active_runs: Arc<RwLock<HashMap<String, Arc<ActiveRun>>>>,
    /// Recurring job scheduler (`None` disables the cron ticker).
    scheduler: Option<Arc<crate::scheduler::Scheduler>>,
}

/// Handler for incoming messages from any platform
#[async_trait::async_trait]
pub trait MessageHandler: Send + Sync {
    /// Handle an incoming message.
    ///
    /// `sink` lets the handler emit progress updates (status heartbeats,
    /// tool notifications) into the originating channel mid-run, and deliver
    /// the final reply itself via [`MessageSink::send_final`] — reusing the
    /// live status message so the "⏳ Working…" placeholder is replaced by
    /// the actual response. `cancel` is the cooperative cancellation flag for
    /// this run — the gateway sets it when the user interrupts.
    ///
    /// Returning `Ok(())` means the reply (or an error notice) was already
    /// delivered to the channel; the gateway sends nothing further. Returning
    /// `Err` means nothing reached the user and the gateway may fall back to
    /// a generic error message.
    async fn handle(
        &self,
        message: IncomingMessage,
        sink: Arc<dyn MessageSink>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()>;
}

impl Gateway {
    /// Create a new gateway with the given configuration
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config,
            adapters: HashMap::new(),
            message_handler: None,
            running: Arc::new(RwLock::new(false)),
            active_runs: Arc::new(RwLock::new(HashMap::new())),
            scheduler: None,
        }
    }

    /// Register a platform adapter
    pub fn with_adapter(mut self, adapter: Arc<dyn PlatformAdapter>) -> Self {
        let name = adapter.name().to_string();
        info!(platform = %name, "Registering platform adapter");
        self.adapters.insert(name, adapter);
        self
    }

    /// Set the message handler
    pub fn with_handler(mut self, handler: Arc<dyn MessageHandler>) -> Self {
        self.message_handler = Some(handler);
        self
    }

    /// Attach the cron scheduler; `run()` starts its ticker task.
    pub fn with_scheduler(mut self, scheduler: Arc<crate::scheduler::Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    /// Start the gateway and all enabled adapters
    pub async fn start(&self) -> Result<()> {
        *self.running.write().await = true;

        for (name, adapter) in &self.adapters {
            if adapter.is_enabled() {
                info!(platform = %name, "Starting platform adapter");
                if let Err(e) = adapter.start().await {
                    error!(platform = %name, error = %e, "Failed to start adapter");
                }
            }
        }

        Ok(())
    }

    /// Stop the gateway and all adapters
    pub async fn stop(&self) -> Result<()> {
        *self.running.write().await = false;

        for (name, adapter) in &self.adapters {
            info!(platform = %name, "Stopping platform adapter");
            if let Err(e) = adapter.stop().await {
                error!(platform = %name, error = %e, "Failed to stop adapter");
            }
        }

        Ok(())
    }

    /// Check if the gateway is running
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Get the status of all adapters
    pub async fn status(&self) -> HashMap<String, bool> {
        let mut status = HashMap::new();
        for (name, adapter) in &self.adapters {
            status.insert(name.clone(), adapter.is_enabled());
        }
        status
    }

    /// Send a message to a specific platform
    pub async fn send_to_platform(&self, platform: &str, message: OutgoingMessage) -> Result<()> {
        let adapter = match self.adapters.get(platform) {
            Some(a) => a,
            None => {
                return Err(crate::error::Error::Agent(format!(
                    "Unknown platform: {}",
                    platform
                )));
            }
        };

        adapter.send_message(message).await
    }

    /// Run the gateway polling loop until stopped.
    ///
    /// Starts all enabled adapters, then continuously polls each polling-capable
    /// adapter for updates. Each incoming message is dispatched to the handler
    /// on its own task so the polling loop never blocks on a long agent run —
    /// this is what makes mid-run interrupts possible. Blocks until `stop()`
    /// is called (e.g. from a signal handler).
    pub async fn run(&self) -> Result<()> {
        let enabled: Vec<(String, Arc<dyn PlatformAdapter>)> = self
            .adapters
            .iter()
            .filter(|(_, a)| a.is_enabled())
            .map(|(name, a)| (name.clone(), a.clone()))
            .collect();

        if enabled.is_empty() && !self.config.webhooks_enabled && !self.config.webui_enabled {
            return Err(crate::error::Error::Agent(
                "No enabled platform adapters, webhook listener, or WebUI server to run"
                    .to_string(),
            ));
        }

        let webui_listener = if self.config.webui_enabled {
            let addr_str = &self.config.webui_addr;
            let socket_addr: std::net::SocketAddr = addr_str.parse().map_err(|e| {
                crate::error::Error::Config(format!("Invalid webui_addr '{addr_str}': {e}"))
            })?;
            if !socket_addr.ip().is_loopback() && self.config.webui_password.trim().is_empty() {
                return Err(crate::error::Error::Config(
                    "WebUI remote non-loopback binding requires a non-empty password".to_string(),
                ));
            }
            let listener = tokio::net::TcpListener::bind(socket_addr)
                .await
                .map_err(|e| {
                    crate::error::Error::Agent(format!(
                        "Failed to bind WebUI listener at {socket_addr}: {e}"
                    ))
                })?;
            Some((listener, socket_addr))
        } else {
            None
        };

        let webhook_listener = if self.config.webhooks_enabled {
            let address = self.config.webhooks_addr.as_deref().ok_or_else(|| {
                crate::error::Error::MissingConfig {
                    key: "webhooks_addr".to_string(),
                }
            })?;
            Some(
                tokio::net::TcpListener::bind(address)
                    .await
                    .map_err(|error| {
                        crate::error::Error::Agent(format!(
                            "Failed to bind webhook listener at {address}: {error}"
                        ))
                    })?,
            )
        } else {
            None
        };

        self.start().await?;

        info!(
            platforms = ?enabled.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "Gateway polling loop started"
        );

        // One polling task per adapter. Adapters have very different poll
        // latencies (Telegram long-polls ~30s server-side; the WhatsApp
        // bridge drains instantly), so a sequential loop would let the
        // slowest adapter starve the rest. Each task owns its own loop and
        // dispatches into the shared gateway.
        let mut handles = Vec::new();

        if let Some((listener, address)) = webui_listener {
            let webui_state = crate::webui::WebUiState::new(
                Arc::new(tokio::sync::RwLock::new(runtime_config())),
                self.config.webui_password.clone(),
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            );
            let running = self.running.clone();
            handles.push(tokio::spawn(async move {
                info!(%address, "WebUI server listening at http://{address}");
                let shutdown = async move {
                    while *running.read().await {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                };
                if let Err(e) =
                    crate::webui::serve_webui_listener(listener, webui_state, shutdown).await
                {
                    error!("WebUI server stopped with error: {e}");
                }
            }));
        }

        if let Some(listener) = webhook_listener {
            let address = listener.local_addr().map_err(|error| {
                crate::error::Error::Agent(format!(
                    "Failed to inspect webhook listener address: {error}"
                ))
            })?;
            let (incoming_tx, mut incoming_rx) = tokio::sync::mpsc::channel(100);
            let state = WebhookServerState {
                incoming_tx: Some(incoming_tx),
                slack_signing_secret: self.config.slack_signing_secret.clone(),
            };
            let running = self.running.clone();
            handles.push(tokio::spawn(async move {
                info!(%address, "Webhook listener started");
                let shutdown = async move {
                    while *running.read().await {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                };
                if let Err(error) = serve_webhook_listener(listener, state, shutdown).await {
                    error!(%error, "Webhook listener stopped with an error");
                }
            }));

            let adapters = self.adapters.clone();
            let webhook_adapter: Arc<dyn PlatformAdapter> = Arc::new(WebhookAdapter);
            let message_handler = self.message_handler.clone();
            let active_runs = self.active_runs.clone();
            let admins = self.config.admins.clone();
            handles.push(tokio::spawn(async move {
                while let Some(incoming) = incoming_rx.recv().await {
                    let platform = incoming.platform.clone();
                    let adapter = if platform == "webhook" {
                        webhook_adapter.clone()
                    } else if let Some(adapter) = adapters.get(&platform) {
                        adapter.clone()
                    } else {
                        warn!(%platform, "Webhook target platform is not registered");
                        continue;
                    };
                    dispatch_message(
                        &adapter,
                        &platform,
                        incoming,
                        &admins,
                        message_handler.clone(),
                        active_runs.clone(),
                    )
                    .await;
                }
            }));
        }
        for (platform, adapter) in enabled {
            let running = self.running.clone();
            let message_handler = self.message_handler.clone();
            let active_runs = self.active_runs.clone();
            let admins = self.config.admins.clone();

            handles.push(tokio::spawn(async move {
                while *running.read().await {
                    let updates = match adapter.poll_updates().await {
                        Ok(u) => u,
                        Err(e) => {
                            warn!(platform = %platform, error = %e, "Poll failed; backing off");
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            continue;
                        }
                    };

                    for update in updates {
                        // Interactive callbacks (Telegram inline-keyboard
                        // button presses, Discord interaction components) resolve
                        // pending approval requests; they never become agent messages.
                        let is_callback = update.get("callback_query").is_some()
                            || update.get("type").and_then(|t| t.as_i64()) == Some(3)
                            || update.get("d").and_then(|d| d.get("type")).and_then(|t| t.as_i64()) == Some(3)
                            || update.get("t").and_then(|t| t.as_str()) == Some("INTERACTION_CREATE");

                        if is_callback {
                            if let Err(e) = adapter.handle_callback_query(update).await {
                                warn!(platform = %platform, error = %e, "Failed to handle callback query");
                            }
                            continue;
                        }

                        let incoming = match adapter.handle_update(update).await {
                            Ok(Some(msg)) => msg,
                            Ok(None) => continue,
                            Err(e) => {
                                warn!(platform = %platform, error = %e, "Failed to parse update");
                                continue;
                            }
                        };

                        // Non-blocking dispatch: spawn the handler so the poll
                        // loop keeps draining updates (including interrupts).
                        dispatch_message(
                            &adapter,
                            &platform,
                            incoming,
                            &admins,
                            message_handler.clone(),
                            active_runs.clone(),
                        )
                        .await;
                    }

                    // Small yield so an empty poll batch doesn't busy-spin.
                    // Long-polling adapters already block server-side when idle.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }));
        }

        // Cron ticker: every 30s, fire due jobs by injecting their prompt
        // as a synthetic incoming message into the owning channel.
        if let Some(scheduler) = self.scheduler.clone() {
            let running = self.running.clone();
            let message_handler = self.message_handler.clone();
            let active_runs = self.active_runs.clone();
            let admins = self.config.admins.clone();
            let adapters: HashMap<String, Arc<dyn PlatformAdapter>> = self.adapters.clone();

            handles.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                while *running.read().await {
                    tick.tick().await;
                    let now = crate::scheduler::now_secs();
                    for job in scheduler.due_jobs(now).await {
                        // Mark fired BEFORE dispatch so a crash between the
                        // two can't double-fire on the next tick.
                        if let Err(e) = scheduler.mark_fired(job.id).await {
                            warn!(job = job.id, error = %e, "Failed to mark cron job fired");
                            continue;
                        }
                        let adapter = match adapters.get(&job.platform) {
                            Some(a) => a.clone(),
                            None => {
                                warn!(job = job.id, platform = %job.platform, "Cron job platform not registered");
                                continue;
                            }
                        };
                        info!(job = job.id, channel = %job.channel_id, "Firing cron job");
                        let incoming = IncomingMessage::new(
                            &job.platform,
                            "cron",
                            "cron",
                            &job.channel_id,
                            &job.prompt,
                        );
                        dispatch_message(
                            &adapter,
                            &job.platform,
                            incoming,
                            &admins,
                            message_handler.clone(),
                            active_runs.clone(),
                        )
                        .await;
                    }
                }
            }));
        }

        // Wait for all polling tasks to finish (they exit when stop() flips
        // the running flag).
        for handle in handles {
            let _ = handle.await;
        }

        self.stop().await?;
        Ok(())
    }
}

/// Dispatch one incoming message to the handler on a background task.
///
/// Interrupt semantics (per channel):
/// - A message arriving while a run is active cancels that run. The
///   gateway notifies the user, waits (bounded) for the run to wind
///   down, then processes the new message.
/// - `/stop` cancels the active run without processing further.
/// - `/stop` with no active run is a no-op notification.
async fn dispatch_message(
    adapter: &Arc<dyn PlatformAdapter>,
    platform: &str,
    incoming: IncomingMessage,
    admins: &[String],
    message_handler: Option<Arc<dyn MessageHandler>>,
    active_runs: Arc<RwLock<HashMap<String, Arc<ActiveRun>>>>,
) {
    // Check if user is admin (synthetic cron messages bypass this: they
    // were created by an already-authorized user in this channel).
    let is_cron = incoming.user_id == "cron";
    if !is_cron && !admins.is_empty() && !admins.contains(&incoming.user_id) {
        debug!(user = %incoming.user_id, "User not authorized");
        let _ = adapter
            .send_message(OutgoingMessage::new(
                &incoming.channel_id,
                "You are not authorized to use this bot.",
            ))
            .await;
        return;
    }

    let handler = match &message_handler {
        Some(h) => h.clone(),
        None => {
            warn!("No message handler configured");
            return;
        }
    };

    let run_key = format!("{}:{}", platform, incoming.channel_id);
    let content = incoming.content.trim().to_string();
    let is_stop = content.eq_ignore_ascii_case("/stop");

    // Is there an active run in this channel?
    let active = {
        let runs = active_runs.read().await;
        runs.get(&run_key).cloned()
    };

    if let Some(active) = active {
        // Interrupt: cancel the running generation.
        active
            .cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);

        if is_stop {
            let _ = adapter
                .send_message(OutgoingMessage::new(
                    &incoming.channel_id,
                    "🛑 Stopping current task...",
                ))
                .await;
            return;
        }

        let _ = adapter
            .send_message(OutgoingMessage::new(
                &incoming.channel_id,
                "⚡ Interrupting current task. I'll respond to your message shortly.",
            ))
            .await;

        // Wait (bounded) for the old run to wind down so the agent's
        // conversation state is repaired before the new turn starts.
        let mut done_rx = active.done.subscribe();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !*done_rx.borrow_and_update() {
                if done_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    } else if is_stop {
        let _ = adapter
            .send_message(OutgoingMessage::new(
                &incoming.channel_id,
                "Nothing is running right now.",
            ))
            .await;
        return;
    }

    // Register the new run.
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (done_tx, _) = tokio::sync::watch::channel(false);
    let run = Arc::new(ActiveRun {
        cancel: cancel.clone(),
        done: done_tx,
    });
    active_runs
        .write()
        .await
        .insert(run_key.clone(), run.clone());

    let sink: Arc<dyn MessageSink> = Arc::new(ChannelSink::new(
        adapter.clone(),
        incoming.channel_id.clone(),
    ));
    let active_runs = active_runs.clone();
    let channel_id = incoming.channel_id.clone();
    let platform = platform.to_string();
    let adapter = adapter.clone();

    tokio::spawn(async move {
        let result = handler.handle(incoming, sink, cancel).await;

        // Mark the run done, then deregister it — but only if the entry
        // still belongs to THIS run. An interrupt that timed out waiting
        // for us to wind down may have already registered its replacement
        // under the same key; a blind remove would evict the new run and
        // make it invisible to /stop and future interrupts.
        let _ = run.done.send(true);
        {
            let mut runs = active_runs.write().await;
            if runs.get(&run_key).is_some_and(|r| Arc::ptr_eq(r, &run)) {
                runs.remove(&run_key);
            }
        }

        match result {
            Ok(()) => {
                // The handler delivered the reply itself (via
                // send_final, reusing the status message). Nothing
                // left for the gateway to do.
            }
            Err(crate::error::Error::Cancelled) => {
                // Cancelled runs are expected; the handler already
                // notified the user. No error fallback.
                debug!(platform = %platform, channel = %channel_id, "Run cancelled");
            }
            Err(e) => {
                error!(platform = %platform, error = %e, "Handler failed");
                let fallback = OutgoingMessage::new(
                    &channel_id,
                    "Sorry, something went wrong processing your message.",
                )
                .no_markdown();
                let _ = adapter.send_message(fallback).await;
            }
        }
    });
}

/// Telegram adapter
pub struct TelegramAdapter {
    token: Option<String>,
    enabled: bool,
    /// Long-polling offset: next update_id to fetch (last seen + 1)
    offset: AtomicI64,
    /// Shared HTTP client (connection pooling across polls/sends)
    client: reqwest::Client,
    /// Voice-note transcription config (`None` disables STT).
    stt: Option<SttConfig>,
}

/// OpenAI-compatible speech-to-text endpoint used to transcribe incoming
/// voice notes before they reach the agent.
#[derive(Debug, Clone)]
pub struct SttConfig {
    /// OpenAI-compatible API base, e.g. `https://…/v1`.
    pub base_url: String,
    /// Bearer token for the API.
    pub api_key: Option<String>,
    /// Model slug, e.g. `gemini/gemini-2.5-pro`.
    pub model: String,
}

impl TelegramAdapter {
    /// Create a new Telegram adapter
    pub fn new(token: Option<String>) -> Self {
        let enabled = token.is_some();
        Self {
            token,
            enabled,
            offset: AtomicI64::new(0),
            // Hard timeouts so a half-open TCP connection can never hang a
            // polling task forever. read_timeout is per-read, not total: the
            // 30s server-side long-poll stays well under the 45s read cap,
            // and a stalled connection dies instead of blocking silently.
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .read_timeout(std::time::Duration::from_secs(45))
                .build()
                .expect("valid reqwest client config"),
            stt: None,
        }
    }

    /// Attach a speech-to-text config so incoming voice notes are
    /// transcribed into text before reaching the agent.
    pub fn with_stt(mut self, stt: SttConfig) -> Self {
        self.stt = Some(stt);
        self
    }

    /// Transcribe a Telegram voice/audio message into text.
    ///
    /// Flow: `getFile` → download the `.oga` from `file.telegram.org` →
    /// POST multipart to the OpenAI-compatible `/audio/transcriptions`
    /// endpoint. Returns `None` (with a warning) on any failure so a
    /// broken STT path never drops the user's message silently — the
    /// caller surfaces a visible error message instead.
    async fn transcribe_voice(&self, file_id: &str) -> Option<String> {
        let stt = self.stt.as_ref()?;

        // 1. Resolve the file path on Telegram's servers.
        let url = format!("{}/getFile?file_id={}", self.api_url(), file_id);
        let response = self.client.get(&url).send().await.ok()?;
        let body: serde_json::Value = response.json().await.ok()?;
        let file_path = body
            .get("result")
            .and_then(|r| r.get("file_path"))
            .and_then(|p| p.as_str())?
            .to_string();

        // 2. Download the audio bytes.
        let token = self.token.as_deref().unwrap_or("");
        let download_url = format!("https://api.telegram.org/file/bot{}/{}", token, file_path);
        let audio = self
            .client
            .get(&download_url)
            .send()
            .await
            .ok()?
            .bytes()
            .await
            .ok()?;
        if audio.is_empty() {
            warn!("Downloaded voice file is empty");
            return None;
        }

        // 3. POST multipart to the transcription endpoint.
        let part = reqwest::multipart::Part::bytes(audio.to_vec())
            .file_name("voice.oga")
            .mime_str("audio/ogg")
            .ok()?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", stt.model.clone());

        let endpoint = format!(
            "{}/audio/transcriptions",
            stt.base_url.trim_end_matches('/')
        );
        let mut request = self.client.post(&endpoint).multipart(form);
        if let Some(key) = &stt.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.ok()?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.ok()?;
        if !status.is_success() {
            warn!(status = %status, body = %body, "Transcription request failed");
            return None;
        }
        let text = body
            .get("text")
            .and_then(|t| t.as_str())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())?;
        info!(chars = text.len(), "Voice note transcribed");
        Some(text)
    }

    fn api_url(&self) -> String {
        let base = runtime_config().gateway.telegram_api_base;
        format!(
            "{}/bot{}",
            base.trim_end_matches('/'),
            self.token.as_ref().unwrap_or(&String::new())
        )
    }

    /// Acknowledge a callback query so Telegram stops the button spinner.
    async fn answer_callback(&self, callback_query_id: &str, text: &str) {
        let body = serde_json::json!({
            "callback_query_id": callback_query_id,
            "text": text,
        });
        let _ = self
            .client
            .post(format!("{}/answerCallbackQuery", self.api_url()))
            .json(&body)
            .send()
            .await;
    }

    /// Send one already-built sendMessage body, validating Telegram's `ok`
    /// flag so API-level rejections surface as errors. Returns the platform
    /// message ID when Telegram provides one (used for in-place edits).
    async fn send_chunk(&self, body: &serde_json::Value) -> Result<Option<String>> {
        let response = self
            .client
            .post(format!("{}/sendMessage", self.api_url()))
            .json(body)
            .send()
            .await?;

        let status = response.status();
        let payload: serde_json::Value = response.json().await.unwrap_or_default();
        if !payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let description = payload
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown Telegram API error");
            return Err(crate::error::Error::ParseResponse(format!(
                "Telegram sendMessage failed (HTTP {status}): {description}"
            )));
        }

        let message_id = payload
            .get("result")
            .and_then(|r| r.get("message_id"))
            .and_then(|id| id.as_i64())
            .map(|id| id.to_string());

        Ok(message_id)
    }

    /// Split an outgoing message into Telegram-ready JSON bodies: chunked at
    /// line boundaries under the 4096-unit cap, code fences re-balanced per
    /// chunk, and (when markdown is enabled) converted to MarkdownV2. Returns
    /// one `sendMessage`-shaped body per chunk. `reply_to` is NOT attached —
    /// callers set it on the first chunk as needed.
    fn prepare_chunks(&self, message: &OutgoingMessage) -> Vec<serde_json::Value> {
        // 3500 leaves headroom for MarkdownV2 escape backslashes.
        let chunks = split_message(&message.content, 3500);
        let mut bodies = Vec::with_capacity(chunks.len());
        let mut in_code_block = false;
        for chunk in chunks.iter() {
            let mut text = chunk.clone();

            // Re-open a code fence that was cut by the split.
            if in_code_block {
                text = format!("```\n{text}");
            }

            // Toggle state per fence line in the ORIGINAL chunk (counting
            // after the prepend above would skew the parity).
            let fence_count = chunk
                .lines()
                .filter(|l| l.trim_start().starts_with("```"))
                .count();
            if fence_count % 2 == 1 {
                in_code_block = !in_code_block;
            }

            // If this chunk ends inside a fence, close it so Telegram
            // doesn't swallow the rest into a code entity.
            if in_code_block {
                text.push_str("\n```");
            }

            let mut body = serde_json::json!({
                "chat_id": message.channel_id,
                "text": text,
            });

            if message.parse_markdown {
                // Agent output is standard markdown; Telegram needs MarkdownV2
                // with strict escaping. Convert per chunk so entities never
                // straddle a split boundary.
                body["text"] = serde_json::json!(markdown_to_markdownv2(&text));
                body["parse_mode"] = serde_json::json!("MarkdownV2");
            }

            bodies.push(body);
        }
        bodies
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    fn name(&self) -> &str {
        "telegram"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    async fn start(&self) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        // Verify the token by getting bot info
        let response = self
            .client
            .get(format!("{}/getMe", self.api_url()))
            .send()
            .await?;

        if response.status().is_success() {
            info!("Telegram bot started successfully");
            Ok(())
        } else {
            Err(crate::error::Error::Agent(
                "Failed to verify Telegram bot token".to_string(),
            ))
        }
    }

    async fn stop(&self) -> Result<()> {
        info!("Telegram adapter stopped");
        Ok(())
    }

    async fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        // Telegram caps a single message at 4096 UTF-16 units. Split long
        // replies into chunks at line boundaries and send them in order.
        let mut bodies = self.prepare_chunks(&message);

        // Only the first chunk replies to the original message.
        if let Some(first) = bodies.first_mut() {
            if let Some(ref reply_to) = message.reply_to {
                first["reply_to_message_id"] = serde_json::json!(reply_to);
            }
        }

        for body in &bodies {
            self.send_chunk(body).await?;
        }

        Ok(())
    }

    async fn send_message_tracked(&self, message: OutgoingMessage) -> Result<Option<String>> {
        // Same chunking as send_message, but capture the first chunk's ID.
        let mut bodies = self.prepare_chunks(&message);

        if let Some(first) = bodies.first_mut() {
            if let Some(ref reply_to) = message.reply_to {
                first["reply_to_message_id"] = serde_json::json!(reply_to);
            }
        }

        let mut first_id: Option<String> = None;
        for (idx, body) in bodies.iter().enumerate() {
            let id = self.send_chunk(body).await?;
            if idx == 0 {
                first_id = id;
            }
        }

        Ok(first_id)
    }

    async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        message: OutgoingMessage,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "chat_id": channel_id,
            "message_id": message_id,
            "text": message.content,
        });

        if message.parse_markdown {
            body["text"] = serde_json::json!(markdown_to_markdownv2(&message.content));
            body["parse_mode"] = serde_json::json!("MarkdownV2");
        }

        let response = self
            .client
            .post(format!("{}/editMessageText", self.api_url()))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let payload: serde_json::Value = response.json().await.unwrap_or_default();
        if !payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let description = payload
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown Telegram API error");
            return Err(crate::error::Error::ParseResponse(format!(
                "Telegram editMessageText failed (HTTP {status}): {description}"
            )));
        }

        Ok(())
    }

    async fn send_final(
        &self,
        channel_id: &str,
        message_id: Option<&str>,
        message: OutgoingMessage,
    ) -> Result<()> {
        // No status message to reuse → plain chunked send.
        let Some(status_id) = message_id else {
            return self.send_message(message).await;
        };

        let bodies = self.prepare_chunks(&message);
        for (idx, body) in bodies.iter().enumerate() {
            if idx == 0 {
                // Replace the "⏳ Working…" status message with the reply.
                let mut edit_body = body.clone();
                edit_body["chat_id"] = serde_json::json!(channel_id);
                edit_body["message_id"] = serde_json::json!(status_id);
                // reply_to is meaningless on an edit; drop it if present.
                if let Some(obj) = edit_body.as_object_mut() {
                    obj.remove("reply_to_message_id");
                }

                let response = self
                    .client
                    .post(format!("{}/editMessageText", self.api_url()))
                    .json(&edit_body)
                    .send()
                    .await?;

                let payload: serde_json::Value = response.json().await.unwrap_or_default();
                if !payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let description = payload
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown Telegram API error");
                    // "message is not modified" is benign (identical text);
                    // anything else falls back to a fresh send so the reply
                    // is never lost.
                    if !description.contains("message is not modified") {
                        warn!(
                            error = %description,
                            "Edit of status message failed; sending reply as new message"
                        );
                        self.send_chunk(body).await?;
                    }
                }
            } else {
                // Overflow chunks go out as new messages.
                self.send_chunk(body).await?;
            }
        }

        Ok(())
    }

    async fn handle_update(&self, update: serde_json::Value) -> Result<Option<IncomingMessage>> {
        // Parse Telegram update
        let message = match update.get("message") {
            Some(m) => m,
            None => return Ok(None),
        };
        let chat = match message.get("chat") {
            Some(c) => c,
            None => return Ok(None),
        };

        let from = message.get("from");

        let mut content = message
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        // Voice/audio notes: transcribe via STT when configured. Without
        // STT the note is ignored (returning Ok(None)); a failed
        // transcription surfaces a visible error so the user knows their
        // message didn't land.
        if content.is_empty() {
            let voice_file_id = message
                .get("voice")
                .or_else(|| message.get("audio"))
                .and_then(|v| v.get("file_id"))
                .and_then(|f| f.as_str());
            if let Some(file_id) = voice_file_id {
                if self.stt.is_some() {
                    match self.transcribe_voice(file_id).await {
                        Some(text) => content = format!("🎤 [voice note] {}", text),
                        None => {
                            content =
                                "⚠️ Voice note received but transcription failed.".to_string();
                        }
                    }
                } else {
                    return Ok(None);
                }
            }
        }

        if content.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            IncomingMessage::new(
                "telegram",
                from.and_then(|f| f.get("id"))
                    .and_then(|id| id.as_i64())
                    .map(|i| i.to_string())
                    .unwrap_or_default(),
                from.and_then(|f| f.get("username"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("unknown"),
                chat.get("id")
                    .and_then(|id| id.as_i64())
                    .map(|i| i.to_string())
                    .unwrap_or_default(),
                content,
            )
            .with_raw(update),
        ))
    }

    async fn handle_callback_query(&self, update: serde_json::Value) -> Result<()> {
        let query = match update.get("callback_query") {
            Some(q) => q,
            None => return Ok(()),
        };
        let query_id = query
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let data = query
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let channel_id = query
            .get("message")
            .and_then(|m| m.get("chat"))
            .and_then(|c| c.get("id"))
            .and_then(|id| id.as_i64())
            .map(|i| format!("telegram:{}", i));

        // Button payloads: "approve:<id>", "session:<id>", "always:<id>", "reject:<id>" (and legacy "deny:<id>").
        let (choice, id_str) = match data.split_once(':') {
            Some(("approve", id)) | Some(("once", id)) => {
                (crate::approval::ApprovalChoice::AllowOnce, id)
            }
            Some(("session", id)) => (crate::approval::ApprovalChoice::Session, id),
            Some(("always", id)) => (crate::approval::ApprovalChoice::AlwaysAllow, id),
            Some(("reject", id)) | Some(("deny", id)) => {
                (crate::approval::ApprovalChoice::Reject, id)
            }
            _ => {
                self.answer_callback(&query_id, "Unknown action").await;
                return Ok(());
            }
        };
        let id: u64 = match id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                self.answer_callback(&query_id, "Malformed request id")
                    .await;
                return Ok(());
            }
        };

        let resolved = resolve_pending_approval(id, choice, channel_id.as_deref());
        let feedback = if !resolved {
            "This request already expired."
        } else {
            match choice {
                crate::approval::ApprovalChoice::AllowOnce => "✅ Allowed once",
                crate::approval::ApprovalChoice::Session => "⏳ Allowed for this session",
                crate::approval::ApprovalChoice::AlwaysAllow => "🔒 Always allowed (persisted)",
                crate::approval::ApprovalChoice::Reject => "❌ Rejected",
            }
        };
        self.answer_callback(&query_id, feedback).await;
        Ok(())
    }

    async fn send_approval_prompt(
        &self,
        channel_id: &str,
        tool_name: &str,
        arguments_preview: &str,
    ) -> Result<(
        u64,
        tokio::sync::oneshot::Receiver<crate::approval::ApprovalChoice>,
    )> {
        let (id, rx) = register_pending_approval(tool_name, arguments_preview);

        let preview: String = arguments_preview.chars().take(200).collect();
        let text = format!(
            "🔐 Tool approval required\n\nTool: {}\nArgs: {}\n\nChoose permission:",
            tool_name,
            if preview.is_empty() {
                "(none)"
            } else {
                &preview
            }
        );

        let body = serde_json::json!({
            "chat_id": channel_id,
            "text": text,
            "reply_markup": {
                "inline_keyboard": [
                    [
                        { "text": "✅ Allow Once", "callback_data": format!("approve:{}", id) },
                        { "text": "⏳ Session", "callback_data": format!("session:{}", id) }
                    ],
                    [
                        { "text": "🔒 Always Allow", "callback_data": format!("always:{}", id) },
                        { "text": "❌ Reject", "callback_data": format!("reject:{}", id) }
                    ]
                ]
            }
        });

        match self.send_chunk(&body).await {
            Ok(_) => Ok((id, rx)),
            Err(e) => {
                // Prompt never reached the user: don't leave a dangling
                // pending entry, and fail closed (deny) rather than running
                // a dangerous tool nobody saw.
                drop_pending_approval(id);
                Err(e)
            }
        }
    }

    async fn poll_updates(&self) -> Result<Vec<serde_json::Value>> {
        if !self.is_enabled() {
            return Ok(Vec::new());
        }

        let offset = self.offset.load(Ordering::SeqCst);
        let url = format!(
            "{}/getUpdates?offset={}&timeout=30&allowed_updates=%5B%22message%22%2C%22callback_query%22%5D",
            self.api_url(),
            offset
        );

        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(crate::error::Error::Agent(format!(
                "getUpdates failed with status {}",
                response.status()
            )));
        }

        let body: serde_json::Value = response.json().await?;
        let ok = body.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
        if !ok {
            return Err(crate::error::Error::Agent(
                "getUpdates returned ok=false".to_string(),
            ));
        }

        let updates = body
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        // Advance offset past the highest update_id we've seen so the next
        // poll only returns new updates.
        if let Some(max_id) = updates
            .iter()
            .filter_map(|u| u.get("update_id").and_then(|id| id.as_i64()))
            .max()
        {
            self.offset.store(max_id + 1, Ordering::SeqCst);
        }

        Ok(updates)
    }

    fn config_json(&self) -> serde_json::Value {
        serde_json::json!({
            "platform": "telegram",
            "enabled": self.enabled,
            "has_token": self.token.is_some()
        })
    }
}

/// WhatsApp adapter (via the Baileys HTTP bridge)
///
/// Talks to the standalone Node.js bridge (`scripts/whatsapp-bridge/bridge.js`)
/// which owns the actual WhatsApp connection. The bridge exposes:
///   GET  /messages  - drain queued incoming events (JSON array)
///   POST /send      - send text { chatId, message, replyTo? }
///   POST /edit      - edit a sent message { chatId, messageId, message }
///   GET  /health    - connection status
///
/// Unlike Telegram, the bridge does its own long-message chunking and (in
/// self-chat mode) reply-prefixing, so the adapter only converts standard
/// markdown to WhatsApp's `*bold* _italic_ ~strike~` syntax before sending.
pub struct WhatsAppAdapter {
    /// Base URL of the bridge, e.g. "http://127.0.0.1:3000"
    bridge_url: Option<String>,
    enabled: bool,
    client: reqwest::Client,
}

impl WhatsAppAdapter {
    /// Create a new WhatsApp adapter pointing at the given bridge URL.
    /// The HTTP client carries hard timeouts: a bridge that accepts a
    /// connection but never answers must not hang the gateway forever
    /// (the poll loop and the progress pump both call into this client).
    pub fn new(bridge_url: Option<String>) -> Self {
        let enabled = bridge_url.is_some();
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            bridge_url,
            enabled,
            client,
        }
    }

    fn base(&self) -> String {
        self.bridge_url
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string()
    }

    /// POST a JSON body to a bridge endpoint, surfacing bridge-level errors.
    async fn post_bridge(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(format!("{}{}", self.base(), path))
            .json(body)
            .send()
            .await?;

        let status = response.status();
        let payload: serde_json::Value = response.json().await.unwrap_or_default();

        if !status.is_success() {
            let err = payload
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown bridge error");
            return Err(crate::error::Error::ParseResponse(format!(
                "WhatsApp bridge {path} failed (HTTP {status}): {err}"
            )));
        }
        Ok(payload)
    }
}

/// Convert standard markdown to WhatsApp-compatible formatting.
///
/// WhatsApp supports `*bold*`, `_italic_`, `~strikethrough~`, ``` ```code``` ```,
/// and monospaced `` `inline` ``. Standard markdown uses different delimiters,
/// so we convert. Fenced code blocks and inline code are protected via
/// placeholder substitution so their contents are never reformatted.
///
/// This is a port of `whatsapp_common.py::format_message`, implemented without
/// regex lookarounds (the `regex` crate doesn't support them) by protecting
/// bold results from the later italic pass.
fn markdown_to_whatsapp(content: &str) -> String {
    if content.is_empty() {
        return content.to_string();
    }

    let mut fences: Vec<String> = Vec::new();
    let mut codes: Vec<String> = Vec::new();
    let mut bolds: Vec<String> = Vec::new();

    // 1. Protect fenced code blocks.
    let fence_re = regex::Regex::new(r"```[\s\S]*?```").expect("static regex");
    let mut result = fence_re
        .replace_all(content, |caps: &regex::Captures| {
            fences.push(caps[0].to_string());
            format!("\u{0001}F{}\u{0001}", fences.len() - 1)
        })
        .into_owned();

    // 2. Protect inline code.
    let code_re = regex::Regex::new(r"`[^`\n]+`").expect("static regex");
    result = code_re
        .replace_all(&result, |caps: &regex::Captures| {
            codes.push(caps[0].to_string());
            format!("\u{0001}C{}\u{0001}", codes.len() - 1)
        })
        .into_owned();

    // 3. Bold: **text** or __text__ → *text*, protected so the italic pass
    //    below doesn't re-process the resulting single-asterisk form.
    let bold_star = regex::Regex::new(r"\*\*([\s\S]+?)\*\*").expect("static regex");
    result = bold_star
        .replace_all(&result, |caps: &regex::Captures| {
            bolds.push(format!("*{}*", &caps[1]));
            format!("\u{0001}B{}\u{0001}", bolds.len() - 1)
        })
        .into_owned();
    let bold_under = regex::Regex::new(r"__([\s\S]+?)__").expect("static regex");
    result = bold_under
        .replace_all(&result, |caps: &regex::Captures| {
            bolds.push(format!("*{}*", &caps[1]));
            format!("\u{0001}B{}\u{0001}", bolds.len() - 1)
        })
        .into_owned();

    // 4. Italic: *text* → _text_. Safe now: all ** pairs were consumed above,
    //    so any remaining single-asterisk pair is a genuine italic. Require a
    //    non-space, non-asterisk first char to avoid list bullets ("* item").
    let italic_re = regex::Regex::new(r"\*([^\s*][^*\n]*?)\*").expect("static regex");
    result = italic_re
        .replace_all(&result, |caps: &regex::Captures| format!("_{}_", &caps[1]))
        .into_owned();

    // 5. Strikethrough: ~~text~~ → ~text~.
    let strike_re = regex::Regex::new(r"~~([\s\S]+?)~~").expect("static regex");
    result = strike_re
        .replace_all(&result, |caps: &regex::Captures| format!("~{}~", &caps[1]))
        .into_owned();

    // 6. Headers: "# Title" → "*Title*". Strip any *...* wrapping the inner
    //    text already carries so we don't emit "**Title**". If the header
    //    content is exactly one protected bold placeholder, leave it alone —
    //    restoration already yields "*Title*".
    let bold_ph_re = regex::Regex::new(r"^\u{0001}B\d+\u{0001}$").expect("static regex");
    let header_re = regex::Regex::new(r"(?m)^#{1,6}\s+(.+)$").expect("static regex");
    result = header_re
        .replace_all(&result, |caps: &regex::Captures| {
            let inner = caps[1].trim();
            if bold_ph_re.is_match(inner) {
                return inner.to_string();
            }
            let mut inner = inner.to_string();
            while inner.len() > 1 && inner.starts_with('*') && inner.ends_with('*') {
                inner = inner[1..inner.len() - 1].trim().to_string();
            }
            format!("*{}*", inner)
        })
        .into_owned();

    // 7. Links: [text](url) → text (url).
    let link_re = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").expect("static regex");
    result = link_re
        .replace_all(&result, |caps: &regex::Captures| {
            format!("{} ({})", &caps[1], &caps[2])
        })
        .into_owned();

    // 8. Restore protected sections (bold, then inline code, then fences).
    for (i, bold) in bolds.iter().enumerate() {
        result = result.replace(&format!("\u{0001}B{}\u{0001}", i), bold);
    }
    for (i, code) in codes.iter().enumerate() {
        result = result.replace(&format!("\u{0001}C{}\u{0001}", i), code);
    }
    for (i, fence) in fences.iter().enumerate() {
        result = result.replace(&format!("\u{0001}F{}\u{0001}", i), fence);
    }

    result
}

#[async_trait]
impl PlatformAdapter for WhatsAppAdapter {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    async fn start(&self) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        // Verify the bridge is reachable and report its connection state.
        let response = self
            .client
            .get(format!("{}/health", self.base()))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(crate::error::Error::Agent(format!(
                "WhatsApp bridge health check failed (HTTP {})",
                response.status()
            )));
        }

        let payload: serde_json::Value = response.json().await.unwrap_or_default();
        let status = payload
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        info!(status = %status, "WhatsApp bridge connected");
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("WhatsApp adapter stopped");
        Ok(())
    }

    async fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        let text = if message.parse_markdown {
            markdown_to_whatsapp(&message.content)
        } else {
            message.content.clone()
        };

        let mut body = serde_json::json!({
            "chatId": message.channel_id,
            "message": text,
        });
        if let Some(ref reply_to) = message.reply_to {
            body["replyTo"] = serde_json::json!(reply_to);
        }

        self.post_bridge("/send", &body).await?;
        Ok(())
    }

    async fn send_message_tracked(&self, message: OutgoingMessage) -> Result<Option<String>> {
        let text = if message.parse_markdown {
            markdown_to_whatsapp(&message.content)
        } else {
            message.content.clone()
        };

        let mut body = serde_json::json!({
            "chatId": message.channel_id,
            "message": text,
        });
        if let Some(ref reply_to) = message.reply_to {
            body["replyTo"] = serde_json::json!(reply_to);
        }

        let payload = self.post_bridge("/send", &body).await?;
        let message_id = payload
            .get("messageId")
            .and_then(|id| id.as_str())
            .map(|s| s.to_string());
        Ok(message_id)
    }

    async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        message: OutgoingMessage,
    ) -> Result<()> {
        let text = if message.parse_markdown {
            markdown_to_whatsapp(&message.content)
        } else {
            message.content.clone()
        };

        let body = serde_json::json!({
            "chatId": channel_id,
            "messageId": message_id,
            "message": text,
        });
        self.post_bridge("/edit", &body).await?;
        Ok(())
    }

    async fn handle_update(&self, update: serde_json::Value) -> Result<Option<IncomingMessage>> {
        // Bridge event shape (see bridge_helpers.js::extractBridgeEvent):
        // { messageId, chatId, senderId, senderName, chatName, isGroup, body,
        //   hasMedia, mediaType, ..., timestamp }
        let body = update
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string();

        if body.is_empty() {
            return Ok(None);
        }

        let chat_id = update
            .get("chatId")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let sender_id = update
            .get("senderId")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let sender_name = update
            .get("senderName")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(Some(
            IncomingMessage::new("whatsapp", sender_id, sender_name, chat_id, body)
                .with_raw(update),
        ))
    }

    async fn poll_updates(&self) -> Result<Vec<serde_json::Value>> {
        if !self.is_enabled() {
            return Ok(Vec::new());
        }

        // The bridge drains its queue on each GET, so this returns only new
        // events since the last poll.
        let response = self
            .client
            .get(format!("{}/messages", self.base()))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(crate::error::Error::Agent(format!(
                "WhatsApp bridge /messages failed (HTTP {})",
                response.status()
            )));
        }

        let events: Vec<serde_json::Value> = response.json().await.unwrap_or_default();
        Ok(events)
    }

    fn config_json(&self) -> serde_json::Value {
        serde_json::json!({
            "platform": "whatsapp",
            "enabled": self.enabled,
            "bridge_url": self.bridge_url,
        })
    }
}

/// Discord adapter
pub struct DiscordAdapter {
    token: Option<String>,
    enabled: bool,
}

impl DiscordAdapter {
    /// Create a new Discord adapter
    pub fn new(token: Option<String>) -> Self {
        let enabled = token.is_some();
        Self { token, enabled }
    }

    fn api_url(&self) -> String {
        runtime_config().gateway.discord_api_base
    }

    /// Respond to an interaction (type 4: ChannelMessageWithSource, ephemeral acknowledgment)
    async fn respond_interaction(
        &self,
        interaction_id: &str,
        interaction_token: &str,
        message: &str,
    ) -> Result<()> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "type": 4, // CHANNEL_MESSAGE_WITH_SOURCE
            "data": {
                "content": message,
                "flags": 64 // EPHEMERAL
            }
        });

        let url = format!(
            "{}/interactions/{}/{}/callback",
            self.api_url(),
            interaction_id,
            interaction_token
        );

        let _ = client.post(&url).json(&body).send().await;
        Ok(())
    }
}

fn sensitive_authorization_header(scheme: &str, token: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("{} {}", scheme, token))
        .map_err(|e| crate::error::Error::Agent(format!("Invalid auth header: {}", e)))?;
    value.set_sensitive(true);
    Ok(value)
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    fn name(&self) -> &str {
        "discord"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    async fn start(&self) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        // Verify the token
        let client = reqwest::Client::new();
        let token = self
            .token
            .as_ref()
            .ok_or_else(|| crate::error::Error::MissingConfig {
                key: "discord_token".to_string(),
            })?;
        let response = client
            .get(format!("{}/users/@me", self.api_url()))
            .header(
                "Authorization",
                sensitive_authorization_header("Bot", token)?,
            )
            .send()
            .await?;

        if response.status().is_success() {
            info!("Discord bot started successfully");
            Ok(())
        } else {
            Err(crate::error::Error::Agent(
                "Failed to verify Discord bot token".to_string(),
            ))
        }
    }

    async fn stop(&self) -> Result<()> {
        info!("Discord adapter stopped");
        Ok(())
    }

    async fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        let client = reqwest::Client::new();
        let token = self
            .token
            .as_ref()
            .ok_or_else(|| crate::error::Error::MissingConfig {
                key: "discord_token".to_string(),
            })?;

        let body = serde_json::json!({
            "content": message.content,
        });

        let url = format!(
            "{}/channels/{}/messages",
            self.api_url(),
            message.channel_id
        );

        client
            .post(&url)
            .header(
                "Authorization",
                sensitive_authorization_header("Bot", token)?,
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        Ok(())
    }

    async fn send_message_tracked(&self, message: OutgoingMessage) -> Result<Option<String>> {
        let client = reqwest::Client::new();
        let token = self
            .token
            .as_ref()
            .ok_or_else(|| crate::error::Error::MissingConfig {
                key: "discord_token".to_string(),
            })?;

        let body = serde_json::json!({
            "content": message.content,
        });

        let url = format!(
            "{}/channels/{}/messages",
            self.api_url(),
            message.channel_id
        );

        let response = client
            .post(&url)
            .header(
                "Authorization",
                sensitive_authorization_header("Bot", token)?,
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(crate::error::Error::Agent(format!(
                "Discord sendMessage failed with status {}",
                response.status()
            )));
        }

        let resp_json: serde_json::Value = response.json().await.unwrap_or_default();
        let message_id = resp_json
            .get("id")
            .and_then(|id| id.as_str())
            .map(|s| s.to_string());

        Ok(message_id)
    }

    async fn handle_update(&self, update: serde_json::Value) -> Result<Option<IncomingMessage>> {
        // Parse Discord message create event
        let d = match update.get("d") {
            Some(d) => d,
            None => return Ok(None),
        };

        let author = match d.get("author") {
            Some(a) => a,
            None => return Ok(None),
        };

        let content = d
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        if content.is_empty() || author.get("bot").and_then(|b| b.as_bool()).unwrap_or(false) {
            return Ok(None);
        }

        let channel_id = d
            .get("channel_id")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        Ok(Some(
            IncomingMessage::new(
                "discord",
                author
                    .get("id")
                    .and_then(|id| id.as_str())
                    .unwrap_or("unknown"),
                author
                    .get("username")
                    .and_then(|u| u.as_str())
                    .unwrap_or("unknown"),
                channel_id,
                content,
            )
            .with_raw(update),
        ))
    }

    async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        message: OutgoingMessage,
    ) -> Result<()> {
        let client = reqwest::Client::new();
        let token = self
            .token
            .as_ref()
            .ok_or_else(|| crate::error::Error::MissingConfig {
                key: "discord_token".to_string(),
            })?;

        let body = serde_json::json!({
            "content": message.content,
        });

        let url = format!(
            "{}/channels/{}/messages/{}",
            self.api_url(),
            channel_id,
            message_id
        );

        let response = client
            .patch(&url)
            .header(
                "Authorization",
                sensitive_authorization_header("Bot", token)?,
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(crate::error::Error::Agent(format!(
                "Discord editMessage failed with status {}",
                response.status()
            )));
        }

        Ok(())
    }

    async fn handle_callback_query(&self, update: serde_json::Value) -> Result<()> {
        // Discord interaction shape: type 3 is MESSAGE_COMPONENT (button click)
        // Can be wrapped in gateway dispatch event {"t": "INTERACTION_CREATE", "d": {...}} or raw interaction {...}
        let d = if let Some(d) = update.get("d") {
            d
        } else {
            &update
        };

        let interaction_type = d.get("type").and_then(|v| v.as_i64()).unwrap_or(0);
        if interaction_type != 3 {
            return Ok(());
        }

        let interaction_id = d
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let interaction_token = d
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let data = match d.get("data") {
            Some(data) => data,
            None => return Ok(()),
        };

        let custom_id = data
            .get("custom_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let channel_id = d
            .get("channel_id")
            .and_then(|v| v.as_str())
            .map(|id| format!("discord:{}", id));

        // Button custom_id payloads: "approve:<id>", "session:<id>", "always:<id>", "reject:<id>" (and legacy "deny:<id>", "once:<id>").
        let (choice, id_str) = match custom_id.split_once(':') {
            Some(("approve", id)) | Some(("once", id)) => {
                (crate::approval::ApprovalChoice::AllowOnce, id)
            }
            Some(("session", id)) => (crate::approval::ApprovalChoice::Session, id),
            Some(("always", id)) => (crate::approval::ApprovalChoice::AlwaysAllow, id),
            Some(("reject", id)) | Some(("deny", id)) => {
                (crate::approval::ApprovalChoice::Reject, id)
            }
            _ => {
                if !interaction_id.is_empty() && !interaction_token.is_empty() {
                    let _ = self
                        .respond_interaction(&interaction_id, &interaction_token, "Unknown action")
                        .await;
                }
                return Ok(());
            }
        };

        let id: u64 = match id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                if !interaction_id.is_empty() && !interaction_token.is_empty() {
                    let _ = self
                        .respond_interaction(
                            &interaction_id,
                            &interaction_token,
                            "Malformed request id",
                        )
                        .await;
                }
                return Ok(());
            }
        };

        let resolved = resolve_pending_approval(id, choice, channel_id.as_deref());
        let feedback = if !resolved {
            "This request already expired."
        } else {
            match choice {
                crate::approval::ApprovalChoice::AllowOnce => "✅ Allowed once",
                crate::approval::ApprovalChoice::Session => "⏳ Allowed for this session",
                crate::approval::ApprovalChoice::AlwaysAllow => "🔒 Always allowed (persisted)",
                crate::approval::ApprovalChoice::Reject => "❌ Rejected",
            }
        };

        if !interaction_id.is_empty() && !interaction_token.is_empty() {
            let _ = self
                .respond_interaction(&interaction_id, &interaction_token, feedback)
                .await;
        }

        Ok(())
    }

    async fn send_approval_prompt(
        &self,
        channel_id: &str,
        tool_name: &str,
        arguments_preview: &str,
    ) -> Result<(
        u64,
        tokio::sync::oneshot::Receiver<crate::approval::ApprovalChoice>,
    )> {
        let (id, rx) = register_pending_approval(tool_name, arguments_preview);

        let preview: String = arguments_preview.chars().take(200).collect();
        let text = format!(
            "🔐 Tool approval required\n\nTool: {}\nArgs: {}\n\nChoose permission:",
            tool_name,
            if preview.is_empty() {
                "(none)"
            } else {
                &preview
            }
        );

        let body = serde_json::json!({
            "content": text,
            "components": [
                {
                    "type": 1, // Action Row
                    "components": [
                        {
                            "type": 2, // Button
                            "style": 3, // Success / Green
                            "label": "Allow Once",
                            "custom_id": format!("approve:{}", id)
                        },
                        {
                            "type": 2, // Button
                            "style": 1, // Primary / Blurple
                            "label": "Session",
                            "custom_id": format!("session:{}", id)
                        },
                        {
                            "type": 2, // Button
                            "style": 2, // Secondary / Grey
                            "label": "Always Allow",
                            "custom_id": format!("always:{}", id)
                        },
                        {
                            "type": 2, // Button
                            "style": 4, // Danger / Red
                            "label": "Reject",
                            "custom_id": format!("reject:{}", id)
                        }
                    ]
                }
            ]
        });

        let client = reqwest::Client::new();
        let token = match self.token.as_ref() {
            Some(t) => t,
            None => {
                drop_pending_approval(id);
                return Err(crate::error::Error::MissingConfig {
                    key: "discord_token".to_string(),
                });
            }
        };

        let url = format!("{}/channels/{}/messages", self.api_url(), channel_id);

        let res = client
            .post(&url)
            .header(
                "Authorization",
                sensitive_authorization_header("Bot", token)?,
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await;

        match res {
            Ok(resp) if resp.status().is_success() => Ok((id, rx)),
            Ok(resp) => {
                drop_pending_approval(id);
                Err(crate::error::Error::Agent(format!(
                    "Discord send_approval_prompt failed with status {}",
                    resp.status()
                )))
            }
            Err(e) => {
                drop_pending_approval(id);
                Err(crate::error::Error::Agent(e.to_string()))
            }
        }
    }

    fn config_json(&self) -> serde_json::Value {
        serde_json::json!({
            "platform": "discord",
            "enabled": self.enabled,
            "has_token": self.token.is_some()
        })
    }
}

/// Slack adapter
pub struct SlackAdapter {
    token: Option<String>,
    enabled: bool,
    /// Signing secret for verifying Slack request signatures (used in webhook mode)
    _signing_secret: Option<String>,
}

impl SlackAdapter {
    /// Create a new Slack adapter
    pub fn new(token: Option<String>, signing_secret: Option<String>) -> Self {
        let enabled = token.is_some();
        Self {
            token,
            enabled,
            _signing_secret: signing_secret,
        }
    }
}

#[async_trait]
impl PlatformAdapter for SlackAdapter {
    fn name(&self) -> &str {
        "slack"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    async fn start(&self) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }

        info!("Slack adapter started (event-based, no polling)");
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Slack adapter stopped");
        Ok(())
    }

    async fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        let client = reqwest::Client::new();
        let token = self
            .token
            .as_ref()
            .ok_or_else(|| crate::error::Error::MissingConfig {
                key: "slack_token".to_string(),
            })?;

        let body = serde_json::json!({
            "channel": message.channel_id,
            "text": message.content,
        });

        client
            .post(format!(
                "{}/chat.postMessage",
                runtime_config()
                    .gateway
                    .slack_api_base
                    .trim_end_matches('/')
            ))
            .header(
                "Authorization",
                sensitive_authorization_header("Bearer", token)?,
            )
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        Ok(())
    }

    async fn handle_update(&self, update: serde_json::Value) -> Result<Option<IncomingMessage>> {
        // Parse Slack event
        let event = match update.get("event") {
            Some(e) => e,
            None => return Ok(None),
        };

        let msg_type = event
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        if msg_type != "message" {
            return Ok(None);
        }

        let user = event
            .get("user")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string();

        let content = event
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();

        let channel = event
            .get("channel")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();

        if content.is_empty() {
            return Ok(None);
        }

        Ok(Some(
            IncomingMessage::new("slack", user.clone(), user, channel, content).with_raw(update),
        ))
    }

    fn config_json(&self) -> serde_json::Value {
        serde_json::json!({
            "platform": "slack",
            "enabled": self.enabled,
            "has_token": self.token.is_some()
        })
    }
}

// ========== Webhooks & Signature Verification ==========

/// Generic inbound webhook payload
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GenericWebhookPayload {
    pub message: String,
    pub target: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Slack inbound webhook event types
#[derive(Debug, Clone)]
pub enum SlackWebhookEvent {
    UrlVerification {
        challenge: String,
    },
    EventCallback {
        channel: String,
        user: String,
        text: String,
        thread_ts: Option<String>,
        event_ts: String,
    },
    Other(serde_json::Value),
}

/// Compute HMAC-SHA256 hex digest for signature verification.
pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    use sha2::Digest;
    // Standard HMAC RFC 2104 implementation using sha2::Sha256
    let block_size = 64;
    let mut k = [0u8; 64];
    if key.len() > block_size {
        let mut hasher = sha2::Sha256::new();
        hasher.update(key);
        let hashed_key = hasher.finalize();
        k[..hashed_key.len()].copy_from_slice(&hashed_key);
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut o_key_pad = [0x5cu8; 64];
    let mut i_key_pad = [0x36u8; 64];
    for i in 0..64 {
        o_key_pad[i] ^= k[i];
        i_key_pad[i] ^= k[i];
    }

    let mut inner_hasher = sha2::Sha256::new();
    inner_hasher.update(i_key_pad);
    inner_hasher.update(data);
    let inner_hash = inner_hasher.finalize();

    let mut outer_hasher = sha2::Sha256::new();
    outer_hasher.update(o_key_pad);
    outer_hasher.update(inner_hash);
    let outer_hash = outer_hasher.finalize();

    let mut hex = String::with_capacity(64);
    for b in outer_hash {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    hex
}

/// Verify Slack webhook signature `X-Slack-Signature` using `X-Slack-Request-Timestamp`
/// and signing secret. Rejects if timestamp skew is greater than 300 seconds (5 mins).
pub fn verify_slack_signature(
    signing_secret: &str,
    timestamp: i64,
    body: &[u8],
    signature_header: &str,
    now_secs: i64,
) -> bool {
    if (now_secs - timestamp).abs() > 300 {
        return false;
    }

    let sig_base = format!("v0:{}:", timestamp);
    let mut sig_base_bytes = sig_base.into_bytes();
    sig_base_bytes.extend_from_slice(body);

    let computed_hex = hmac_sha256_hex(signing_secret.as_bytes(), &sig_base_bytes);
    let expected = format!("v0={}", computed_hex);

    // Constant-time-like comparison
    if signature_header.len() != expected.len() {
        return false;
    }

    let mut diff = 0u8;
    for (a, b) in signature_header.bytes().zip(expected.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Parse generic webhook JSON payload
pub fn parse_generic_webhook(body: &[u8]) -> Result<GenericWebhookPayload> {
    serde_json::from_slice(body)
        .map_err(|e| crate::error::Error::Agent(format!("Invalid webhook payload: {}", e)))
}

/// Parse Slack event / challenge from webhook body
pub fn parse_slack_webhook_event(body: &[u8]) -> Result<SlackWebhookEvent> {
    let json: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| crate::error::Error::Agent(format!("Invalid Slack event JSON: {}", e)))?;

    if let Some(t) = json.get("type").and_then(|v| v.as_str()) {
        if t == "url_verification" {
            let challenge = json
                .get("challenge")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            return Ok(SlackWebhookEvent::UrlVerification { challenge });
        }
        if t == "event_callback" {
            if let Some(ev) = json.get("event") {
                let channel = ev
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let user = ev
                    .get("user")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let text = ev
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let thread_ts = ev
                    .get("thread_ts")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let event_ts = ev
                    .get("ts")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                return Ok(SlackWebhookEvent::EventCallback {
                    channel,
                    user,
                    text,
                    thread_ts,
                    event_ts,
                });
            }
        }
    }

    Ok(SlackWebhookEvent::Other(json))
}

// ========== Message splitting ==========

/// Split a long message into chunks of at most `max_chars` characters,
/// preferring line boundaries so markdown structure stays intact.
///
/// Telegram's limit is 4096 UTF-16 units; callers should pass a value
/// comfortably below that (e.g. 4000) to leave room for escaping overhead.
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for line in text.split_inclusive('\n') {
        let line_len = line.chars().count();

        // A single line longer than the cap must be hard-split by chars.
        if line_len > max_chars {
            // Flush what we have first.
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            let mut piece = String::new();
            let mut piece_len = 0usize;
            for ch in line.chars() {
                if piece_len >= max_chars {
                    chunks.push(std::mem::take(&mut piece));
                    piece_len = 0;
                }
                piece.push(ch);
                piece_len += 1;
            }
            if !piece.is_empty() {
                chunks.push(piece);
            }
            continue;
        }

        if current_len + line_len > max_chars {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        current.push_str(line);
        current_len += line_len;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

// ========== Webhook HTTP Server & Routing ==========

struct WebhookAdapter;

#[async_trait]
impl PlatformAdapter for WebhookAdapter {
    fn name(&self) -> &str {
        "webhook"
    }

    fn is_enabled(&self) -> bool {
        true
    }

    async fn start(&self) -> Result<()> {
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn send_message(&self, message: OutgoingMessage) -> Result<()> {
        debug!(channel = %message.channel_id, "Discarding fire-and-forget webhook reply");
        Ok(())
    }

    async fn handle_update(&self, _update: serde_json::Value) -> Result<Option<IncomingMessage>> {
        Ok(None)
    }

    fn config_json(&self) -> serde_json::Value {
        serde_json::json!({ "platform": "webhook", "enabled": true })
    }
}

#[derive(Clone)]
pub struct WebhookServerState {
    pub incoming_tx: Option<tokio::sync::mpsc::Sender<IncomingMessage>>,
    pub slack_signing_secret: Option<String>,
}

pub fn create_webhook_router(state: WebhookServerState) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/health", get(webhook_health_handler))
        .route("/webhook", post(webhook_generic_handler))
        .route("/webhook/generic", post(webhook_generic_handler))
        .route("/webhook/slack", post(webhook_slack_handler))
        .with_state(state)
}

pub async fn serve_webhook_listener<F>(
    listener: tokio::net::TcpListener,
    state: WebhookServerState,
    shutdown: F,
) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, create_webhook_router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|error| crate::error::Error::Agent(format!("Webhook server failed: {error}")))
}

async fn webhook_health_handler() -> axum::http::StatusCode {
    axum::http::StatusCode::OK
}

async fn webhook_generic_handler(
    axum::extract::State(state): axum::extract::State<WebhookServerState>,
    body: axum::body::Bytes,
) -> std::result::Result<axum::http::StatusCode, (axum::http::StatusCode, String)> {
    let payload = parse_generic_webhook(&body).map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            format!("Invalid generic webhook payload: {e}"),
        )
    })?;

    let (platform, channel) = match payload.target.as_deref() {
        Some(target) => {
            let (platform, channel) = target.split_once(':').ok_or_else(|| {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    "Webhook target must use platform:channel format".to_string(),
                )
            })?;
            if platform.is_empty() || channel.is_empty() {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    "Webhook target must use platform:channel format".to_string(),
                ));
            }
            (platform, channel)
        }
        None => ("webhook", "default"),
    };

    if let Some(tx) = &state.incoming_tx {
        let user_name = payload
            .source
            .unwrap_or_else(|| "webhook_sender".to_string());
        let msg = IncomingMessage::new(
            platform,
            user_name.clone(),
            user_name,
            channel,
            payload.message,
        );
        tx.send(msg).await.map_err(|_| {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Gateway is shutting down".to_string(),
            )
        })?;
    }

    Ok(axum::http::StatusCode::ACCEPTED)
}

async fn webhook_slack_handler(
    axum::extract::State(state): axum::extract::State<WebhookServerState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> std::result::Result<axum::response::Response, (axum::http::StatusCode, String)> {
    use axum::response::IntoResponse;
    let secret = state.slack_signing_secret.as_deref().ok_or_else(|| {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Slack signing secret is not configured".to_string(),
        )
    })?;
    let timestamp_str = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                "Missing X-Slack-Request-Timestamp header".to_string(),
            )
        })?;
    let timestamp: i64 = timestamp_str.parse().map_err(|_| {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            "Invalid X-Slack-Request-Timestamp header".to_string(),
        )
    })?;
    let signature = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                "Missing X-Slack-Signature header".to_string(),
            )
        })?;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    if !verify_slack_signature(secret, timestamp, &body, signature, now_secs) {
        return Err((
            axum::http::StatusCode::UNAUTHORIZED,
            "Slack signature verification failed".to_string(),
        ));
    }

    let event = parse_slack_webhook_event(&body).map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            format!("Invalid Slack event payload: {e}"),
        )
    })?;

    match event {
        SlackWebhookEvent::UrlVerification { challenge } => {
            let json = serde_json::json!({ "challenge": challenge });
            Ok(axum::Json(json).into_response())
        }
        SlackWebhookEvent::EventCallback {
            channel,
            user,
            text,
            ..
        } => {
            if let Some(tx) = &state.incoming_tx {
                let msg = IncomingMessage::new("slack", user.clone(), user, channel, text);
                let _ = tx.send(msg).await;
            }
            Ok(axum::http::StatusCode::OK.into_response())
        }
        SlackWebhookEvent::Other(_) => Ok(axum::http::StatusCode::OK.into_response()),
    }
}

// ========== Markdown → Telegram MarkdownV2 conversion ==========

/// Characters that must be backslash-escaped in MarkdownV2 outside of
/// code/link entities.
const V2_SPECIAL: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// Escape text for use outside any MarkdownV2 entity.
fn escape_v2_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if V2_SPECIAL.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escape text inside a code entity: only backslash and backtick matter.
fn escape_v2_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if ch == '\\' || ch == '`' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escape a URL inside a link entity: only backslash and `)` matter.
fn escape_v2_url(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if ch == '\\' || ch == ')' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Find the index of the next `delim` char at or after `start`, ignoring
/// backslash-escaped occurrences. Returns None if not found.
fn find_delim(chars: &[char], start: usize, delim: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == delim {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Convert inline markdown (bold, italic, code, links) within a single line
/// to MarkdownV2, escaping everything else.
fn convert_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() * 2);
    let mut i = 0;

    while i < chars.len() {
        // Inline code: `...`
        if chars[i] == '`' {
            if let Some(end) = find_delim(&chars, i + 1, '`') {
                let code: String = chars[i + 1..end].iter().collect();
                out.push('`');
                out.push_str(&escape_v2_code(&code));
                out.push('`');
                i = end + 1;
                continue;
            }
        }

        // Bold: **...**
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            // Find closing **
            let mut j = i + 2;
            let mut close: Option<usize> = None;
            while j + 1 < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == '*' && chars[j + 1] == '*' {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = close {
                let inner: String = chars[i + 2..end].iter().collect();
                out.push('*');
                out.push_str(&escape_v2_text(&inner));
                out.push('*');
                i = end + 2;
                continue;
            }
        }

        // Italic: *...* (single)
        if chars[i] == '*' {
            if let Some(end) = find_delim(&chars, i + 1, '*') {
                let inner: String = chars[i + 1..end].iter().collect();
                out.push('_');
                out.push_str(&escape_v2_text(&inner));
                out.push('_');
                i = end + 1;
                continue;
            }
        }

        // Link: [text](url)
        if chars[i] == '[' {
            if let Some(text_end) = find_delim(&chars, i + 1, ']') {
                if text_end + 1 < chars.len() && chars[text_end + 1] == '(' {
                    if let Some(url_end) = find_delim(&chars, text_end + 2, ')') {
                        let link_text: String = chars[i + 1..text_end].iter().collect();
                        let url: String = chars[text_end + 2..url_end].iter().collect();
                        out.push('[');
                        out.push_str(&escape_v2_text(&link_text));
                        out.push_str("](");
                        out.push_str(&escape_v2_url(&url));
                        out.push(')');
                        i = url_end + 1;
                        continue;
                    }
                }
            }
        }

        // Plain character: escape if special.
        if V2_SPECIAL.contains(&chars[i]) {
            out.push('\\');
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Split a markdown table row `| a | b |` into its trimmed cells `["a", "b"]`.
fn split_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    // Strip a single leading/trailing pipe so "| a | b |" -> " a | b ".
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or_else(|| trimmed.strip_prefix('|').unwrap_or(trimmed));
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// True if `line` is a table separator row like `|---|:---:|---:|`.
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.contains('|') && !trimmed.contains('-') {
        return false;
    }
    let cells = split_table_row(trimmed);
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|c| {
        !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ') && c.contains('-')
    })
}

/// True if `line` looks like the start of a table row (leading pipe).
fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// Convert a parsed table (header + data rows) into a bullet list.
///
/// Each data row becomes one bullet of `header: value` pairs joined by ` · `,
/// which reads cleanly in Telegram without the unsupported table syntax.
fn table_to_bullets(header: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in rows {
        let mut parts: Vec<String> = Vec::new();
        for (i, cell) in row.iter().enumerate() {
            let key = header.get(i).map(|h| h.as_str()).unwrap_or("");
            let val = cell.trim();
            if val.is_empty() {
                continue;
            }
            let piece = if key.is_empty() {
                convert_inline(val)
            } else {
                format!("{}: {}", convert_inline(key), convert_inline(val))
            };
            parts.push(piece);
        }
        if parts.is_empty() {
            continue;
        }
        out.push_str("• ");
        out.push_str(&parts.join(" · "));
        out.push('\n');
    }
    out
}

/// Convert standard markdown (as produced by an LLM) to Telegram MarkdownV2.
///
/// Handles fenced code blocks, headings, list items, tables (rendered as
/// bullet lists), and inline bold/italic/code/links. Everything else is
/// escaped so Telegram renders it literally instead of rejecting the message.
pub fn markdown_to_markdownv2(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    let mut in_code_block = false;

    let lines: Vec<&str> = input.lines().collect();
    let mut idx = 0;

    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim_start();

        // Fenced code block delimiter.
        if trimmed.starts_with("```") {
            if in_code_block {
                out.push_str("```\n");
                in_code_block = false;
            } else {
                in_code_block = true;
                // Keep the language tag (```rust) for syntax highlighting.
                out.push_str(trimmed);
                out.push('\n');
            }
            idx += 1;
            continue;
        }

        if in_code_block {
            out.push_str(&escape_v2_code(line));
            out.push('\n');
            idx += 1;
            continue;
        }

        // Markdown table: a header row starting with `|` immediately followed
        // by a separator row. Telegram has no table syntax, so render the rows
        // as a bullet list of `header: value` pairs.
        if is_table_row(line) && idx + 1 < lines.len() && is_table_separator(lines[idx + 1]) {
            let header = split_table_row(line);
            idx += 2; // consume header + separator
            let mut rows: Vec<Vec<String>> = Vec::new();
            while idx < lines.len() && is_table_row(lines[idx]) {
                rows.push(split_table_row(lines[idx]));
                idx += 1;
            }
            out.push_str(&table_to_bullets(&header, &rows));
            continue;
        }

        // Horizontal rule: a lone line of --- / *** / ___ (3+). Telegram has
        // no <hr>; swap in a unicode rule so the separator still reads.
        {
            let t = trimmed.trim();
            let is_hr = t.len() >= 3
                && (t.chars().all(|c| c == '-')
                    || t.chars().all(|c| c == '*')
                    || t.chars().all(|c| c == '_'));
            if is_hr {
                out.push_str("────────────────\n");
                idx += 1;
                continue;
            }
        }

        // Heading: # / ## / ### ... → bold line.
        if trimmed.starts_with('#') {
            let rest = trimmed.trim_start_matches('#').trim_start();
            if !rest.is_empty() {
                out.push('*');
                out.push_str(&escape_v2_text(rest));
                out.push_str("*\n");
                idx += 1;
                continue;
            }
        }

        // Blockquote: "> text" → Telegram MarkdownV2 blockquote (each line
        // prefixed with ">"). Consecutive "> " lines stay as separate lines.
        if trimmed.starts_with('>') {
            let rest = trimmed.strip_prefix('>').unwrap_or("").trim_start();
            out.push('>');
            if !rest.is_empty() {
                out.push(' ');
                out.push_str(&convert_inline(rest));
            }
            out.push('\n');
            idx += 1;
            continue;
        }

        // Unordered list item: "- x" or "* x" → bullet.
        if (trimmed.starts_with("- ") || trimmed.starts_with("* ")) && trimmed.len() > 2 {
            let rest = &trimmed[2..];
            out.push_str("• ");
            out.push_str(&convert_inline(rest));
            out.push('\n');
            idx += 1;
            continue;
        }

        out.push_str(&convert_inline(line));
        out.push('\n');
        idx += 1;
    }

    // Drop the trailing newline we added if the input didn't end with one.
    if !input.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serial_test::serial;
    use tower::ServiceExt;

    #[tokio::test]
    #[serial]
    async fn poisoned_pending_approvals_still_registers_and_resolves() {
        let _ = std::thread::spawn(|| {
            let _guard = PENDING_APPROVALS.lock().unwrap();
            panic!("poison pending approvals");
        })
        .join();

        let (id, receiver) = register_pending_approval("terminal", "echo hello");
        assert!(resolve_pending_approval(
            id,
            crate::approval::ApprovalChoice::AllowOnce,
            None
        ));
        assert_eq!(
            receiver.await,
            Ok(crate::approval::ApprovalChoice::AllowOnce)
        );
    }

    #[test]
    fn split_message_short_stays_single() {
        let chunks = split_message("hello world", 100);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn split_message_breaks_at_line_boundary() {
        let text = "line one\nline two\nline three";
        let chunks = split_message(text, 18);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].ends_with('\n'));
        assert_eq!(chunks[0], "line one\nline two\n");
        assert_eq!(chunks[1], "line three");
    }

    #[test]
    fn split_message_hard_splits_giant_line() {
        let text = "a".repeat(50);
        let chunks = split_message(&text, 20);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 20);
        assert_eq!(chunks[2].len(), 10);
    }

    #[test]
    fn markdown_v2_escapes_special_chars() {
        assert_eq!(escape_v2_text("a.b!c"), "a\\.b\\!c");
        assert_eq!(escape_v2_text("plain"), "plain");
    }

    #[test]
    fn markdown_v2_converts_bold_and_heading() {
        assert_eq!(markdown_to_markdownv2("**bold**"), "*bold*");
        assert_eq!(markdown_to_markdownv2("## Title"), "*Title*");
    }

    #[test]
    fn markdown_v2_converts_list_items() {
        assert_eq!(markdown_to_markdownv2("- item one"), "• item one");
        assert_eq!(markdown_to_markdownv2("* item two"), "• item two");
    }

    #[test]
    fn markdown_v2_keeps_code_blocks_verbatim() {
        let input = "```\nlet x = 1_2;\n```";
        let out = markdown_to_markdownv2(input);
        // Inside code entities only backslash and backtick need escaping.
        assert!(out.contains("let x = 1_2;"));
        assert!(out.starts_with("```"));
    }

    #[test]
    fn markdown_v2_converts_links() {
        let out = markdown_to_markdownv2("[site](https://example.com/a)");
        assert_eq!(out, "[site](https://example.com/a)");
    }

    #[test]
    fn markdown_v2_escapes_dots_in_plain_text() {
        let out = markdown_to_markdownv2("Hello, world!");
        assert_eq!(out, "Hello, world\\!");
    }

    #[test]
    fn markdown_v2_converts_horizontal_rule() {
        assert_eq!(markdown_to_markdownv2("---"), "────────────────");
        assert_eq!(markdown_to_markdownv2("***"), "────────────────");
        assert_eq!(markdown_to_markdownv2("___"), "────────────────");
        // But a list item is NOT a rule.
        assert_eq!(markdown_to_markdownv2("- item"), "• item");
    }

    #[test]
    fn markdown_v2_converts_blockquote() {
        // Single-line blockquote keeps the ">" prefix (Telegram MarkdownV2
        // blockquote) and escapes inner specials.
        assert_eq!(
            markdown_to_markdownv2("> 💡 Tips: hello world."),
            "> 💡 Tips: hello world\\."
        );
        // Consecutive quoted lines stay as separate blockquote lines.
        assert_eq!(
            markdown_to_markdownv2("> line one\n> line two"),
            "> line one\n> line two"
        );
        // Inline formatting inside a quote is converted.
        assert_eq!(markdown_to_markdownv2("> **bold** text"), "> *bold* text");
        // A bare ">" with no content still emits a quote line.
        assert_eq!(markdown_to_markdownv2(">"), ">");
    }

    #[test]
    fn split_table_row_parses_cells() {
        assert_eq!(split_table_row("| a | b | c |"), vec!["a", "b", "c"]);
        assert_eq!(split_table_row("|a|b|"), vec!["a", "b"]);
        assert_eq!(split_table_row("a | b"), vec!["a", "b"]);
    }

    #[test]
    fn is_table_separator_detects_alignment_rows() {
        assert!(is_table_separator("|---|---|"));
        assert!(is_table_separator("|:---|:---:|---:|"));
        assert!(is_table_separator("---|---"));
        assert!(!is_table_separator("| a | b |"));
        assert!(!is_table_separator("hello"));
    }

    #[test]
    fn markdown_v2_converts_table_to_bullets() {
        let input = "| Name | Age |\n|---|---|\n| Alice | 30 |\n| Bob | 25 |";
        let out = markdown_to_markdownv2(input);
        assert!(out.contains("• Name: Alice · Age: 30"));
        assert!(out.contains("• Name: Bob · Age: 25"));
        // No raw pipes should survive.
        assert!(!out.contains('|'));
    }

    #[test]
    fn markdown_v2_table_skips_empty_cells() {
        let input = "| A | B |\n|---|---|\n| x |  |";
        let out = markdown_to_markdownv2(input);
        assert!(out.contains("• A: x"));
        assert!(!out.contains("B:"));
    }

    #[test]
    fn markdown_v2_leaves_pipe_without_separator_alone() {
        // A lone pipe line with no separator row is not a table; escape it.
        let out = markdown_to_markdownv2("a | b");
        assert_eq!(out, "a \\| b");
    }

    #[test]
    fn markdown_v2_table_inside_code_block_stays_verbatim() {
        let input = "```\n| a | b |\n|---|---|\n```";
        let out = markdown_to_markdownv2(input);
        // Table syntax inside a fence must not be converted.
        assert!(out.contains("| a | b |"));
    }

    #[test]
    fn test_incoming_message() {
        let msg = IncomingMessage::new("telegram", "12345", "testuser", "67890", "Hello, world!");

        assert_eq!(msg.platform, "telegram");
        assert_eq!(msg.user_id, "12345");
        assert_eq!(msg.content, "Hello, world!");
    }

    #[test]
    fn test_outgoing_message() {
        let msg = OutgoingMessage::new("67890", "Response to you")
            .no_markdown()
            .with_reply_to("111");

        assert_eq!(msg.channel_id, "67890");
        assert_eq!(msg.content, "Response to you");
        assert!(!msg.parse_markdown);
        assert_eq!(msg.reply_to, Some("111".to_string()));
    }

    #[tokio::test]
    async fn test_gateway_config() {
        let config = GatewayConfig::default();
        assert!(!config.telegram_enabled);
        assert!(!config.discord_enabled);
    }

    #[test]
    fn sensitive_authorization_header_marks_token_sensitive() {
        let header = sensitive_authorization_header("Bot", "secret-token").unwrap();

        assert_eq!(header.to_str().unwrap(), "Bot secret-token");
        assert!(header.is_sensitive());
    }

    #[test]
    fn sensitive_authorization_header_rejects_invalid_value() {
        let result = sensitive_authorization_header("Bearer", "bad\n token");

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_telegram_adapter_disabled() {
        let adapter = TelegramAdapter::new(None);
        assert!(!adapter.is_enabled());
    }

    #[tokio::test]
    async fn test_discord_adapter_disabled() {
        let adapter = DiscordAdapter::new(None);
        assert!(!adapter.is_enabled());
    }

    #[tokio::test]
    async fn discord_approval_flow_resolves_receiver() {
        let adapter = DiscordAdapter::new(Some("test-token".to_string()));
        let (id, rx) = register_pending_approval("execute_command", "rm -rf /tmp/test");

        // Simulate Discord interaction webhook / event
        let interaction_update = serde_json::json!({
            "type": 3,
            "id": "interaction_123",
            "token": "token_abc",
            "channel_id": "987654321",
            "data": {
                "custom_id": format!("approve:{}", id),
                "component_type": 2
            }
        });

        adapter
            .handle_callback_query(interaction_update)
            .await
            .expect("handle interaction callback");

        let choice = rx.await.expect("receive approval choice");
        assert_eq!(choice, crate::approval::ApprovalChoice::AllowOnce);
    }

    #[tokio::test]
    async fn discord_approval_reject_flow_resolves_receiver() {
        let adapter = DiscordAdapter::new(Some("test-token".to_string()));
        let (id, rx) = register_pending_approval("execute_command", "rm -rf /tmp/test");

        let interaction_update = serde_json::json!({
            "d": {
                "type": 3,
                "id": "interaction_123",
                "token": "token_abc",
                "channel_id": "987654321",
                "data": {
                    "custom_id": format!("reject:{}", id),
                    "component_type": 2
                }
            }
        });

        adapter
            .handle_callback_query(interaction_update)
            .await
            .expect("handle interaction callback");

        let choice = rx.await.expect("receive approval choice");
        assert_eq!(choice, crate::approval::ApprovalChoice::Reject);
    }

    #[tokio::test]
    async fn discord_send_approval_prompt_fails_closed_without_token() {
        let adapter = DiscordAdapter::new(None);
        let result = adapter
            .send_approval_prompt("channel_123", "bash", "echo hi")
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn discord_send_message_returns_missing_config_without_token() {
        let adapter = DiscordAdapter::new(None);
        let result = adapter
            .send_message(OutgoingMessage::new("channel", "hello"))
            .await;

        assert!(matches!(
            result,
            Err(crate::error::Error::MissingConfig { key }) if key == "discord_token"
        ));
    }

    #[tokio::test]
    async fn test_slack_adapter_disabled() {
        let adapter = SlackAdapter::new(None, None);
        assert!(!adapter.is_enabled());
    }

    #[tokio::test]
    async fn slack_send_message_returns_missing_config_without_token() {
        let adapter = SlackAdapter::new(None, None);
        let result = adapter
            .send_message(OutgoingMessage::new("channel", "hello"))
            .await;

        assert!(matches!(
            result,
            Err(crate::error::Error::MissingConfig { key }) if key == "slack_token"
        ));
    }

    #[tokio::test]
    async fn gateway_run_errors_without_enabled_adapters() {
        let gateway = Gateway::new(GatewayConfig::default());
        // No adapters registered at all -> run() must fail fast.
        let result = gateway.run().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn gateway_run_errors_when_only_disabled_adapters() {
        let gateway = Gateway::new(GatewayConfig::default())
            .with_adapter(Arc::new(TelegramAdapter::new(None)));
        // Adapter registered but disabled (no token) -> still no enabled adapters.
        let result = gateway.run().await;
        assert!(result.is_err());
    }

    struct CapturingHandler {
        incoming_tx: tokio::sync::mpsc::Sender<IncomingMessage>,
    }

    #[async_trait]
    impl MessageHandler for CapturingHandler {
        async fn handle(
            &self,
            message: IncomingMessage,
            _sink: Arc<dyn MessageSink>,
            _cancel: Arc<std::sync::atomic::AtomicBool>,
        ) -> Result<()> {
            self.incoming_tx
                .send(message)
                .await
                .map_err(|_| crate::error::Error::Agent("test receiver dropped".to_string()))
        }
    }

    #[tokio::test]
    async fn gateway_run_accepts_webhook_only_triggers() {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve address");
        let address = probe.local_addr().expect("reserved address");
        drop(probe);

        let (incoming_tx, mut incoming_rx) = tokio::sync::mpsc::channel(1);
        let config = GatewayConfig {
            webhooks_enabled: true,
            webhooks_addr: Some(address.to_string()),
            ..Default::default()
        };
        let gateway =
            Arc::new(Gateway::new(config).with_handler(Arc::new(CapturingHandler { incoming_tx })));
        let running_gateway = gateway.clone();
        let run_task = tokio::spawn(async move { running_gateway.run().await });

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !gateway.is_running().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("gateway started");

        let response = reqwest::Client::new()
            .post(format!("http://{address}/webhook/generic"))
            .json(&serde_json::json!({
                "message": "Run CI diagnostics",
                "source": "ci"
            }))
            .send()
            .await
            .expect("post webhook");
        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

        let received = tokio::time::timeout(std::time::Duration::from_secs(2), incoming_rx.recv())
            .await
            .expect("handler invoked")
            .expect("handler message");
        assert_eq!(received.content, "Run CI diagnostics");

        gateway.stop().await.expect("stop gateway");
        tokio::time::timeout(std::time::Duration::from_secs(2), run_task)
            .await
            .expect("gateway run stopped")
            .expect("gateway task joined")
            .expect("gateway run succeeded");
    }

    #[tokio::test]
    async fn telegram_poll_updates_disabled_returns_empty() {
        let adapter = TelegramAdapter::new(None);
        let updates = adapter.poll_updates().await.unwrap();
        assert!(updates.is_empty());
    }

    #[tokio::test]
    async fn slack_poll_updates_default_returns_empty() {
        // Slack relies on webhooks; the default poll_updates impl yields nothing.
        let adapter = SlackAdapter::new(None, None);
        let updates = adapter.poll_updates().await.unwrap();
        assert!(updates.is_empty());
    }

    #[tokio::test]
    async fn telegram_handle_update_parses_message() {
        let adapter = TelegramAdapter::new(Some("test-token".to_string()));
        let update = serde_json::json!({
            "update_id": 42,
            "message": {
                "message_id": 7,
                "text": "hello bot",
                "chat": { "id": 999, "type": "private" },
                "from": { "id": 123, "username": "nix" }
            }
        });

        let incoming = adapter.handle_update(update).await.unwrap().unwrap();
        assert_eq!(incoming.platform, "telegram");
        assert_eq!(incoming.user_id, "123");
        assert_eq!(incoming.username, "nix");
        assert_eq!(incoming.channel_id, "999");
        assert_eq!(incoming.content, "hello bot");
    }

    #[tokio::test]
    async fn telegram_handle_update_ignores_non_message() {
        let adapter = TelegramAdapter::new(Some("test-token".to_string()));
        let update = serde_json::json!({ "update_id": 1, "edited_message": {} });
        assert!(adapter.handle_update(update).await.unwrap().is_none());
    }

    // ---------- WhatsApp ----------

    #[tokio::test]
    async fn test_whatsapp_adapter_disabled() {
        let adapter = WhatsAppAdapter::new(None);
        assert!(!adapter.is_enabled());
    }

    #[tokio::test]
    async fn whatsapp_poll_updates_disabled_returns_empty() {
        let adapter = WhatsAppAdapter::new(None);
        let updates = adapter.poll_updates().await.unwrap();
        assert!(updates.is_empty());
    }

    #[tokio::test]
    async fn whatsapp_handle_update_parses_bridge_event() {
        let adapter = WhatsAppAdapter::new(Some("http://127.0.0.1:3000".to_string()));
        let update = serde_json::json!({
            "messageId": "ABC123",
            "chatId": "6285179682870@s.whatsapp.net",
            "senderId": "6285179682870@s.whatsapp.net",
            "senderName": "Nix",
            "chatName": "Nix",
            "isGroup": false,
            "body": "halo bot",
            "hasMedia": false,
            "timestamp": 1755500000
        });

        let incoming = adapter.handle_update(update).await.unwrap().unwrap();
        assert_eq!(incoming.platform, "whatsapp");
        assert_eq!(incoming.user_id, "6285179682870@s.whatsapp.net");
        assert_eq!(incoming.username, "Nix");
        assert_eq!(incoming.channel_id, "6285179682870@s.whatsapp.net");
        assert_eq!(incoming.content, "halo bot");
    }

    #[tokio::test]
    async fn whatsapp_handle_update_ignores_empty_body() {
        let adapter = WhatsAppAdapter::new(Some("http://127.0.0.1:3000".to_string()));
        let update = serde_json::json!({
            "chatId": "x@s.whatsapp.net",
            "body": "",
            "hasMedia": false
        });
        assert!(adapter.handle_update(update).await.unwrap().is_none());
    }

    #[test]
    fn whatsapp_markdown_bold() {
        assert_eq!(markdown_to_whatsapp("**bold**"), "*bold*");
        assert_eq!(markdown_to_whatsapp("__bold__"), "*bold*");
    }

    #[test]
    fn whatsapp_markdown_italic() {
        assert_eq!(markdown_to_whatsapp("*italic*"), "_italic_");
    }

    #[test]
    fn whatsapp_markdown_bold_and_italic_no_crosstalk() {
        // **bold** must not be mangled into italic by the single-* pass.
        assert_eq!(
            markdown_to_whatsapp("**bold** and *italic*"),
            "*bold* and _italic_"
        );
    }

    #[test]
    fn whatsapp_markdown_strikethrough() {
        assert_eq!(markdown_to_whatsapp("~~gone~~"), "~gone~");
    }

    #[test]
    fn whatsapp_markdown_header_to_bold() {
        assert_eq!(markdown_to_whatsapp("# Title"), "*Title*");
        assert_eq!(markdown_to_whatsapp("## **Title**"), "*Title*");
    }

    #[test]
    fn whatsapp_markdown_link() {
        assert_eq!(
            markdown_to_whatsapp("[docs](https://example.com)"),
            "docs (https://example.com)"
        );
    }

    #[test]
    fn whatsapp_markdown_protects_code() {
        // Inline code and fenced blocks must not be reformatted.
        assert_eq!(markdown_to_whatsapp("`**not bold**`"), "`**not bold**`");
        assert_eq!(
            markdown_to_whatsapp("```\n**raw**\n```"),
            "```\n**raw**\n```"
        );
    }

    #[test]
    fn whatsapp_markdown_list_bullet_not_italic() {
        // "* item" is a list bullet, not italic.
        assert_eq!(markdown_to_whatsapp("* item one"), "* item one");
    }

    #[test]
    fn hmac_sha256_hex_computes_expected_digest() {
        let key = b"secret";
        let message = b"hello world";
        let computed = hmac_sha256_hex(key, message);
        // Computed known test vector for HMAC-SHA256("secret", "hello world")
        assert_eq!(
            computed,
            "734cc62f32841568f45715aeb9f4d7891324e6d948e4c6c60c0621cdac48623a"
        );
    }

    #[test]
    fn verify_slack_signature_validates_and_rejects() {
        let secret = "8f742231b10e8888abcd99yyzz";
        let timestamp = 1700000000;
        let body = r#"{"type":"url_verification","challenge":"test_challenge"}"#;
        let sig_base = format!("v0:{}:{}", timestamp, body);
        let expected_hash = hmac_sha256_hex(secret.as_bytes(), sig_base.as_bytes());
        let valid_header = format!("v0={}", expected_hash);

        // Valid signature within timestamp window
        assert!(verify_slack_signature(
            secret,
            timestamp,
            body.as_bytes(),
            &valid_header,
            timestamp + 30
        ));

        // Invalid signature header
        assert!(!verify_slack_signature(
            secret,
            timestamp,
            body.as_bytes(),
            "v0=invalidhash",
            timestamp + 30
        ));

        // Timestamp too old (skew > 300s)
        assert!(!verify_slack_signature(
            secret,
            timestamp,
            body.as_bytes(),
            &valid_header,
            timestamp + 400
        ));
    }

    #[test]
    fn parse_webhook_payload_generic() {
        let raw = r#"{
            "event": "alert",
            "message": "Disk space low on srv-1",
            "source": "monitoring",
            "target": "telegram:12345"
        }"#;
        let parsed = parse_generic_webhook(raw.as_bytes()).expect("parse generic webhook");
        assert_eq!(parsed.message, "Disk space low on srv-1");
        assert_eq!(parsed.target.as_deref(), Some("telegram:12345"));
        assert_eq!(parsed.source.as_deref(), Some("monitoring"));
    }

    #[test]
    fn parse_slack_url_verification_event() {
        let raw = r#"{
            "type": "url_verification",
            "token": "Jhj5dZrVaK7ZwHHjRyZWjbDl",
            "challenge": "3eZbrw1aBm2rZgRNFDxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"
        }"#;
        let event =
            parse_slack_webhook_event(raw.as_bytes()).expect("parse slack url verification");
        match event {
            SlackWebhookEvent::UrlVerification { challenge } => {
                assert_eq!(
                    challenge,
                    "3eZbrw1aBm2rZgRNFDxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"
                );
            }
            _ => panic!("expected UrlVerification event"),
        }
    }

    #[test]
    fn parse_slack_app_mention_event() {
        let raw = r#"{
            "type": "event_callback",
            "event_id": "Ev123456",
            "event_time": 1700000000,
            "event": {
                "type": "app_mention",
                "user": "U123456",
                "text": "<@U999999> run check status",
                "ts": "1700000000.000200",
                "channel": "C123456"
            }
        }"#;
        let event = parse_slack_webhook_event(raw.as_bytes()).expect("parse slack app mention");
        match event {
            SlackWebhookEvent::EventCallback {
                channel,
                user,
                text,
                thread_ts,
                event_ts,
            } => {
                assert_eq!(channel, "C123456");
                assert_eq!(user, "U123456");
                assert_eq!(text, "<@U999999> run check status");
                assert_eq!(event_ts, "1700000000.000200");
                assert_eq!(thread_ts, None);
            }
            _ => panic!("expected EventCallback event"),
        }
    }

    #[tokio::test]
    async fn webhook_listener_receives_generic_trigger_and_shuts_down() {
        let (incoming_tx, mut incoming_rx) = tokio::sync::mpsc::channel(1);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let state = WebhookServerState {
            incoming_tx: Some(incoming_tx),
            slack_signing_secret: None,
        };

        let server = tokio::spawn(serve_webhook_listener(listener, state, async {
            let _ = shutdown_rx.await;
        }));

        let response = reqwest::Client::new()
            .post(format!("http://{address}/webhook/generic"))
            .json(&serde_json::json!({
                "message": "Run CI diagnostics",
                "source": "ci"
            }))
            .send()
            .await
            .expect("post webhook");

        assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        let received = incoming_rx.recv().await.expect("receive trigger");
        assert_eq!(received.platform, "webhook");
        assert_eq!(received.channel_id, "default");
        assert_eq!(received.content, "Run CI diagnostics");

        shutdown_tx.send(()).expect("request shutdown");
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("listener stopped")
            .expect("listener task joined")
            .expect("listener succeeded");
    }

    #[tokio::test]
    async fn test_webhook_router_health_endpoint() {
        let state = WebhookServerState {
            incoming_tx: None,
            slack_signing_secret: Some("test_secret".to_string()),
        };
        let app = create_webhook_router(state);

        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_router_generic_post() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let state = WebhookServerState {
            incoming_tx: Some(tx),
            slack_signing_secret: None,
        };
        let app = create_webhook_router(state);

        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let body_json = serde_json::json!({
            "message": "Alert triggered",
            "target": "telegram:12345",
            "source": "prometheus"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/generic")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body_json).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let received = rx.recv().await.expect("message received in channel");
        assert_eq!(received.platform, "telegram");
        assert_eq!(received.content, "Alert triggered");
        assert_eq!(received.channel_id, "12345");
        assert_eq!(received.username, "prometheus");
    }

    #[tokio::test]
    async fn webhook_router_rejects_malformed_target() {
        let state = WebhookServerState {
            incoming_tx: None,
            slack_signing_secret: None,
        };
        let response = create_webhook_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/generic")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"message":"run","target":"telegram"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn webhook_router_rejects_slack_without_signing_secret() {
        let state = WebhookServerState {
            incoming_tx: None,
            slack_signing_secret: None,
        };
        let response = create_webhook_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/slack")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"type":"url_verification","challenge":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_webhook_router_slack_signature_and_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let secret = "test_signing_secret";
        let state = WebhookServerState {
            incoming_tx: Some(tx),
            slack_signing_secret: Some(secret.to_string()),
        };
        let app = create_webhook_router(state);

        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let body_json = serde_json::json!({
            "type": "event_callback",
            "event": {
                "type": "app_mention",
                "user": "UUSER99",
                "text": "help status",
                "channel": "C999"
            }
        });
        let body_bytes = serde_json::to_vec(&body_json).unwrap();

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let sig_base = format!("v0:{}:", now_secs);
        let mut sig_base_bytes = sig_base.into_bytes();
        sig_base_bytes.extend_from_slice(&body_bytes);
        let computed_hex = hmac_sha256_hex(secret.as_bytes(), &sig_base_bytes);
        let sig_header = format!("v0={}", computed_hex);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook/slack")
                    .header("content-type", "application/json")
                    .header("x-slack-request-timestamp", now_secs.to_string())
                    .header("x-slack-signature", sig_header)
                    .body(Body::from(body_bytes))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let received = rx.recv().await.expect("slack message received in channel");
        assert_eq!(received.platform, "slack");
        assert_eq!(received.content, "help status");
        assert_eq!(received.channel_id, "C999");
        assert_eq!(received.username, "UUSER99");
    }
}
