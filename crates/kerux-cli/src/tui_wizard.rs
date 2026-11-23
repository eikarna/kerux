//! Full-screen interactive onboarding wizard using Ratatui.
//!
//! Provides a modern terminal UI with step progress indicator,
//! styled ASCII branding, provider selection cards, inline credential input,
//! live model discovery and capability badge rendering, optional capability
//! probe execution with TTFT tracking, fallback provider configuration,
//! and smoke test verification.

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use kerux_core::client::{LLMProvider, ModelInfo, ProviderKind};
use kerux_core::probe::{probe_model, ProbeResult};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph};
use ratatui::{Frame, Terminal};

use crate::tui::theme::{ACCENT, BG, ERROR, HELP, MUTED, PANEL, PANEL_ALT, SUCCESS, TEXT, WARN};
use crate::wizard::{
    build_plan_provider, default_base_url, detect_env_provider, format_context_window,
    format_smoke, mask_secret, probe_summary_lines, smoke_test, wizard_config_path,
    write_wizard_config, FallbackChoice, SmokeOutcome, WizardPlan,
};

const DISCOVER_TIMEOUT: Duration = Duration::from_secs(45);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

const ASCII_LOGO: &[&str] = &[
    r" _  _______ ____  _   _ __  __ ",
    r"| |/ / ____|  _ \| | | |\ \/ / ",
    r"| ' /|  _| | |_) | | | | \  /  ",
    r"| . \| |___|  _ <| |_| | /  \  ",
    r"|_|\_\_____|_| \_\\___/ /_/\_\ ",
];

const WIZARD_STEPS: &[(&str, &str)] = &[
    ("1", "Provider"),
    ("2", "Endpoint & Key"),
    ("3", "Model Selection"),
    ("4", "Capability Probe"),
    ("5", "Fallback Routing"),
    ("6", "Tools & Web Search"),
    ("7", "Budget Guardrails"),
    ("8", "Save & Smoke"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardPhase {
    Provider,
    EndpointAndKey,
    Model,
    Probe,
    Fallback,
    ToolsAndSearch,
    BudgetGuardrails,
    WriteAndSmoke,
    Finished,
}

#[derive(Debug, Clone)]
pub struct TuiWizardState {
    pub phase: WizardPhase,
    pub step_index: usize,

    // Step 1: Provider
    pub provider_choices: Vec<(&'static str, ProviderKind)>,
    pub selected_provider_idx: usize,

    // Step 2: Endpoint & Key
    pub endpoint_input: String,
    pub api_key_input: String,
    pub active_field: usize, // 0 = endpoint, 1 = api_key
    pub key_source_note: Option<String>,

    // Step 3: Model Selection
    pub is_loading_models: bool,
    pub model_list: Vec<ModelInfo>,
    pub model_filter: String,
    pub filtered_model_indices: Vec<usize>,
    pub selected_model_list_idx: usize,
    pub manual_model_input: String,
    pub is_manual_model_mode: bool,
    pub load_error: Option<String>,
    pub chosen_model: Option<String>,

    // Step 4: Capability Probe
    pub probe_skipped: bool,
    pub is_probing: bool,
    pub probe_result: Option<ProbeResult>,
    pub probe_error: Option<String>,

    // Step 5: Fallback
    pub want_fallback: bool,
    pub fallback_provider_idx: usize,
    pub fallback_endpoint: String,
    pub fallback_key: String,
    pub fallback_model: String,
    pub fallback_field: usize, // 0 = want (toggle), 1 = provider, 2 = endpoint, 3 = key, 4 = model
    pub fallback_choice: Option<FallbackChoice>,

    // Step 6: Tools & Search
    pub tool_presets: Vec<(&'static str, &'static str)>,
    pub selected_preset_idx: usize,
    pub web_search_url_input: String,
    pub tool_field: usize, // 0 = preset, 1 = search_url

    // Step 7: Budget Guardrails
    pub budget_enabled: bool,
    pub daily_limit_input: String,
    pub budget_field: usize, // 0 = enabled toggle, 1 = daily limit

    // Step 8: Write & Smoke
    pub config_target: PathBuf,
    pub config_written: bool,
    pub is_running_smoke: bool,
    pub smoke_outcome: Option<Result<SmokeOutcome, String>>,
    pub status_message: Option<(String, Color)>,
}

impl Default for TuiWizardState {
    fn default() -> Self {
        let provider_choices = vec![
            ("OpenAI / Compatible (custom URL)", ProviderKind::Openai),
            ("Anthropic Claude", ProviderKind::Anthropic),
            ("Google Gemini", ProviderKind::Gemini),
            ("Ollama (Local)", ProviderKind::Ollama),
            ("OpenRouter", ProviderKind::Openrouter),
        ];

        let target = wizard_config_path();

        let mut state = Self {
            phase: WizardPhase::Provider,
            step_index: 0,
            provider_choices,
            selected_provider_idx: 0,
            endpoint_input: String::new(),
            api_key_input: String::new(),
            active_field: 0,
            key_source_note: None,
            is_loading_models: false,
            model_list: Vec::new(),
            model_filter: String::new(),
            filtered_model_indices: Vec::new(),
            selected_model_list_idx: 0,
            manual_model_input: String::new(),
            is_manual_model_mode: false,
            load_error: None,
            chosen_model: None,
            probe_skipped: false,
            is_probing: false,
            probe_result: None,
            probe_error: None,
            want_fallback: false,
            fallback_provider_idx: 4, // OpenRouter default fallback
            fallback_endpoint: "https://openrouter.ai/api/v1".to_string(),
            fallback_key: String::new(),
            fallback_model: String::new(),
            fallback_field: 0,
            fallback_choice: None,
            tool_presets: vec![
                (
                    "full",
                    "Full Developer Agent (Terminal, Files, Web Search, Code Exec, Memory)",
                ),
                (
                    "coding",
                    "Coding-First (Terminal, Files, Code Exec, Memory)",
                ),
                (
                    "safe",
                    "Read-Only / Safe Mode (Files, Search, Memory - No Shell Exec)",
                ),
            ],
            selected_preset_idx: 0,
            web_search_url_input: "https://lite.duckduckgo.com/lite/?q={query}".to_string(),
            tool_field: 0,
            budget_enabled: false,
            daily_limit_input: "5.0".to_string(),
            budget_field: 0,
            config_target: target,
            config_written: false,
            is_running_smoke: false,
            smoke_outcome: None,
            status_message: None,
        };

        if let Some(detected) = detect_env_provider() {
            if let Some(pos) = state
                .provider_choices
                .iter()
                .position(|(_, k)| *k == detected.kind)
            {
                state.selected_provider_idx = pos;
            }
            state.endpoint_input = if detected.is_host {
                normalize_host(&detected.value)
            } else {
                default_base_url(detected.kind).to_string()
            };
            if !detected.is_host {
                state.api_key_input = detected.value.clone();
                state.key_source_note = Some(format!("Detected from env ({})", detected.env_var));
            } else {
                state.key_source_note = Some(format!("Host from env ({})", detected.env_var));
            }
        } else {
            state.endpoint_input = default_base_url(ProviderKind::Openai).to_string();
        }

        state
    }
}

fn normalize_host(host: &str) -> String {
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

pub struct FullscreenWizard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    pub state: TuiWizardState,
}

impl FullscreenWizard {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;

        Ok(Self {
            terminal,
            state: TuiWizardState::default(),
        })
    }

    pub fn exit(mut self) -> Result<()> {
        let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        Ok(())
    }

    pub async fn run(&mut self) -> Result<bool> {
        loop {
            self.terminal.draw(|f| render_wizard_ui(f, &self.state))?;

            if self.state.phase == WizardPhase::Finished {
                return Ok(self.state.config_written);
            }

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if (key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat)
                        && self.handle_key(key).await?
                    {
                        return Ok(self.state.config_written);
                    }
                }
            }
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true); // exit without saving
        }

        match self.state.phase {
            WizardPhase::Provider => match key.code {
                KeyCode::Esc => return Ok(true),
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.state.selected_provider_idx > 0 {
                        self.state.selected_provider_idx -= 1;
                        self.sync_provider_endpoint();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.state.selected_provider_idx + 1 < self.state.provider_choices.len() {
                        self.state.selected_provider_idx += 1;
                        self.sync_provider_endpoint();
                    }
                }
                KeyCode::Enter => {
                    self.state.phase = WizardPhase::EndpointAndKey;
                    self.state.step_index = 1;
                    self.state.active_field = 0;
                }
                _ => {}
            },

            WizardPhase::EndpointAndKey => match key.code {
                KeyCode::Esc => {
                    self.state.phase = WizardPhase::Provider;
                    self.state.step_index = 0;
                }
                KeyCode::Tab | KeyCode::Down => {
                    self.state.active_field = (self.state.active_field + 1) % 2;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.state.active_field = if self.state.active_field == 0 { 1 } else { 0 };
                }
                KeyCode::Backspace => {
                    if self.state.active_field == 0 {
                        self.state.endpoint_input.pop();
                    } else {
                        self.state.api_key_input.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if self.state.active_field == 0 {
                        self.state.endpoint_input.push(c);
                    } else {
                        self.state.api_key_input.push(c);
                    }
                }
                KeyCode::Enter => {
                    self.state.phase = WizardPhase::Model;
                    self.state.step_index = 2;
                    self.start_model_fetch().await?;
                }
                _ => {}
            },

            WizardPhase::Model => match key.code {
                KeyCode::Esc => {
                    if self.state.is_manual_model_mode {
                        self.state.is_manual_model_mode = false;
                    } else {
                        self.state.phase = WizardPhase::EndpointAndKey;
                        self.state.step_index = 1;
                    }
                }
                KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.state.is_manual_model_mode = !self.state.is_manual_model_mode;
                }
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.start_model_fetch().await?;
                }
                KeyCode::Up => {
                    if !self.state.is_manual_model_mode && self.state.selected_model_list_idx > 0 {
                        self.state.selected_model_list_idx -= 1;
                    }
                }
                KeyCode::Down => {
                    if !self.state.is_manual_model_mode
                        && self.state.selected_model_list_idx + 1
                            < self.state.filtered_model_indices.len()
                    {
                        self.state.selected_model_list_idx += 1;
                    }
                }
                KeyCode::Backspace => {
                    if self.state.is_manual_model_mode {
                        self.state.manual_model_input.pop();
                    } else {
                        self.state.model_filter.pop();
                        self.update_filtered_models();
                    }
                }
                KeyCode::Char(c) => {
                    if self.state.is_manual_model_mode {
                        self.state.manual_model_input.push(c);
                    } else {
                        self.state.model_filter.push(c);
                        self.update_filtered_models();
                    }
                }
                KeyCode::Enter => {
                    let chosen = if self.state.is_manual_model_mode {
                        if self.state.manual_model_input.trim().is_empty() {
                            None
                        } else {
                            Some(self.state.manual_model_input.trim().to_string())
                        }
                    } else if !self.state.filtered_model_indices.is_empty() {
                        let idx =
                            self.state.filtered_model_indices[self.state.selected_model_list_idx];
                        Some(self.state.model_list[idx].id.clone())
                    } else if !self.state.model_filter.trim().is_empty() {
                        Some(self.state.model_filter.trim().to_string())
                    } else {
                        None
                    };

                    if let Some(model) = chosen {
                        self.state.chosen_model = Some(model);
                        self.state.phase = WizardPhase::Probe;
                        self.state.step_index = 3;
                    }
                }
                _ => {}
            },

            WizardPhase::Probe => match key.code {
                KeyCode::Esc => {
                    self.state.phase = WizardPhase::Model;
                    self.state.step_index = 2;
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    self.state.probe_skipped = true;
                    self.state.phase = WizardPhase::Fallback;
                    self.state.step_index = 4;
                }
                KeyCode::Enter | KeyCode::Char('p') => {
                    self.run_capability_probe().await?;
                    self.state.phase = WizardPhase::Fallback;
                    self.state.step_index = 4;
                }
                _ => {}
            },

            WizardPhase::Fallback => match key.code {
                KeyCode::Esc => {
                    self.state.phase = WizardPhase::Probe;
                    self.state.step_index = 3;
                }
                KeyCode::Tab | KeyCode::Down => {
                    self.state.fallback_field = (self.state.fallback_field + 1) % 5;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.state.fallback_field = if self.state.fallback_field == 0 {
                        4
                    } else {
                        self.state.fallback_field - 1
                    };
                }
                KeyCode::Char(' ') if self.state.fallback_field == 0 => {
                    self.state.want_fallback = !self.state.want_fallback;
                }
                KeyCode::Left if self.state.fallback_field == 1 => {
                    if self.state.fallback_provider_idx > 0 {
                        self.state.fallback_provider_idx -= 1;
                        let kind = self.state.provider_choices[self.state.fallback_provider_idx].1;
                        self.state.fallback_endpoint = default_base_url(kind).to_string();
                    }
                }
                KeyCode::Right if self.state.fallback_field == 1 => {
                    if self.state.fallback_provider_idx + 1 < self.state.provider_choices.len() {
                        self.state.fallback_provider_idx += 1;
                        let kind = self.state.provider_choices[self.state.fallback_provider_idx].1;
                        self.state.fallback_endpoint = default_base_url(kind).to_string();
                    }
                }
                KeyCode::Backspace => match self.state.fallback_field {
                    2 => {
                        self.state.fallback_endpoint.pop();
                    }
                    3 => {
                        self.state.fallback_key.pop();
                    }
                    4 => {
                        self.state.fallback_model.pop();
                    }
                    _ => {}
                },
                KeyCode::Char(c) => match self.state.fallback_field {
                    2 => self.state.fallback_endpoint.push(c),
                    3 => self.state.fallback_key.push(c),
                    4 => self.state.fallback_model.push(c),
                    _ => {}
                },
                KeyCode::Enter => {
                    if self.state.want_fallback && !self.state.fallback_model.trim().is_empty() {
                        let kind = self.state.provider_choices[self.state.fallback_provider_idx].1;
                        self.state.fallback_choice = Some(FallbackChoice {
                            provider: kind,
                            base_url: self.state.fallback_endpoint.clone(),
                            api_key: if self.state.fallback_key.trim().is_empty() {
                                None
                            } else {
                                Some(self.state.fallback_key.trim().to_string())
                            },
                            model: self.state.fallback_model.trim().to_string(),
                        });
                    } else {
                        self.state.fallback_choice = None;
                    }
                    self.state.phase = WizardPhase::ToolsAndSearch;
                    self.state.step_index = 5;
                }
                _ => {}
            },

            WizardPhase::ToolsAndSearch => match key.code {
                KeyCode::Esc => {
                    self.state.phase = WizardPhase::Fallback;
                    self.state.step_index = 4;
                }
                KeyCode::Tab | KeyCode::Down => {
                    self.state.tool_field = (self.state.tool_field + 1) % 2;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.state.tool_field = if self.state.tool_field == 0 { 1 } else { 0 };
                }
                KeyCode::Left if self.state.tool_field == 0 => {
                    if self.state.selected_preset_idx > 0 {
                        self.state.selected_preset_idx -= 1;
                    }
                }
                KeyCode::Right if self.state.tool_field == 0 => {
                    if self.state.selected_preset_idx + 1 < self.state.tool_presets.len() {
                        self.state.selected_preset_idx += 1;
                    }
                }
                KeyCode::Backspace if self.state.tool_field == 1 => {
                    self.state.web_search_url_input.pop();
                }
                KeyCode::Char(c) if self.state.tool_field == 1 => {
                    self.state.web_search_url_input.push(c);
                }
                KeyCode::Enter => {
                    self.state.phase = WizardPhase::BudgetGuardrails;
                    self.state.step_index = 6;
                }
                _ => {}
            },

            WizardPhase::BudgetGuardrails => match key.code {
                KeyCode::Esc => {
                    self.state.phase = WizardPhase::ToolsAndSearch;
                    self.state.step_index = 5;
                }
                KeyCode::Tab | KeyCode::Down => {
                    self.state.budget_field = (self.state.budget_field + 1) % 2;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.state.budget_field = if self.state.budget_field == 0 { 1 } else { 0 };
                }
                KeyCode::Char(' ') if self.state.budget_field == 0 => {
                    self.state.budget_enabled = !self.state.budget_enabled;
                }
                KeyCode::Backspace if self.state.budget_field == 1 => {
                    self.state.daily_limit_input.pop();
                }
                KeyCode::Char(c) if self.state.budget_field == 1 => {
                    if c.is_ascii_digit() || c == '.' {
                        self.state.daily_limit_input.push(c);
                    }
                }
                KeyCode::Enter => {
                    self.state.phase = WizardPhase::WriteAndSmoke;
                    self.state.step_index = 7;
                }
                _ => {}
            },

            WizardPhase::WriteAndSmoke => match key.code {
                KeyCode::Esc => {
                    self.state.phase = WizardPhase::BudgetGuardrails;
                    self.state.step_index = 6;
                }
                KeyCode::Char('w') | KeyCode::Enter if !self.state.config_written => {
                    self.save_configuration()?;
                }
                KeyCode::Char('s') if self.state.config_written && !self.state.is_running_smoke => {
                    self.run_smoke_test().await?;
                }
                KeyCode::Char('q') | KeyCode::Char('d') if self.state.config_written => {
                    self.state.phase = WizardPhase::Finished;
                }
                _ => {}
            },

            WizardPhase::Finished => return Ok(true),
        }

        Ok(false)
    }

    fn sync_provider_endpoint(&mut self) {
        let kind = self.state.provider_choices[self.state.selected_provider_idx].1;
        self.state.endpoint_input = default_base_url(kind).to_string();
    }

    fn update_filtered_models(&mut self) {
        let query = self.state.model_filter.trim().to_lowercase();
        self.state.filtered_model_indices = self
            .state
            .model_list
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                if query.is_empty() {
                    true
                } else {
                    m.id.to_lowercase().contains(&query)
                }
            })
            .map(|(i, _)| i)
            .collect();
        self.state.selected_model_list_idx = 0;
    }

    async fn start_model_fetch(&mut self) -> Result<()> {
        self.state.is_loading_models = true;
        self.state.load_error = None;

        let kind = self.state.provider_choices[self.state.selected_provider_idx].1;
        let endpoint = self.state.endpoint_input.clone();
        let api_key = if self.state.api_key_input.trim().is_empty() {
            None
        } else {
            Some(self.state.api_key_input.trim().to_string())
        };

        let client_res = build_provider_client(kind, &endpoint, api_key);
        match client_res {
            Ok(provider) => {
                let cache = kerux_core::client::ModelCache::default_location();
                let (models, err) = kerux_core::client::discover_models_or_empty(
                    &*provider,
                    &cache,
                    kind,
                    &endpoint,
                    true,
                    DISCOVER_TIMEOUT,
                )
                .await;
                self.state.model_list = models;
                self.state.load_error = err;
                self.update_filtered_models();
            }
            Err(e) => {
                self.state.load_error = Some(format!("Failed to build client: {e}"));
            }
        }

        self.state.is_loading_models = false;
        Ok(())
    }

    async fn run_capability_probe(&mut self) -> Result<()> {
        self.state.is_probing = true;
        self.state.probe_error = None;
        let kind = self.state.provider_choices[self.state.selected_provider_idx].1;
        let endpoint = self.state.endpoint_input.clone();
        let api_key = if self.state.api_key_input.trim().is_empty() {
            None
        } else {
            Some(self.state.api_key_input.trim().to_string())
        };

        if let Some(model) = &self.state.chosen_model {
            match build_provider_client(kind, &endpoint, api_key) {
                Ok(provider) => {
                    let res = probe_model(&*provider, model, PROBE_TIMEOUT).await;
                    self.state.probe_result = Some(res);
                }
                Err(e) => {
                    self.state.probe_error = Some(format!("Probe error: {e}"));
                }
            }
        }
        self.state.is_probing = false;
        Ok(())
    }

    fn save_configuration(&mut self) -> Result<()> {
        let kind = self.state.provider_choices[self.state.selected_provider_idx].1;
        let endpoint = self.state.endpoint_input.clone();
        let key = if self.state.api_key_input.trim().is_empty() {
            None
        } else {
            Some(self.state.api_key_input.trim().to_string())
        };
        let model = self
            .state
            .chosen_model
            .clone()
            .unwrap_or_else(|| "gpt-4o".to_string());

        let preset_name = self.state.tool_presets[self.state.selected_preset_idx]
            .0
            .to_string();
        let search_url = if self.state.web_search_url_input.trim().is_empty() {
            None
        } else {
            Some(self.state.web_search_url_input.trim().to_string())
        };
        let daily_budget = if self.state.budget_enabled {
            self.state.daily_limit_input.trim().parse::<f64>().ok()
        } else {
            None
        };

        let plan = WizardPlan {
            provider: kind,
            base_url: endpoint,
            api_key: match &self.state.key_source_note {
                Some(source) if source.contains("env") => None,
                _ => key,
            },
            model,
            fallback: self.state.fallback_choice.clone(),
            toolset_preset: Some(preset_name),
            web_search_url: search_url,
            budget_daily_limit: daily_budget,
        };

        write_wizard_config(&self.state.config_target, &plan, true)?;
        self.state.config_written = true;
        self.state.status_message = Some(("Configuration successfully saved!".into(), SUCCESS));
        Ok(())
    }

    async fn run_smoke_test(&mut self) -> Result<()> {
        self.state.is_running_smoke = true;
        let kind = self.state.provider_choices[self.state.selected_provider_idx].1;
        let endpoint = self.state.endpoint_input.clone();
        let key = if self.state.api_key_input.trim().is_empty() {
            None
        } else {
            Some(self.state.api_key_input.trim().to_string())
        };
        let model = self.state.chosen_model.clone().unwrap_or_default();

        match build_provider_client(kind, &endpoint, key) {
            Ok(provider) => match smoke_test(&*provider, &model).await {
                Ok(outcome) => {
                    self.state.smoke_outcome = Some(Ok(outcome));
                }
                Err(e) => {
                    self.state.smoke_outcome = Some(Err(format!("{e:#}")));
                }
            },
            Err(e) => {
                self.state.smoke_outcome = Some(Err(format!("Client creation failed: {e}")));
            }
        }
        self.state.is_running_smoke = false;
        Ok(())
    }
}

fn build_provider_client(
    kind: ProviderKind,
    endpoint: &str,
    api_key: Option<String>,
) -> Result<Arc<dyn LLMProvider>> {
    build_plan_provider(kind, endpoint, api_key)
}

// ---------------------------------------------------------------------------
// Render Pipeline
// ---------------------------------------------------------------------------

fn render_wizard_ui(frame: &mut Frame<'_>, state: &TuiWizardState) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // Header + ASCII banner
            Constraint::Length(3), // Step Stepper
            Constraint::Min(12),   // Active Step Content Card
            Constraint::Length(3), // Navigation & Status Footer
        ])
        .split(area);

    render_header(frame, vertical[0]);
    render_stepper(frame, vertical[1], state.step_index);
    render_content(frame, vertical[2], state);
    render_footer(frame, vertical[3], state);
}

fn render_header(frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(34), Constraint::Min(20)])
        .split(area);

    let banner_lines: Vec<Line> = ASCII_LOGO
        .iter()
        .map(|&l| {
            Line::from(Span::styled(
                l,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(banner_lines), chunks[0]);

    let title_block = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled(
                "ONBOARDING & SETUP WIZARD",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  v0.4.0", Style::default().fg(MUTED)),
        ]),
        Line::from(Span::styled(
            "Intuitive, zero-config agent onboarding powered by Ratatui",
            Style::default().fg(HELP),
        )),
        Line::from(Span::styled(
            "Fasilitasi model LLM, capability probing, fallback failover & guardrails",
            Style::default().fg(MUTED),
        )),
    ]))
    .alignment(Alignment::Left);
    frame.render_widget(title_block, chunks[1]);
}

fn render_stepper(frame: &mut Frame<'_>, area: Rect, current_step: usize) {
    let mut spans = Vec::new();
    for (i, (num, name)) in WIZARD_STEPS.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ──▶  ", Style::default().fg(PANEL_ALT)));
        }
        let is_current = i == current_step;
        let is_done = i < current_step;

        let (box_color, text_color, modif) = if is_current {
            (ACCENT, TEXT, Modifier::BOLD)
        } else if is_done {
            (SUCCESS, TEXT, Modifier::empty())
        } else {
            (MUTED, MUTED, Modifier::empty())
        };

        let badge = if is_done {
            "✓".to_string()
        } else {
            num.to_string()
        };
        spans.push(Span::styled(
            format!("[{badge}] "),
            Style::default().fg(box_color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            *name,
            Style::default().fg(text_color).add_modifier(modif),
        ));
    }

    let p = Paragraph::new(Line::from(spans))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(PANEL)),
        )
        .alignment(Alignment::Center);
    frame.render_widget(p, area);
}

fn render_content(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    match state.phase {
        WizardPhase::Provider => render_provider_step(frame, area, state),
        WizardPhase::EndpointAndKey => render_endpoint_key_step(frame, area, state),
        WizardPhase::Model => render_model_step(frame, area, state),
        WizardPhase::Probe => render_probe_step(frame, area, state),
        WizardPhase::Fallback => render_fallback_step(frame, area, state),
        WizardPhase::ToolsAndSearch => render_tools_search_step(frame, area, state),
        WizardPhase::BudgetGuardrails => render_budget_guardrails_step(frame, area, state),
        WizardPhase::WriteAndSmoke | WizardPhase::Finished => {
            render_write_smoke_step(frame, area, state)
        }
    }
}

fn render_provider_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 1: Select LLM Provider ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(8)])
        .split(inner);

    let desc = Paragraph::new(Span::styled(
        "Pilih upstream provider default untuk Kerux. Panah Atas/Bawah (atau j/k) untuk memilih, Enter untuk lanjut.",
        Style::default().fg(HELP),
    ));
    frame.render_widget(desc, chunks[0]);

    let mut items = Vec::new();
    for (i, (label, kind)) in state.provider_choices.iter().enumerate() {
        let is_sel = i == state.selected_provider_idx;
        let prefix = if is_sel { "  ❯ " } else { "    " };
        let mut line_spans = vec![
            Span::styled(
                prefix,
                Style::default().fg(if is_sel { ACCENT } else { MUTED }),
            ),
            Span::styled(
                *label,
                Style::default()
                    .fg(if is_sel { TEXT } else { HELP })
                    .add_modifier(if is_sel {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        if is_sel {
            line_spans.push(Span::styled(
                format!("  (Slug: {})", kind.as_str()),
                Style::default().fg(MUTED),
            ));
        }
        items.push(ListItem::new(Line::from(line_spans)));
    }

    let list = List::new(items).style(Style::default().bg(PANEL));
    frame.render_widget(list, chunks[1]);
}

fn render_endpoint_key_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 2: Provider Endpoint & API Key ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(inner);

    let desc = Paragraph::new(Span::styled(
        "Konfigurasi URL base provider dan credential. Tab / Panah untuk switch input, Enter untuk fetch models.",
        Style::default().fg(HELP),
    ));
    frame.render_widget(desc, chunks[0]);

    // Endpoint field
    let is_f0 = state.active_field == 0;
    let endpoint_block = Block::default()
        .title(Span::styled(
            " Base Endpoint URL ",
            Style::default().fg(if is_f0 { ACCENT } else { MUTED }),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if is_f0 { ACCENT } else { PANEL_ALT }));
    let endpoint_p = Paragraph::new(state.endpoint_input.as_str())
        .block(endpoint_block)
        .style(Style::default().fg(TEXT));
    frame.render_widget(endpoint_p, chunks[1]);

    // API Key field
    let is_f1 = state.active_field == 1;
    let key_block = Block::default()
        .title(Span::styled(
            " API Key / Token ",
            Style::default().fg(if is_f1 { ACCENT } else { MUTED }),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if is_f1 { ACCENT } else { PANEL_ALT }));
    let displayed_key = if state.api_key_input.is_empty() {
        "(kosong / tidak butuh key untuk Ollama)".to_string()
    } else {
        mask_secret(&state.api_key_input)
    };
    let key_p = Paragraph::new(displayed_key)
        .block(key_block)
        .style(Style::default().fg(if state.api_key_input.is_empty() {
            MUTED
        } else {
            TEXT
        }));
    frame.render_widget(key_p, chunks[2]);

    if let Some(note) = &state.key_source_note {
        let note_p = Paragraph::new(Line::from(vec![
            Span::styled("ℹ ", Style::default().fg(ACCENT)),
            Span::styled(note, Style::default().fg(HELP)),
        ]));
        frame.render_widget(note_p, chunks[3]);
    }
}

fn render_model_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 3: Model Selection & Capabilities ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Search / Mode bar
            Constraint::Min(8),    // List or Manual Input
            Constraint::Length(2), // Hint
        ])
        .split(inner);

    if state.is_manual_model_mode {
        let bar = Block::default()
            .title(Span::styled(
                " Mode: Manual Model ID Input ",
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(WARN));
        let p = Paragraph::new(state.manual_model_input.as_str())
            .block(bar)
            .style(Style::default().fg(TEXT));
        frame.render_widget(p, chunks[0]);

        let help = Paragraph::new(Span::styled(
            "Ketik model id langsung (misal: 'gpt-4o', 'claude-3-7-sonnet'). Tekan Ctrl+M untuk kembali ke daftar auto-discovery.",
            Style::default().fg(HELP),
        ));
        frame.render_widget(help, chunks[1]);
    } else {
        let search_block = Block::default()
            .title(Span::styled(
                format!(" Filter Models ({}) ", state.filtered_model_indices.len()),
                Style::default().fg(ACCENT),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(PANEL_ALT));
        let p = Paragraph::new(state.model_filter.as_str())
            .block(search_block)
            .style(Style::default().fg(TEXT));
        frame.render_widget(p, chunks[0]);

        if state.is_loading_models {
            let loading_p = Paragraph::new("Sedang memindai model dari provider endpoint...")
                .style(Style::default().fg(WARN));
            frame.render_widget(loading_p, chunks[1]);
        } else if let Some(err) = &state.load_error {
            let lines = vec![
                Line::from(Span::styled(
                    format!("⚠ Gagal mengambil daftar model: {err}"),
                    Style::default().fg(ERROR),
                )),
                Line::from(Span::styled(
                    "Gunakan Ctrl+M untuk beralih ke input manual, atau Ctrl+R untuk mencoba lagi.",
                    Style::default().fg(HELP),
                )),
            ];
            frame.render_widget(Paragraph::new(lines), chunks[1]);
        } else if state.filtered_model_indices.is_empty() {
            let p = Paragraph::new(
                "Tidak ada model yang cocok dengan filter. Tekan Ctrl+M untuk input manual.",
            )
            .style(Style::default().fg(MUTED));
            frame.render_widget(p, chunks[1]);
        } else {
            // Render list window
            let visible_count = chunks[1].height as usize;
            let total = state.filtered_model_indices.len();
            let start = if state.selected_model_list_idx >= visible_count {
                state.selected_model_list_idx + 1 - visible_count
            } else {
                0
            };
            let end = (start + visible_count).min(total);

            let mut items = Vec::new();
            for i in start..end {
                let model_idx = state.filtered_model_indices[i];
                let model = &state.model_list[model_idx];
                let is_sel = i == state.selected_model_list_idx;

                let prefix = if is_sel { " ❯ " } else { "   " };
                let mut spans = vec![
                    Span::styled(
                        prefix,
                        Style::default().fg(if is_sel { ACCENT } else { MUTED }),
                    ),
                    Span::styled(
                        model.id.clone(),
                        Style::default()
                            .fg(if is_sel { TEXT } else { HELP })
                            .add_modifier(if is_sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ];

                if let Some(ctx) = model.context_window {
                    spans.push(Span::styled(
                        format!("  [ctx {}]", format_context_window(ctx)),
                        Style::default().fg(PANEL_ALT),
                    ));
                }

                items.push(ListItem::new(Line::from(spans)));
            }
            frame.render_widget(List::new(items), chunks[1]);
        }

        let hint = Paragraph::new(Span::styled(
            "Panah Atas/Bawah: Pilih model | Ketik: Filter teks | Enter: Pilih | Ctrl+M: Input manual | Ctrl+R: Refresh",
            Style::default().fg(MUTED),
        ));
        frame.render_widget(hint, chunks[2]);
    }
}

fn render_probe_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 4: Capability Probing ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(inner);

    let model_name = state.chosen_model.as_deref().unwrap_or("Unknown");
    let top = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("Target Model: ", Style::default().fg(HELP)),
            Span::styled(model_name, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(Span::styled("Probing menguji langsung respon nyata: Streaming token, Tool Call execution, dan Base64 Vision.", Style::default().fg(MUTED))),
    ]));
    frame.render_widget(top, chunks[0]);

    if state.is_probing {
        let p = Paragraph::new(Span::styled(
            "Sedang menjalankan 3 micro-test ke upstream... Mohon tunggu...",
            Style::default().fg(WARN),
        ));
        frame.render_widget(p, chunks[1]);
    } else if let Some(res) = &state.probe_result {
        let lines: Vec<Line> = probe_summary_lines(res)
            .into_iter()
            .map(|l| Line::from(Span::raw(l)))
            .collect();
        frame.render_widget(Paragraph::new(lines), chunks[1]);
    } else if let Some(err) = &state.probe_error {
        let p = Paragraph::new(Span::styled(
            format!("Gagal probe: {err}"),
            Style::default().fg(ERROR),
        ));
        frame.render_widget(p, chunks[1]);
    } else {
        let p = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Tekan [Enter] atau [P] untuk memulai probe live.",
                Style::default().fg(TEXT),
            )),
            Line::from(Span::styled(
                "Tekan [S] untuk lewati tahap ini dan langsung ke setup Fallback.",
                Style::default().fg(HELP),
            )),
        ]));
        frame.render_widget(p, chunks[1]);
    }

    let hint = Paragraph::new(Span::styled(
        "Enter: Mulai Probe | S: Lewati (Skip) | Esc: Kembali",
        Style::default().fg(MUTED),
    ));
    frame.render_widget(hint, chunks[2]);
}

fn render_fallback_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 5: High-Availability Fallback Routing ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3), // Toggle
            Constraint::Length(3), // Provider
            Constraint::Length(3), // Endpoint
            Constraint::Length(3), // Key
            Constraint::Length(3), // Model
            Constraint::Min(1),
        ])
        .split(inner);

    let desc = Paragraph::new(Span::styled(
        "Konfigurasi failover otomatis saat primary provider kehabisan kuota, 429 rate limit, atau server outage.",
        Style::default().fg(HELP),
    ));
    frame.render_widget(desc, chunks[0]);

    // Field 0: Toggle
    let is_f0 = state.fallback_field == 0;
    let toggle_box = format!(
        "[{}] Aktifkan Fallback Provider",
        if state.want_fallback { "✓" } else { " " }
    );
    let toggle_p = Paragraph::new(Span::styled(
        toggle_box,
        Style::default()
            .fg(if is_f0 { ACCENT } else { TEXT })
            .add_modifier(if is_f0 {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    ))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(PANEL_ALT)),
    );
    frame.render_widget(toggle_p, chunks[1]);

    if state.want_fallback {
        // Field 1: Provider selection
        let is_f1 = state.fallback_field == 1;
        let p_name = state.provider_choices[state.fallback_provider_idx].0;
        let prov_p = Paragraph::new(format!("Provider: ◀ {} ▶", p_name))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if is_f1 { ACCENT } else { PANEL_ALT })),
            )
            .style(Style::default().fg(TEXT));
        frame.render_widget(prov_p, chunks[2]);

        // Field 2: Endpoint
        let is_f2 = state.fallback_field == 2;
        let end_p = Paragraph::new(state.fallback_endpoint.as_str())
            .block(
                Block::default()
                    .title(" Fallback Endpoint ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if is_f2 { ACCENT } else { PANEL_ALT })),
            )
            .style(Style::default().fg(TEXT));
        frame.render_widget(end_p, chunks[3]);

        // Field 3: Key
        let is_f3 = state.fallback_field == 3;
        let key_disp = if state.fallback_key.is_empty() {
            "(blank)"
        } else {
            "••••••••"
        };
        let key_p = Paragraph::new(key_disp)
            .block(
                Block::default()
                    .title(" Fallback API Key ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if is_f3 { ACCENT } else { PANEL_ALT })),
            )
            .style(Style::default().fg(TEXT));
        frame.render_widget(key_p, chunks[4]);

        // Field 4: Model
        let is_f4 = state.fallback_field == 4;
        let mod_p = Paragraph::new(state.fallback_model.as_str())
            .block(
                Block::default()
                    .title(" Fallback Model ID ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if is_f4 { ACCENT } else { PANEL_ALT })),
            )
            .style(Style::default().fg(TEXT));
        frame.render_widget(mod_p, chunks[5]);
    } else {
        let disabled_msg = Paragraph::new(Span::styled(
            "Fallback dinonaktifkan (tekan Spasi pada toggle untuk mengaktifkan)",
            Style::default().fg(MUTED),
        ));
        frame.render_widget(disabled_msg, chunks[2]);
    }
}

fn render_tools_search_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 6: Toolset Preset & Web Search Engine ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3), // Preset Selector
            Constraint::Length(3), // Web Search URL
            Constraint::Min(4),    // Tool details
            Constraint::Length(2), // Hint
        ])
        .split(inner);

    let desc = Paragraph::new(Span::styled(
        "Pilih preset tool runtime dan konfigurasi zero-token DuckDuckGo search endpoint.",
        Style::default().fg(HELP),
    ));
    frame.render_widget(desc, chunks[0]);

    // Field 0: Tool preset
    let is_f0 = state.tool_field == 0;
    let (p_name, p_desc) = state.tool_presets[state.selected_preset_idx];
    let preset_p = Paragraph::new(format!(
        "Preset: ◀ {} ▶  ({})",
        p_name.to_uppercase(),
        p_desc
    ))
    .block(
        Block::default()
            .title(" Active Tool Profile (Panah Kiri/Kanan) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if is_f0 { ACCENT } else { PANEL_ALT })),
    )
    .style(Style::default().fg(TEXT));
    frame.render_widget(preset_p, chunks[1]);

    // Field 1: Search URL
    let is_f1 = state.tool_field == 1;
    let url_p = Paragraph::new(state.web_search_url_input.as_str())
        .block(
            Block::default()
                .title(" Zero-Token Web Search Endpoint ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if is_f1 { ACCENT } else { PANEL_ALT })),
        )
        .style(Style::default().fg(TEXT));
    frame.render_widget(url_p, chunks[2]);

    let details = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("• Web Search: ", Style::default().fg(ACCENT)),
            Span::styled(
                "DuckDuckGo Lite HTML scraper (hemat bandwidth & tanpa API key external)",
                Style::default().fg(HELP),
            ),
        ]),
        Line::from(vec![
            Span::styled("• Sandboxing: ", Style::default().fg(ACCENT)),
            Span::styled(
                "Eksekusi terminal & patch dibatasi dalam workspace project",
                Style::default().fg(HELP),
            ),
        ]),
        Line::from(vec![
            Span::styled("• Memory: ", Style::default().fg(ACCENT)),
            Span::styled(
                "Long-term distilled facts & skills lifecycle curator otomatis aktif",
                Style::default().fg(HELP),
            ),
        ]),
    ]));
    frame.render_widget(details, chunks[3]);

    let hint = Paragraph::new(Span::styled(
        "Tab: Pindah Field | Panah Kiri/Kanan: Ganti Preset | Enter: Lanjut ke Budget Guardrails",
        Style::default().fg(MUTED),
    ));
    frame.render_widget(hint, chunks[4]);
}

fn render_budget_guardrails_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 7: Safety & Cost Guardrails ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3), // Toggle
            Constraint::Length(3), // Daily limit input
            Constraint::Min(4),    // Info policy
            Constraint::Length(2), // Hint
        ])
        .split(inner);

    let desc = Paragraph::new(Span::styled(
        "Pasang batas pengeluaran token harian (hard ceiling) untuk mencegah kuota tersedot loop otonom.",
        Style::default().fg(HELP),
    ));
    frame.render_widget(desc, chunks[0]);

    // Field 0: Toggle
    let is_f0 = state.budget_field == 0;
    let toggle_box = format!(
        "[{}] Aktifkan Cost Guardrails Engine",
        if state.budget_enabled { "✓" } else { " " }
    );
    let toggle_p = Paragraph::new(Span::styled(
        toggle_box,
        Style::default()
            .fg(if is_f0 { ACCENT } else { TEXT })
            .add_modifier(if is_f0 {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    ))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(PANEL_ALT)),
    );
    frame.render_widget(toggle_p, chunks[1]);

    if state.budget_enabled {
        let is_f1 = state.budget_field == 1;
        let limit_p = Paragraph::new(format!("${} / hari", state.daily_limit_input))
            .block(
                Block::default()
                    .title(" Daily Spend Ceiling (USD) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if is_f1 { ACCENT } else { PANEL_ALT })),
            )
            .style(Style::default().fg(TEXT));
        frame.render_widget(limit_p, chunks[2]);

        let info = Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled("Policy Trip: ", Style::default().fg(WARN)),
                Span::styled(
                    "Otomatis PAUSE dan beri notifikasi ketika limit 80% dan 100% tercapai.",
                    Style::default().fg(HELP),
                ),
            ]),
            Line::from(vec![
                Span::styled("Sovereign: ", Style::default().fg(ACCENT)),
                Span::styled(
                    "User tetap memegang kendali penuh untuk resume kapan saja.",
                    Style::default().fg(HELP),
                ),
            ]),
        ]));
        frame.render_widget(info, chunks[3]);
    } else {
        let disabled_msg = Paragraph::new(Span::styled(
            "Cost Guardrails dinonaktifkan (Unlimited spend). Tekan Spasi pada toggle untuk mengaktifkan.",
            Style::default().fg(MUTED),
        ));
        frame.render_widget(disabled_msg, chunks[2]);
    }

    let hint = Paragraph::new(Span::styled(
        "Spasi: Toggle On/Off | Tab: Pindah Input | Ketik Angka: Set Limit | Enter: Simpan & Selesai",
        Style::default().fg(MUTED),
    ));
    frame.render_widget(hint, chunks[4]);
}

fn render_write_smoke_step(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let block = Block::default()
        .title(Span::styled(
            " Step 8: Persist Configuration & Smoke Verification ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PANEL_ALT))
        .style(Style::default().bg(PANEL));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // Summary card
            Constraint::Min(6),    // Smoke output
            Constraint::Length(2), // Hints
        ])
        .split(inner);

    let prov = state.provider_choices[state.selected_provider_idx].0;
    let model = state.chosen_model.as_deref().unwrap_or("default");
    let dest = state.config_target.display().to_string();

    let summary = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("Target Path : ", Style::default().fg(HELP)),
            Span::styled(dest, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("Provider    : ", Style::default().fg(HELP)),
            Span::styled(prov, Style::default().fg(ACCENT)),
        ]),
        Line::from(vec![
            Span::styled("Model       : ", Style::default().fg(HELP)),
            Span::styled(
                model,
                Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Status File : ", Style::default().fg(HELP)),
            Span::styled(
                if state.config_written {
                    "TERSIMPAN ✓"
                } else {
                    "Belum ditulis (Tekan Enter/W)"
                },
                Style::default()
                    .fg(if state.config_written { SUCCESS } else { WARN })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ]));
    frame.render_widget(summary, chunks[0]);

    if state.is_running_smoke {
        let p = Paragraph::new(Span::styled(
            "Menjalankan smoke test ('say hi') ke model upstream...",
            Style::default().fg(WARN),
        ));
        frame.render_widget(p, chunks[1]);
    } else if let Some(outcome) = &state.smoke_outcome {
        match outcome {
            Ok(smoke) => {
                let lines = vec![
                    Line::from(Span::styled(
                        "✓ Smoke Test Berhasil!",
                        Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(format_smoke(smoke), Style::default().fg(HELP))),
                ];
                frame.render_widget(Paragraph::new(lines), chunks[1]);
            }
            Err(err) => {
                let p = Paragraph::new(Span::styled(
                    format!("⚠ Smoke test gagal: {err}"),
                    Style::default().fg(ERROR),
                ));
                frame.render_widget(p, chunks[1]);
            }
        }
    } else {
        let p = Paragraph::new(Span::styled(
            if state.config_written {
                "Tekan [S] untuk menjalankan uji verifikasi (Smoke Test) atau [D/Q] untuk selesai."
            } else {
                "Tekan [W] atau [Enter] untuk menyimpan konfigurasi ke disk."
            },
            Style::default().fg(HELP),
        ));
        frame.render_widget(p, chunks[1]);
    }

    let hint = Paragraph::new(Span::styled(
        "Enter/W: Tulis Config | S: Uji Smoke | D/Q: Selesai & Keluar",
        Style::default().fg(MUTED),
    ));
    frame.render_widget(hint, chunks[2]);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &TuiWizardState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let status_line = if let Some((msg, col)) = &state.status_message {
        Line::from(Span::styled(
            msg,
            Style::default().fg(*col).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(vec![
            Span::styled("Kerux Framework  ·  ", Style::default().fg(MUTED)),
            Span::styled("Zero-friction onboarding", Style::default().fg(HELP)),
        ])
    };
    frame.render_widget(Paragraph::new(status_line), chunks[0]);

    let controls = Paragraph::new(Line::from(vec![
        Span::styled("[Esc] ", Style::default().fg(ACCENT)),
        Span::styled("Kembali  ", Style::default().fg(HELP)),
        Span::styled("[Ctrl+C] ", Style::default().fg(ACCENT)),
        Span::styled("Batal", Style::default().fg(HELP)),
    ]))
    .alignment(Alignment::Right);
    frame.render_widget(controls, chunks[1]);
}
