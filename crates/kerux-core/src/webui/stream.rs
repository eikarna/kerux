//! SSE stream converter, replay buffer, and AgentRunner abstraction.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};

use crate::agent::AgentEvent;
use crate::client::Message;
use crate::error::Result;

pub const MAX_EVENTS_PER_STREAM: usize = 100;
pub const MAX_CHUNK_SIZE_BYTES: usize = 60_000;
pub const MAX_WIRE_FRAME_BYTES: usize = 65_536; // 64 KiB
pub const STREAM_REPLAY_TTL: Duration = Duration::from_secs(300); // 5 minutes
pub const MAX_CONCURRENT_RUNNING_STREAMS: usize = 32;

/// Terminal state codes for the stream lifecycle state machine.
pub const STATE_ACTIVE: u8 = 0;
pub const STATE_COMPLETED: u8 = 1;
pub const STATE_CANCELLED: u8 = 2;
pub const STATE_ERRORED: u8 = 3;
pub const STATE_PANIC_CAUGHT: u8 = 4;

/// A wire frame representing an SSE event name, payload, and monotonic sequence ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseWireFrame {
    pub seq: usize,
    pub event: String,
    pub data: String,
}

/// Pluggable AgentRunner trait allowing live agent execution or mock offline runners.
#[async_trait::async_trait]
pub trait AgentRunner: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        session_id: &str,
        user_message: &str,
        model: Option<&str>,
        history: Vec<Message>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: Arc<AtomicBool>,
        steer_rx: mpsc::Receiver<String>,
    ) -> Result<String>;
}

/// Default LiveAgentRunner that connects to configured LLM providers and executes tools.
pub struct LiveAgentRunner {
    config: Arc<RwLock<crate::config::AppConfig>>,
}

impl LiveAgentRunner {
    pub fn new(config: Arc<RwLock<crate::config::AppConfig>>) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait]
impl AgentRunner for LiveAgentRunner {
    async fn run_turn(
        &self,
        _session_id: &str,
        user_message: &str,
        model_override: Option<&str>,
        history: Vec<Message>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: Arc<AtomicBool>,
        mut steer_rx: mpsc::Receiver<String>,
    ) -> Result<String> {
        let mut cfg = self.config.read().await.clone();
        if let Some(m) = model_override {
            cfg.agent.model = m.to_string();
        }

        let provider = crate::client::build_provider_client(&cfg.client)?;
        let tools =
            crate::tools::ToolRegistry::new(Duration::from_secs(cfg.agent.tool_timeout_secs));

        let agent_config = crate::agent::AgentConfig::from(&cfg.agent);
        let agent = crate::agent::KeruxAgent::with_provider_events(
            agent_config,
            provider,
            tools,
            event_tx.clone(),
        );

        // Populate history
        for msg in history {
            agent.add_message(msg).await;
        }

        // Spawn background task to observe steering messages
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            while let Some(_steer_msg) = steer_rx.recv().await {
                if cancel_clone.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        let response = agent
            .run_with_cancel(user_message.to_string(), cancel)
            .await?;
        Ok(response.content)
    }
}

/// Deterministic mock agent runner for offline unit and integration tests.
pub struct MockAgentRunner {
    pub scripted_events: Vec<AgentEvent>,
    pub scripted_response: String,
    pub barrier: Option<Arc<tokio::sync::Notify>>,
    pub delay: Option<Duration>,
}

impl MockAgentRunner {
    pub fn new(scripted_events: Vec<AgentEvent>, scripted_response: String) -> Self {
        Self {
            scripted_events,
            scripted_response,
            barrier: None,
            delay: None,
        }
    }

    pub fn with_delay(
        scripted_events: Vec<AgentEvent>,
        scripted_response: String,
        delay: Duration,
    ) -> Self {
        Self {
            scripted_events,
            scripted_response,
            barrier: None,
            delay: Some(delay),
        }
    }

    pub fn with_barrier(
        scripted_events: Vec<AgentEvent>,
        scripted_response: String,
        barrier: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            scripted_events,
            scripted_response,
            barrier: Some(barrier),
            delay: None,
        }
    }
}

#[async_trait::async_trait]
impl AgentRunner for MockAgentRunner {
    async fn run_turn(
        &self,
        _session_id: &str,
        _user_message: &str,
        _model: Option<&str>,
        _history: Vec<Message>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: Arc<AtomicBool>,
        mut steer_rx: mpsc::Receiver<String>,
    ) -> Result<String> {
        if let Some(ref barrier) = self.barrier {
            barrier.notified().await;
        }

        if let Some(delay) = self.delay {
            let start = tokio::time::Instant::now();
            while start.elapsed() < delay {
                if cancel.load(Ordering::Relaxed) {
                    return Err(crate::error::Error::Cancelled);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        for event in &self.scripted_events {
            if cancel.load(Ordering::Relaxed) {
                return Err(crate::error::Error::Cancelled);
            }
            let _ = event_tx.send(event.clone()).await;
            tokio::task::yield_now().await;
        }

        while let Ok(_steer) = steer_rx.try_recv() {}

        if cancel.load(Ordering::Relaxed) {
            return Err(crate::error::Error::Cancelled);
        }

        Ok(self.scripted_response.clone())
    }
}

/// Represents an active or recently completed stream.
pub struct ActiveStream {
    pub stream_id: String,
    pub session_id: String,
    pub cancel_flag: Arc<AtomicBool>,
    pub steer_tx: Arc<tokio::sync::Mutex<Option<mpsc::Sender<String>>>>,
    pub state: Arc<AtomicU8>,
    pub created_at: Instant,
    pub completed_at: Arc<RwLock<Option<Instant>>>,
    pub next_seq: Arc<AtomicUsize>,
    pub buffered_events: Arc<RwLock<Vec<SseWireFrame>>>,
}

impl ActiveStream {
    pub fn new(stream_id: String, session_id: String, steer_tx: mpsc::Sender<String>) -> Self {
        Self {
            stream_id,
            session_id,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            steer_tx: Arc::new(tokio::sync::Mutex::new(Some(steer_tx))),
            state: Arc::new(AtomicU8::new(STATE_ACTIVE)),
            created_at: Instant::now(),
            completed_at: Arc::new(RwLock::new(None)),
            next_seq: Arc::new(AtomicUsize::new(1)),
            buffered_events: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn is_running(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STATE_ACTIVE
    }

    pub async fn buffer_raw_frame(&self, event: String, data: String) {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let frame = SseWireFrame { seq, event, data };
        let mut buffer = self.buffered_events.write().await;
        if buffer.len() >= MAX_EVENTS_PER_STREAM {
            buffer.remove(0);
        }
        buffer.push(frame);
    }

    /// Try transition to terminal state atomically. Returns true if this thread won the transition.
    pub fn try_transition_terminal(&self, target_state: u8) -> bool {
        self.state
            .compare_exchange(
                STATE_ACTIVE,
                target_state,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    pub async fn finalize_stream(&self) {
        let mut comp = self.completed_at.write().await;
        if comp.is_none() {
            *comp = Some(Instant::now());
        }
        // Drop steering sender to unblock receiver tasks
        let mut tx_guard = self.steer_tx.lock().await;
        let _ = tx_guard.take();
    }

    pub async fn is_expired(&self) -> bool {
        if self.is_running() {
            return false;
        }
        let comp = self.completed_at.read().await;
        if let Some(t) = *comp {
            t.elapsed() > STREAM_REPLAY_TTL
        } else {
            false
        }
    }
}

/// Evict completed streams exceeding STREAM_REPLAY_TTL from the active streams map.
pub async fn evict_expired_completed_streams(
    streams: &RwLock<HashMap<String, Arc<ActiveStream>>>,
    now_override: Option<Instant>,
) {
    let mut map = streams.write().await;
    map.retain(|_, s| {
        if s.is_running() {
            true
        } else {
            let comp_opt = s.completed_at.try_read();
            if let Ok(guard) = comp_opt {
                if let Some(t) = *guard {
                    let elapsed = if let Some(now) = now_override {
                        now.saturating_duration_since(t)
                    } else {
                        t.elapsed()
                    };
                    elapsed <= STREAM_REPLAY_TTL
                } else {
                    true
                }
            } else {
                true
            }
        }
    });
}

/// Splits text safely along UTF-8 char boundaries and formats into frames <= MAX_WIRE_FRAME_BYTES.
pub fn split_utf8_safe(text: &str, max_raw_bytes: usize) -> Vec<String> {
    if text.len() <= max_raw_bytes {
        return vec![text.to_string()];
    }

    let mut result = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let target_end = (start + max_raw_bytes).min(text.len());
        let mut end = target_end;
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = target_end + 1;
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
        }
        result.push(text[start..end].to_string());
        start = end;
    }
    result
}

fn truncate_string_safe(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [TRUNCATED]", &s[..end])
}

pub fn agent_event_to_unsequenced_frames(event: &AgentEvent) -> Vec<(String, String)> {
    match event {
        AgentEvent::Content { text } => {
            // Target ~50KB raw text chunking to guarantee serialized frame <= 64KB
            let chunks = split_utf8_safe(text, 50_000);
            chunks
                .into_iter()
                .map(|chunk| {
                    (
                        "token".to_string(),
                        serde_json::json!({ "content": chunk }).to_string(),
                    )
                })
                .collect()
        }
        AgentEvent::Reasoning { text } => {
            let chunks = split_utf8_safe(text, 50_000);
            chunks
                .into_iter()
                .map(|chunk| {
                    (
                        "reasoning".to_string(),
                        serde_json::json!({ "content": chunk }).to_string(),
                    )
                })
                .collect()
        }
        AgentEvent::Thinking { content } => {
            let chunks = split_utf8_safe(content, 50_000);
            chunks
                .into_iter()
                .map(|chunk| {
                    (
                        "reasoning".to_string(),
                        serde_json::json!({ "content": chunk }).to_string(),
                    )
                })
                .collect()
        }
        AgentEvent::ToolStart {
            call_id,
            name,
            arguments,
        } => {
            let args_val: serde_json::Value =
                serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!(arguments));
            vec![(
                "toolStarted".to_string(),
                serde_json::json!({
                    "id": call_id,
                    "name": name,
                    "arguments": args_val,
                })
                .to_string(),
            )]
        }
        AgentEvent::ToolComplete { result } => {
            let safe_content = if result.content.len() > 50_000 {
                truncate_string_safe(&result.content, 50_000)
            } else {
                result.content.clone()
            };
            vec![(
                "toolCompleted".to_string(),
                serde_json::json!({
                    "id": result.tool_call_id,
                    "success": result.success,
                    "content": safe_content,
                })
                .to_string(),
            )]
        }
        AgentEvent::ToolError { name, error } => {
            let safe_err = truncate_string_safe(error, 50_000);
            vec![(
                "toolCompleted".to_string(),
                serde_json::json!({
                    "id": name,
                    "success": false,
                    "error": safe_err,
                })
                .to_string(),
            )]
        }
        AgentEvent::Telemetry { telemetry } => {
            vec![(
                "metering".to_string(),
                serde_json::json!({
                    "prompt_tokens": telemetry.prompt_tokens,
                    "completion_tokens": telemetry.completion_tokens,
                    "total_tokens": telemetry.total_tokens,
                })
                .to_string(),
            )]
        }
        AgentEvent::Error { error } => {
            let safe_err = truncate_string_safe(error, 50_000);
            vec![(
                "error".to_string(),
                serde_json::json!({ "error": safe_err }).to_string(),
            )]
        }
        AgentEvent::Done { message } => {
            let safe_msg = truncate_string_safe(&message.content, 50_000);
            vec![(
                "done".to_string(),
                serde_json::json!({ "message": safe_msg }).to_string(),
            )]
        }
        _ => Vec::new(),
    }
}
