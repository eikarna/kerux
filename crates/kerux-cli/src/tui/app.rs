use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use kerux_core::agent::{AgentEvent, KeruxAgent};
use kerux_core::client::Message;
use kerux_core::config::{AppConfig, McpServerConfig, McpTransportKind};
use kerux_core::mcp::McpManager;
use kerux_core::skills::SkillManager;
use kerux_core::tools::{KeruxTool, TerminalTool, ToolContext, ToolResult};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::create_runtime_agent;
use crate::tui::action::Action;
use crate::tui::forms::{Modal, SubmittedMcpForm};
use crate::tui::render;
use crate::tui::state::{
    ActivePanel, AppState, InputMode, McpServerItem, SkillItem, Tone, ViewMode,
};

pub enum LaunchMode {
    Landing,
    Query(String),
}

pub struct TuiApp {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    state: AppState,
    system_prompt: Option<String>,
    agent: Option<Arc<KeruxAgent>>,
    event_tx: mpsc::Sender<AgentEvent>,
    event_rx: mpsc::Receiver<AgentEvent>,
    run_handle: Option<JoinHandle<kerux_core::Result<Message>>>,
    mcp_manager: McpManager,
    skill_manager: SkillManager,
    /// Pre-run repository snapshot enabling the `/undo` command.
    last_snapshot: Option<kerux_core::githarness::RepoSnapshot>,
}

impl TuiApp {
    pub async fn enter(
        config: AppConfig,
        system_prompt: Option<String>,
        launch: LaunchMode,
    ) -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            rollback_terminal_setup();
            return Err(error.into());
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                rollback_terminal_setup();
                return Err(error.into());
            }
        };
        if let Err(error) = terminal.clear() {
            rollback_terminal_setup();
            return Err(error.into());
        }
        let (event_tx, event_rx) = mpsc::channel(config.tools.event_channel_size);
        let prompt = match &launch {
            LaunchMode::Landing => String::new(),
            LaunchMode::Query(query) => query.clone(),
        };
        let mut app = Self {
            terminal,
            state: AppState::new(
                config.clone(),
                prompt,
                matches!(launch, LaunchMode::Query(_)),
            ),
            system_prompt,
            agent: None,
            event_tx,
            event_rx,
            run_handle: None,
            mcp_manager: McpManager::new(),
            skill_manager: SkillManager::new(config.skills.root_dir.clone()),
            last_snapshot: None,
        };
        if let Err(error) = app.refresh_skills() {
            app.record_app_event(
                "Skill load failed",
                format!("Skill load failed: {}", error),
                Tone::Error,
                "skill load failed",
            );
        }
        if let Err(error) = app.refresh_mcp().await {
            app.record_app_event(
                "MCP refresh failed",
                format!("MCP refresh failed: {}", error),
                Tone::Error,
                "mcp refresh failed",
            );
        }
        if matches!(launch, LaunchMode::Query(_)) {
            app.start_run().await;
        }
        Ok(app)
    }

    pub async fn run(mut self) -> Result<()> {
        let run_result = self.run_loop().await;
        let exit_result = self.exit();

        match (run_result, exit_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(run_error), Err(exit_error)) => Err(anyhow::anyhow!(
                "{}; additionally failed to restore terminal: {}",
                run_error,
                exit_error
            )),
        }
    }

    async fn run_loop(&mut self) -> Result<()> {
        loop {
            let size = self.terminal.size()?;
            self.state.set_layout_for_width(size.width);
            self.drain_events();
            self.finish_run_if_ready().await?;

            self.terminal
                .draw(|frame| render::render(frame, &self.state))?;

            if self.state.ui.should_quit {
                break;
            }

            if event::poll(Duration::from_millis(
                self.state.persistent.config.tui.refresh_rate_ms,
            ))? {
                match event::read()? {
                    Event::Key(key) if should_process_key_event(key) => {
                        self.handle_key(key).await?;
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn exit(mut self) -> Result<()> {
        let mouse_result = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        let leave_result = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let raw_result = disable_raw_mode();
        let cursor_result = self.terminal.show_cursor();

        mouse_result?;
        leave_result?;
        raw_result?;
        cursor_result?;
        Ok(())
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.state.reduce(Action::AgentEvent(event));
        }
    }

    async fn finish_run_if_ready(&mut self) -> Result<()> {
        let is_finished = self.run_handle.as_ref().is_some_and(|h| h.is_finished());
        if is_finished {
            if let Some(handle) = self.run_handle.take() {
                let action = match handle.await {
                    Ok(Ok(_)) => Action::ApplyRunResult(None),
                    Ok(Err(error)) => {
                        Action::ApplyRunResult(Some(format!("Run failed: {}", error)))
                    }
                    Err(error) => {
                        Action::ApplyRunResult(Some(format!("Agent task join failed: {}", error)))
                    }
                };
                if matches!(action, Action::ApplyRunResult(None)) {
                    self.auto_commit_if_enabled().await;
                }
                self.state.reduce(action);
            }
        }
        Ok(())
    }

    /// Commit the run's working-tree changes when `[agent].auto_commit` is on.
    /// Best-effort: committing consumes the `/undo` snapshot, so a successful
    /// commit clears rollback state. Failures are surfaced as footer notices.
    ///
    /// Git plumbing shells out and can walk large trees, so it runs on the
    /// blocking pool instead of stalling the UI loop.
    async fn auto_commit_if_enabled(&mut self) {
        if !self.state.persistent.config.agent.auto_commit {
            return;
        }
        let Some(_snapshot) = self.last_snapshot.take() else {
            return;
        };
        let commit = tokio::task::spawn_blocking(|| {
            kerux_core::githarness::GitHarness::open(std::path::Path::new("."))
                .and_then(|harness| harness.commit_transaction("apply agent edits"))
        })
        .await
        .unwrap_or_else(|join_error| {
            Err(kerux_core::Error::Agent(format!(
                "git commit task failed: {join_error}"
            )))
        });
        match commit {
            Ok(Some(hash)) => {
                self.state.set_footer_notice(
                    format!("auto-committed run changes ({})", hash),
                    Tone::Success,
                );
            }
            Ok(None) => {}
            Err(error) => {
                self.state
                    .set_footer_notice(format!("auto-commit failed: {}", error), Tone::Warning);
            }
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if is_global_quit_key(key) {
            self.state.reduce(Action::Quit);
            return Ok(());
        }

        if self.state.ui.modal.is_some() {
            return self.handle_modal_key(key).await;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l') {
            if let Some(agent) = &self.agent {
                agent.clear_history().await;
            }
            self.state.reduce(Action::ClearSession);
            if self.state.persistent.needs_rebuild {
                self.agent = None;
            }
            self.state
                .set_footer_notice("new session ready", Tone::Info);
            return Ok(());
        }

        match self.state.ui.input_mode {
            InputMode::Prompt => self.handle_prompt_key(key).await,
            InputMode::Command => self.handle_command_key(key).await,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if handle_mouse_click_to_prompt(&mut self.state, mouse) {
            return;
        }
        handle_mouse_scroll(&mut self.state, mouse);
    }

    async fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        if handle_prompt_scroll_key(&mut self.state, key) {
            return Ok(());
        }

        match key.code {
            KeyCode::Esc => self.state.reduce(Action::SetInputMode(InputMode::Command)),
            KeyCode::Enter => self.start_run().await,
            KeyCode::Backspace => self.state.reduce(Action::PromptBackspace),
            KeyCode::Up => self.state.prompt_history_previous(),
            KeyCode::Down => self.state.prompt_history_next(),
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.reduce(Action::AppendPrompt(ch));
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_command_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(ch) = landing_prompt_bootstrap_char(&self.state, key) {
            self.state.reduce(Action::SetInputMode(InputMode::Prompt));
            self.state.reduce(Action::AppendPrompt(ch));
            self.state.clear_footer_notice();
            return Ok(());
        }

        if handle_conversation_scroll_key(&mut self.state, key) {
            return Ok(());
        }

        match key.code {
            KeyCode::Char('i') => self.state.reduce(Action::SetInputMode(InputMode::Prompt)),
            KeyCode::Enter if self.state.ui.view == ViewMode::Landing => {
                if landing_enter_starts_prompt(&self.state) {
                    self.state.reduce(Action::SetInputMode(InputMode::Prompt));
                    self.state
                        .set_footer_notice("type prompt, then press Enter to run", Tone::Info);
                } else {
                    self.start_run().await;
                }
            }
            KeyCode::Tab => self.state.reduce(Action::CyclePanelForward),
            KeyCode::BackTab => self.state.reduce(Action::CyclePanelBackward),
            KeyCode::Char('w') => self
                .state
                .reduce(Action::SetActivePanel(ActivePanel::Session)),
            KeyCode::Char('m') => self.state.reduce(Action::SetActivePanel(ActivePanel::Mcp)),
            KeyCode::Char('s') => self
                .state
                .reduce(Action::SetActivePanel(ActivePanel::Skills)),
            KeyCode::Char('b') => self
                .state
                .reduce(Action::SetActivePanel(ActivePanel::Behavior)),
            KeyCode::Char('o') => self.state.reduce(Action::OpenModal(Modal::settings(
                &self.state.persistent.config.tui.theme,
                self.state.persistent.config.tui.rich_output, // we'll use this as a boolean toggle
            ))),

            KeyCode::Up => self.state.reduce(Action::SelectPrevious),
            KeyCode::Down => self.state.reduce(Action::SelectNext),
            KeyCode::Char('a') if self.state.ui.active_panel == ActivePanel::Mcp => {
                self.state.reduce(Action::OpenModal(Modal::add_mcp()));
            }
            KeyCode::Char('n') if self.state.ui.active_panel == ActivePanel::Skills => {
                self.state.reduce(Action::OpenModal(Modal::create_skill()));
            }
            KeyCode::Char('r') if self.state.ui.active_panel == ActivePanel::Skills => {
                match self.refresh_skills() {
                    Ok(()) => self.record_app_event(
                        "Skills reloaded",
                        "Skills reloaded from disk.",
                        Tone::Success,
                        "skills reloaded",
                    ),
                    Err(error) => self.handle_runtime_error("Skill reload failed", error),
                }
            }
            KeyCode::Char('d') if self.state.ui.active_panel == ActivePanel::Mcp => {
                if let Err(error) = self.remove_selected_mcp().await {
                    self.handle_runtime_error("MCP remove failed", error);
                }
            }
            KeyCode::Char('a') if self.state.ui.active_panel == ActivePanel::Skills => {
                if let Err(error) = self.approve_selected_skill() {
                    self.handle_runtime_error("Skill approve failed", error);
                }
            }
            KeyCode::Char('d') if self.state.ui.active_panel == ActivePanel::Skills => {
                if let Err(error) = self.remove_selected_skill() {
                    self.handle_runtime_error("Skill delete failed", error);
                }
            }
            KeyCode::Char('e') if self.state.ui.active_panel == ActivePanel::Behavior => {
                self.open_behavior_editor();
            }
            KeyCode::Char(' ') if self.state.ui.active_panel == ActivePanel::Behavior => {
                self.toggle_selected_behavior();
            }
            KeyCode::Esc => self.state.reduce(Action::SetView(ViewMode::Landing)),
            _ => {}
        }
        Ok(())
    }

    async fn handle_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(mut modal) = self.state.ui.modal.clone() else {
            return Ok(());
        };
        match key.code {
            KeyCode::Esc => {
                self.state.reduce(Action::CloseModal);
                return Ok(());
            }
            KeyCode::Tab => modal.next_field(),
            KeyCode::BackTab => modal.previous_field(),
            KeyCode::Backspace => modal.backspace(),
            KeyCode::Enter => {
                if let Err(error) = self.submit_modal(modal).await {
                    self.handle_runtime_error("Form submit failed", error);
                }
                self.state.reduce(Action::CloseModal);
                return Ok(());
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                modal.push_char(ch)
            }
            _ => {}
        }
        self.state.reduce(Action::UpdateModal(modal));
        Ok(())
    }

    async fn submit_modal(&mut self, modal: Modal) -> Result<()> {
        match modal {
            Modal::AddMcp(form) => {
                let submitted = parse_mcp_form(&form)?;
                let config = McpServerConfig {
                    name: submitted.name.clone(),
                    transport: submitted.transport.clone(),
                    url: submitted.url.clone(),
                    auth_token: submitted.auth_token.clone(),
                    command: submitted.command.clone(),
                    args: submitted.args.clone(),
                    env: submitted.env.clone(),
                    enabled: true,
                };
                self.upsert_mcp_server(config, submitted).await?;
            }
            Modal::CreateSkill(form) => {
                let name = form.fields[0].value.trim();
                let description = form.fields[1].value.trim();
                if name.is_empty() {
                    self.state
                        .set_footer_notice("skill name cannot be empty", Tone::Error);
                } else {
                    let content = default_skill_content(name, description);
                    self.skill_manager.create(name, &content)?;
                    self.refresh_skills()?;
                    self.record_app_event(
                        "Skill created",
                        format!("Created skill '{}'.", name),
                        Tone::Success,
                        "skill created",
                    );
                }
            }

            Modal::Settings(form) => {
                let theme = form.fields[0].value.trim();
                let simple_mode = form.fields[1].value.trim().to_lowercase() == "true";
                self.state.persistent.config.tui.theme = theme.to_string();
                self.state.persistent.config.tui.rich_output = simple_mode;

                self.state.record_app_event(
                    "Settings applied",
                    "Updated TUI settings.",
                    Tone::Success,
                    Some("settings applied".to_string()),
                );
            }
            Modal::EditBehavior(form) => {
                let field = form.fields[0].value.trim().to_string();
                let value = form.fields[1].value.trim().to_string();
                self.apply_behavior_edit(&field, &value)?;
            }
        }
        Ok(())
    }

    async fn start_run(&mut self) {
        if self.state.session.running {
            return;
        }

        let query = self.state.ui.prompt_input.trim().to_string();
        if query.is_empty() {
            self.state
                .set_footer_notice("prompt is empty", Tone::Warning);
            return;
        }

        if query == "/undo" {
            self.state.reduce(Action::ClearPrompt);
            match &self.last_snapshot {
                None => {
                    self.state
                        .set_footer_notice("nothing to undo: no snapshot recorded", Tone::Warning);
                }
                Some(snapshot) => {
                    let snapshot = snapshot.clone();
                    match tokio::task::spawn_blocking(move || {
                        kerux_core::githarness::GitHarness::open(std::path::Path::new("."))
                            .and_then(|harness| harness.undo(&snapshot))
                    })
                    .await
                    {
                        Ok(Ok(())) => {
                            self.last_snapshot = None;
                            self.state
                                .set_footer_notice("undid last run's file changes", Tone::Success);
                        }
                        Ok(Err(error)) => {
                            self.state
                                .set_footer_notice(format!("undo failed: {}", error), Tone::Error);
                        }
                        Err(join_error) => {
                            let error = anyhow::anyhow!("git undo task failed: {join_error}");
                            self.state
                                .set_footer_notice(format!("undo failed: {}", error), Tone::Error);
                        }
                    }
                }
            }
            return;
        }

        if let Some(command) = parse_shell_input(&query) {
            if self.state.ui.pending_shell_command.as_deref() != Some(command.as_str()) {
                self.state.ui.pending_shell_command = Some(command);
                self.state.set_footer_notice(
                    "shell command ready; press Enter again to run",
                    Tone::Warning,
                );
                return;
            }
            self.state.ui.pending_shell_command = None;
            if self.agent.is_none() || self.state.persistent.needs_rebuild {
                if let Err(error) = self.rebuild_agent().await {
                    self.record_app_event(
                        "Agent context sync skipped",
                        format!("Shell will run without agent context sync: {}", error),
                        Tone::Warning,
                        "shell will run without agent context sync",
                    );
                }
            }
            self.start_shell_run(command, self.agent.clone()).await;
            return;
        }
        self.state.ui.pending_shell_command = None;

        if self.state.persistent.needs_rebuild && !self.state.session.transcript.is_empty() {
            let prompt = query.clone();
            self.state.reduce(Action::ClearSession);
            self.state.ui.view = ViewMode::Workspace;
            self.state.ui.prompt_input = prompt;
        }

        if self.agent.is_none() || self.state.persistent.needs_rebuild {
            if let Err(error) = self.rebuild_agent().await {
                self.handle_runtime_error("Agent rebuild failed", error);
                return;
            }
        }

        let Some(agent) = self.agent.clone() else {
            self.handle_runtime_error(
                "Agent start failed",
                anyhow::anyhow!("agent was not available"),
            );
            return;
        };
        // Transactional snapshot: capture pre-run git state so `/undo` can
        // roll back whatever this run changes. Best-effort; outside a repo
        // the snapshot is simply skipped. Git plumbing runs on the blocking
        // pool so a large tree can't stall the UI loop.
        self.last_snapshot = tokio::task::spawn_blocking(|| {
            kerux_core::githarness::GitHarness::open(std::path::Path::new("."))
                .ok()
                .and_then(|harness| harness.snapshot(true).ok())
        })
        .await
        .unwrap_or(None);

        self.state.reduce(Action::StartRun(query.clone()));
        self.state.reduce(Action::ClearPrompt);
        self.run_handle = Some(tokio::spawn(async move { agent.run(query).await }));
    }

    async fn start_shell_run(&mut self, command: String, agent: Option<Arc<KeruxAgent>>) {
        self.state.reduce(Action::StartShellRun(command.clone()));
        self.state.reduce(Action::ClearPrompt);
        let event_tx = self.event_tx.clone();
        self.run_handle = Some(tokio::spawn(async move {
            let _ = event_tx
                .send(AgentEvent::Thinking {
                    content: format!("Running shell command: {}", command),
                })
                .await;

            let result = run_prompt_shell_command(&command).await;
            let content = format_shell_result(&command, &result);
            record_shell_context(agent, &command, &content).await;
            let message = Message::assistant(content.clone());
            let _ = event_tx.send(AgentEvent::Content { text: content }).await;
            let _ = event_tx
                .send(AgentEvent::Done {
                    message: message.clone(),
                })
                .await;
            Ok(message)
        }));
    }

    async fn rebuild_agent(&mut self) -> Result<()> {
        let (event_tx, event_rx) =
            mpsc::channel(self.state.persistent.config.tools.event_channel_size);
        self.event_tx = event_tx.clone();
        self.event_rx = event_rx;
        let agent = create_runtime_agent(
            &self.state.persistent.config,
            &self.state.persistent.behavior,
            self.system_prompt.as_deref(),
            event_tx,
            &mut self.mcp_manager,
        )
        .await?;
        self.agent = Some(Arc::new(agent));
        self.state.persistent.needs_rebuild = false;
        Ok(())
    }

    async fn refresh_mcp(&mut self) -> Result<()> {
        if self.state.persistent.config.mcp.autoload {
            let _ = self.mcp_manager.auto_discover_and_connect().await;
        }

        let mut items = Vec::new();
        let mut processed_names = std::collections::HashSet::new();

        // 1. Configured servers in config.mcp.servers
        for server in &self.state.persistent.config.mcp.servers {
            processed_names.insert(server.name.clone());
            let connected = match self.mcp_manager.get(&server.name) {
                Some(transport) => transport.is_connected().await,
                None => false,
            };
            let tool_count = match self.mcp_manager.get(&server.name) {
                Some(transport) if connected => transport.get_tools().await.len(),
                _ => 0,
            };
            items.push(McpServerItem {
                name: server.name.clone(),
                transport: server.transport.clone(),
                endpoint: server
                    .url
                    .clone()
                    .or_else(|| server.command.clone())
                    .unwrap_or_default(),
                enabled: server.enabled,
                connected,
                tool_count,
            });
        }

        // 2. Active servers from McpManager (including auto-discovered ones) not in config.mcp.servers
        for (name, transport) in self.mcp_manager.servers() {
            if !processed_names.contains(name) {
                let connected = transport.is_connected().await;
                let tool_count = if connected {
                    transport.get_tools().await.len()
                } else {
                    0
                };
                let transport_kind = match transport {
                    kerux_core::mcp::McpTransport::Http(_) => McpTransportKind::Http,
                    kerux_core::mcp::McpTransport::Stdio(_) => McpTransportKind::Stdio,
                };
                items.push(McpServerItem {
                    name: name.clone(),
                    transport: transport_kind,
                    endpoint: "auto-discovered".to_string(),
                    enabled: true,
                    connected,
                    tool_count,
                });
            }
        }

        self.state.reduce(Action::SyncMcp(items));
        Ok(())
    }

    fn refresh_skills(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.state.persistent.config.skills.root_dir)?;
        let loaded = self.skill_manager.load_all()?;
        let mut items = loaded
            .iter()
            .map(|skill| SkillItem {
                name: skill.name.clone(),
                description: skill.description.clone(),
                version: skill.version.clone(),
                available: self.skill_manager.is_available(skill),
                pending: false,
            })
            .collect::<Vec<_>>();
        for skill in self.skill_manager.pending_skills() {
            items.push(SkillItem {
                name: skill.name,
                description: skill.description,
                version: skill.version,
                available: false,
                pending: true,
            });
        }
        self.state.reduce(Action::SyncSkills(items));
        Ok(())
    }

    fn handle_runtime_error(&mut self, prefix: &str, error: anyhow::Error) {
        self.state.reduce(Action::RuntimeError {
            prefix: prefix.to_string(),
            error: error.to_string(),
        });
    }

    async fn upsert_mcp_server(
        &mut self,
        config: McpServerConfig,
        submitted: SubmittedMcpForm,
    ) -> Result<()> {
        match submitted.transport {
            McpTransportKind::Http => {
                let url = submitted
                    .url
                    .clone()
                    .context("MCP HTTP server requires a URL.")?;
                self.mcp_manager
                    .add_server(config.name.clone(), url, submitted.auth_token.clone())
                    .await?;
            }
            McpTransportKind::Stdio => {
                let command = submitted
                    .command
                    .clone()
                    .context("MCP stdio server requires a command.")?;
                self.mcp_manager
                    .add_stdio_server(
                        config.name.clone(),
                        command,
                        submitted.args.clone(),
                        submitted.env.clone(),
                    )
                    .await?;
            }
        }

        if let Some(existing) = self
            .state
            .persistent
            .config
            .mcp
            .servers
            .iter_mut()
            .find(|server| server.name == config.name)
        {
            *existing = config.clone();
        } else {
            self.state.persistent.config.mcp.servers.push(config);
        }
        self.state.persistent.needs_rebuild = true;
        self.refresh_mcp().await?;
        self.record_app_event(
            "MCP server saved",
            "MCP server saved. Press ctrl+l for a fresh session with updated tools.",
            Tone::Success,
            "mcp server saved",
        );
        Ok(())
    }

    async fn remove_selected_mcp(&mut self) -> Result<()> {
        if let Some(server) = self
            .state
            .persistent
            .mcp_servers
            .get(self.state.ui.selected_mcp)
            .cloned()
        {
            self.mcp_manager.remove_server(&server.name).await?;
            self.state
                .persistent
                .config
                .mcp
                .servers
                .retain(|item| item.name != server.name);
            self.state.persistent.needs_rebuild = true;
            self.refresh_mcp().await?;
            self.record_app_event(
                "MCP server removed",
                format!("Removed MCP server '{}'.", server.name),
                Tone::Warning,
                "mcp server removed",
            );
        }
        Ok(())
    }

    fn remove_selected_skill(&mut self) -> Result<()> {
        if let Some(skill) = self
            .state
            .persistent
            .skills
            .get(self.state.ui.selected_skill)
            .cloned()
        {
            if skill.pending {
                // Pending skills are discarded (removed from `_pending/`), not
                // archived — they were never loadable.
                if self.skill_manager.discard_pending(&skill.name)? {
                    self.refresh_skills()?;
                    self.record_app_event(
                        "Pending skill discarded",
                        format!("Discarded pending skill '{}'.", skill.name),
                        Tone::Warning,
                        "pending skill discarded",
                    );
                }
                return Ok(());
            }
            self.skill_manager.delete(&skill.name)?;
            self.refresh_skills()?;
            self.record_app_event(
                "Skill deleted",
                format!("Deleted skill '{}'.", skill.name),
                Tone::Warning,
                "skill deleted",
            );
        }
        Ok(())
    }

    /// Approve the selected pending skill: moves it from `_pending/` into the
    /// loadable set, then refreshes. No-op for already-live skills.
    fn approve_selected_skill(&mut self) -> Result<()> {
        if let Some(skill) = self
            .state
            .persistent
            .skills
            .get(self.state.ui.selected_skill)
            .cloned()
        {
            if !skill.pending {
                self.state
                    .set_footer_notice("skill is already approved", Tone::Info);
                return Ok(());
            }
            if self.skill_manager.approve(&skill.name)? {
                self.refresh_skills()?;
                self.record_app_event(
                    "Skill approved",
                    format!("Approved distilled skill '{}'.", skill.name),
                    Tone::Success,
                    "skill approved",
                );
            }
        }
        Ok(())
    }

    fn open_behavior_editor(&mut self) {
        let rows = self.state.behavior_rows();
        if let Some((field, value)) = rows.get(self.state.ui.selected_behavior) {
            self.state
                .reduce(Action::OpenModal(Modal::edit_behavior(field, value)));
        }
    }

    fn toggle_selected_behavior(&mut self) {
        let rows = self.state.behavior_rows();
        if let Some((field, value)) = rows.get(self.state.ui.selected_behavior) {
            if value == "true" || value == "false" {
                let next = if value == "true" { "false" } else { "true" };
                if self.apply_behavior_edit(field, next).is_ok() {
                    self.state
                        .set_footer_notice(format!("updated {}", field), Tone::Success);
                }
            }
        }
    }

    fn apply_behavior_edit(&mut self, field: &str, value: &str) -> Result<()> {
        let behavior = &mut self.state.persistent.behavior;
        match field {
            "model" => behavior.model = value.to_string(),
            "system_prompt" => {
                behavior.system_prompt = if value.is_empty() || value == "(default)" {
                    None
                } else {
                    Some(value.to_string())
                }
            }
            "max_iterations" => behavior.max_iterations = value.parse()?,
            "tool_timeout_secs" => behavior.tool_timeout_secs = value.parse()?,
            "request_timeout_secs" => behavior.request_timeout_secs = value.parse()?,
            "context_window" => behavior.context_window = value.parse()?,
            "stream" => behavior.stream = parse_bool(value)?,
            "show_reasoning" => behavior.show_reasoning = parse_bool(value)?,
            "max_healing_attempts" => behavior.max_healing_attempts = value.parse()?,
            _ => {}
        }

        self.state.persistent.config.agent = behavior.clone();
        self.state.session.max_iterations = behavior.max_iterations;
        self.state.persistent.needs_rebuild = true;
        self.record_app_event(
            "Behavior updated",
            format!(
                "Updated {}. Press ctrl+l to start a fresh session with new behavior.",
                field
            ),
            Tone::Success,
            format!("updated {}", field).as_str(),
        );
        Ok(())
    }

    fn record_app_event(&mut self, label: &str, body: impl Into<String>, tone: Tone, notice: &str) {
        self.state
            .record_app_event(label, body, tone, Some(notice.to_string()));
    }
}

impl Drop for TuiApp {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn parse_mcp_form(form: &crate::tui::forms::FormState) -> Result<SubmittedMcpForm> {
    let transport = match form.fields[0].value.trim().to_ascii_lowercase().as_str() {
        "http" => McpTransportKind::Http,
        "stdio" => McpTransportKind::Stdio,
        other => anyhow::bail!("Unknown MCP transport '{}'. Use http or stdio.", other),
    };
    let name = form.fields[1].value.trim().to_string();
    if name.is_empty() {
        anyhow::bail!("MCP server name cannot be empty.");
    }

    Ok(SubmittedMcpForm {
        transport,
        name,
        url: non_empty(form.fields[2].value.trim()),
        auth_token: non_empty(form.fields[3].value.trim()),
        command: non_empty(form.fields[4].value.trim()),
        args: split_csv(&form.fields[5].value),
        env: parse_env_pairs(&form.fields[6].value)?,
    })
}

fn default_skill_content(name: &str, description: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\nversion: 0.1.0\n---\n# {name}\n\nDescribe the workflow here.\n"
    )
}

fn parse_env_pairs(value: &str) -> Result<HashMap<String, String>> {
    let mut pairs = HashMap::new();
    for item in split_csv(value) {
        let Some((key, value)) = item.split_once('=') else {
            anyhow::bail!("Invalid env entry '{}'. Use KEY=VALUE.", item);
        };
        pairs.insert(key.trim().to_string(), value.trim().to_string());
    }
    Ok(pairs)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_shell_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if let Some(command) = trimmed.strip_prefix('!') {
        let command = command.trim();
        return (!command.is_empty()).then(|| command.to_string());
    }

    if let Some(command) = trimmed.strip_prefix("$ ") {
        let command = command.trim();
        return (!command.is_empty()).then(|| command.to_string());
    }

    None
}

async fn run_prompt_shell_command(command: &str) -> ToolResult {
    TerminalTool
        .execute(
            serde_json::json!({
                "command": command,
                "useShell": true,
            }),
            ToolContext::default(),
        )
        .await
}

async fn record_shell_context(agent: Option<Arc<KeruxAgent>>, command: &str, output: &str) {
    if let Some(agent) = agent {
        agent
            .add_message(Message::user(format!("Shell command:\n{command}")))
            .await;
        agent
            .add_message(Message::assistant(format!("Shell output:\n{output}")))
            .await;
    }
}

fn format_shell_result(command: &str, result: &ToolResult) -> String {
    let value: serde_json::Value = serde_json::from_str(&result.content).unwrap_or_default();
    if !result.success {
        return format!(
            "$ {command}\n\n{}",
            result.error.as_deref().unwrap_or("Command failed")
        );
    }

    let exit_code = value
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let stdout = value
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim_end();
    let stderr = value
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim_end();

    let mut output = format!("$ {command}\n\nexit code: {exit_code}");
    if !stdout.is_empty() {
        output.push_str("\n\nstdout:\n");
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        output.push_str("\n\nstderr:\n");
        output.push_str(stderr);
    }
    output
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("Expected a boolean value."),
    }
}

fn is_global_quit_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q')
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
}

fn landing_enter_starts_prompt(state: &AppState) -> bool {
    state.ui.view == ViewMode::Landing && state.ui.prompt_input.trim().is_empty()
}

fn landing_prompt_bootstrap_char(state: &AppState, key: KeyEvent) -> Option<char> {
    if state.ui.view != ViewMode::Landing {
        return None;
    }

    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }

    match key.code {
        KeyCode::Char(ch) => Some(ch),
        _ => None,
    }
}

fn should_process_key_event(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn rollback_terminal_setup() {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

const MOUSE_SCROLL_LINES: u16 = 3;

fn handle_mouse_click_to_prompt(state: &mut AppState, mouse: MouseEvent) -> bool {
    if state.ui.modal.is_some() {
        return false;
    }

    if matches!(
        mouse.kind,
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
    ) {
        // Any tap in the bottom rows / input footer or landing prompt triggers Prompt mode
        state.reduce(Action::SetInputMode(InputMode::Prompt));
        state.clear_footer_notice();
        return true;
    }

    false
}

fn handle_mouse_scroll(state: &mut AppState, mouse: MouseEvent) -> bool {
    if state.ui.modal.is_some() {
        return false;
    }

    if state.ui.view != ViewMode::Workspace || state.ui.active_panel != ActivePanel::Session {
        return false;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => state.scroll_conversation_up(MOUSE_SCROLL_LINES),
        MouseEventKind::ScrollDown => state.scroll_conversation_down(MOUSE_SCROLL_LINES),
        _ => return false,
    }

    true
}

fn handle_conversation_scroll_key(state: &mut AppState, key: KeyEvent) -> bool {
    if state.ui.view != ViewMode::Workspace || state.ui.active_panel != ActivePanel::Session {
        return false;
    }

    match key.code {
        KeyCode::Up => state.scroll_conversation_up(1),
        KeyCode::Down => state.scroll_conversation_down(1),
        KeyCode::PageUp => state.scroll_conversation_up(8),
        KeyCode::PageDown => state.scroll_conversation_down(8),
        KeyCode::Home => state.scroll_conversation_to_top(),
        KeyCode::End => state.scroll_conversation_down(u16::MAX),
        _ => return false,
    }

    true
}

fn handle_prompt_scroll_key(state: &mut AppState, key: KeyEvent) -> bool {
    if state.ui.view != ViewMode::Workspace || state.ui.active_panel != ActivePanel::Session {
        return false;
    }

    match key.code {
        KeyCode::PageUp => state.scroll_conversation_up(8),
        KeyCode::PageDown => state.scroll_conversation_down(8),
        KeyCode::Home => state.scroll_conversation_to_top(),
        KeyCode::End => state.scroll_conversation_down(u16::MAX),
        _ => return false,
    }

    true
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use kerux_core::config::AppConfig;
    use tokio::task;

    use super::*;

    #[test]
    fn quit_keys_are_global() {
        assert!(is_global_quit_key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        )));
        assert!(is_global_quit_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_global_quit_key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn landing_enter_switches_to_prompt_only_when_empty() {
        let empty = AppState::new(AppConfig::default(), String::new(), false);
        assert!(landing_enter_starts_prompt(&empty));

        let filled = AppState::new(AppConfig::default(), "hello".to_string(), false);
        assert!(!landing_enter_starts_prompt(&filled));
    }

    #[test]
    fn landing_typing_bootstraps_prompt_input() {
        let state = AppState::new(AppConfig::default(), String::new(), false);
        let key = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert_eq!(landing_prompt_bootstrap_char(&state, key), Some('a'));
    }

    #[test]
    fn shell_input_uses_explicit_prefixes() {
        assert_eq!(
            parse_shell_input("!cargo test"),
            Some("cargo test".to_string())
        );
        assert_eq!(
            parse_shell_input("$ cargo test"),
            Some("cargo test".to_string())
        );
        assert_eq!(parse_shell_input("!   "), None);
        assert_eq!(parse_shell_input("what is $PATH"), None);
    }

    #[tokio::test]
    async fn shell_context_is_added_to_agent_history() {
        let agent = Arc::new(KeruxAgent::new(
            kerux_core::agent::AgentConfig::default(),
            kerux_core::client::OpenAIClient::new(kerux_core::client::ClientConfig::default()),
            kerux_core::tools::ToolRegistry::new(Duration::from_secs(1)),
        ));

        record_shell_context(Some(agent.clone()), "echo hello", "stdout: hello").await;

        let conversation = agent.conversation().await;
        assert_eq!(conversation.len(), 2);
        assert!(conversation[0].content.contains("Shell command"));
        assert!(conversation[1].content.contains("Shell output"));
    }

    #[test]
    fn workspace_typing_does_not_bootstrap_prompt_input() {
        let state = AppState::new(AppConfig::default(), String::new(), true);
        let key = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert_eq!(landing_prompt_bootstrap_char(&state, key), None);
    }

    #[test]
    fn ignores_key_release_events() {
        let release = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };
        let press = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert!(!should_process_key_event(release));
        assert!(should_process_key_event(press));
    }

    #[test]
    fn session_panel_scroll_keys_adjust_conversation_offset() {
        let mut state = AppState::new(AppConfig::default(), String::new(), true);
        state.ui.view = ViewMode::Workspace;
        state.ui.active_panel = ActivePanel::Session;

        assert!(handle_conversation_scroll_key(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
        ));
        assert_eq!(state.conversation_scroll(), 1);

        assert!(handle_conversation_scroll_key(
            &mut state,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
        ));
        assert_eq!(state.conversation_scroll(), 9);

        assert!(handle_conversation_scroll_key(
            &mut state,
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)
        ));
        assert_eq!(state.conversation_scroll(), 0);
    }

    #[test]
    fn prompt_mode_keeps_page_scroll_for_session() {
        let mut state = AppState::new(AppConfig::default(), String::new(), true);
        state.ui.view = ViewMode::Workspace;
        state.ui.active_panel = ActivePanel::Session;
        state.ui.input_mode = InputMode::Prompt;

        assert!(handle_prompt_scroll_key(
            &mut state,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)
        ));
        assert_eq!(state.conversation_scroll(), 8);
    }

    #[test]
    fn mouse_wheel_scrolls_session_conversation() {
        let mut state = AppState::new(AppConfig::default(), "draft".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.ui.active_panel = ActivePanel::Session;
        state.ui.input_mode = InputMode::Prompt;

        assert!(handle_mouse_scroll(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }
        ));

        assert_eq!(state.conversation_scroll(), MOUSE_SCROLL_LINES);
        assert_eq!(state.ui.prompt_input, "draft");

        assert!(handle_mouse_scroll(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }
        ));
        assert_eq!(state.conversation_scroll(), 0);
    }

    #[test]
    fn mouse_wheel_ignores_non_session_views() {
        let mut state = AppState::new(AppConfig::default(), String::new(), false);
        state.ui.view = ViewMode::Landing;
        state.ui.active_panel = ActivePanel::Mcp;

        assert!(!handle_mouse_scroll(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }
        ));
        assert_eq!(state.conversation_scroll(), 0);
    }

    #[test]
    fn mouse_wheel_does_not_scroll_behind_modal() {
        let mut state = AppState::new(AppConfig::default(), String::new(), true);
        state.ui.view = ViewMode::Workspace;
        state.ui.active_panel = ActivePanel::Session;
        state.ui.modal = Some(Modal::settings("default", true));

        assert!(!handle_mouse_scroll(
            &mut state,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            }
        ));
        assert_eq!(state.conversation_scroll(), 0);
    }

    #[tokio::test]
    async fn join_errors_stay_inside_tui_state() {
        let mut state = AppState::new(AppConfig::default(), String::new(), false);
        state.begin_run("hello".to_string());
        let run_handle: tokio::task::JoinHandle<kerux_core::Result<Message>> =
            task::spawn(async move {
                panic!("boom");
                #[allow(unreachable_code)]
                Ok(kerux_core::client::Message::assistant("never"))
            });
        tokio::task::yield_now().await;

        let action = match run_handle.await {
            Ok(Ok(_)) => Action::ApplyRunResult(None),
            Ok(Err(error)) => Action::ApplyRunResult(Some(format!("Run failed: {}", error))),
            Err(error) => {
                Action::ApplyRunResult(Some(format!("Agent task join failed: {}", error)))
            }
        };
        state.reduce(action);

        assert_eq!(state.session.status, "Run failed");
        assert!(state
            .session
            .activity
            .iter()
            .any(|item| item.label == "Run failed" || item.label == "Agent task join failed"));
    }
}
