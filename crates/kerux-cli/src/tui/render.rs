use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::layout::{
    build_compact_layout, build_constrained_layout, build_desktop_layout,
    build_landing_compact_layout, build_landing_constrained_layout, build_medium_layout,
    centered_rect_percent, is_constrained, DESKTOP_HEIGHT, DESKTOP_WIDTH,
};
use crate::tui::state::{
    ActivePanel, AppState, InputMode, LayoutMode, McpServerItem, SkillItem, Tone, ToolCallLine,
    TranscriptEntry,
};
use crate::tui::theme::{ACCENT, BG, ERROR, HELP, MUTED, PANEL, PANEL_ALT, SUCCESS, TEXT, WARN};

pub fn render(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(background_fill(area, BG), area);

    match state.ui.view {
        crate::tui::state::ViewMode::Landing => render_landing(frame, state, area),
        crate::tui::state::ViewMode::Workspace => render_workspace(frame, state, area),
    }

    if let Some(modal) = &state.ui.modal {
        render_modal(
            frame,
            area,
            modal.title(),
            modal.help(),
            &modal.form().fields,
            modal.form().selected,
        );
    }

    if matches!(state.ui.input_mode, InputMode::Prompt) {
        set_active_input_cursor(frame, state, area);
    }
}

fn set_active_input_cursor(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    if state.ui.modal.is_some() {
        return;
    }

    match state.ui.view {
        crate::tui::state::ViewMode::Landing => {
            if is_constrained(area) {
                let layout = build_landing_constrained_layout(area);
                let text_w = UnicodeWidthStr::width(state.ui.prompt_input.as_str()) as u16;
                let cx = layout
                    .prompt
                    .x
                    .saturating_add(1)
                    .saturating_add(text_w)
                    .min(layout.prompt.right().saturating_sub(2));
                let cy = layout.prompt.y.saturating_add(1);
                frame.set_cursor_position((cx, cy));
            } else if area.width < 100 {
                let is_portrait_like = area.width < 56;
                let layout = build_landing_compact_layout(area, is_portrait_like);
                let text_w = UnicodeWidthStr::width(state.ui.prompt_input.as_str()) as u16;
                let cx = layout
                    .prompt
                    .x
                    .saturating_add(text_w)
                    .min(layout.prompt.right().saturating_sub(1));
                let cy = layout.prompt.y;
                frame.set_cursor_position((cx, cy));
            } else {
                let prompt_area = centered_rect_percent(area, 52, 40, 100, 8);
                let text_w = UnicodeWidthStr::width(state.ui.prompt_input.as_str()) as u16;
                let cx = prompt_area
                    .x
                    .saturating_add(1)
                    .saturating_add(text_w)
                    .min(prompt_area.right().saturating_sub(2));
                let cy = prompt_area.y.saturating_add(1);
                frame.set_cursor_position((cx, cy));
            }
        }
        crate::tui::state::ViewMode::Workspace => {
            if is_constrained(area) {
                let layout = build_constrained_layout(area);
                let status = status_summary(state);
                let prompt_width = area.width.saturating_sub(status.len() as u16 + 12) as usize;
                let displayed = sliding_prompt_display(&state.ui.prompt_input, prompt_width);
                let disp_w = UnicodeWidthStr::width(displayed.as_str()) as u16;
                // Format: "PROMPT " (7 chars) + prompt
                let cx = layout
                    .footer
                    .x
                    .saturating_add(7)
                    .saturating_add(disp_w)
                    .min(layout.footer.right().saturating_sub(1));
                let cy = layout.footer.y;
                frame.set_cursor_position((cx, cy));
            } else {
                let footer_y = area.bottom().saturating_sub(3); // footer block has border
                let displayed = sliding_prompt_display(&state.ui.prompt_input, 180);
                let disp_w = UnicodeWidthStr::width(displayed.as_str()) as u16;
                // Format: Border (1) + "PROMPT " (7) + displayed
                let cx = area
                    .x
                    .saturating_add(1)
                    .saturating_add(7)
                    .saturating_add(disp_w)
                    .min(area.right().saturating_sub(2));
                let cy = footer_y.saturating_add(1);
                frame.set_cursor_position((cx, cy));
            }
        }
    }
}

fn render_landing(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    if is_constrained(area) {
        render_landing_constrained(frame, state, area);
        return;
    }

    if area.width < 100 {
        render_landing_compact(frame, state, area);
        return;
    }

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(18),
            Constraint::Max(6),
            Constraint::Max(8),
            Constraint::Min(3),
            Constraint::Max(2),
        ])
        .split(area);

    let title = build_landing_title(state);
    frame.render_widget(title, vertical[1]);

    let prompt = build_landing_prompt(state, area.width);
    let prompt_area = centered_rect_percent(area, 52, 40, 100, 8);
    frame.render_widget(prompt, prompt_area);

    let footer_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(vertical[4]);

    let footer = build_landing_footer();
    frame.render_widget(footer, footer_row[1]);

    let status = build_landing_status(state);
    frame.render_widget(status, footer_row[0]);
}

fn build_landing_title<'a>(state: &'a AppState) -> Paragraph<'a> {
    Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            state.persistent.config.tui.landing_title.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "prompt-first terminal agent",
            Style::default().fg(MUTED),
        )),
    ]))
    .style(Style::default().bg(BG))
    .alignment(Alignment::Center)
}

fn build_landing_prompt<'a>(state: &'a AppState, area_width: u16) -> Paragraph<'a> {
    let prompt_block = Block::default()
        .borders(Borders::LEFT | Borders::BOTTOM)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(PANEL))
        .title(Span::styled(" prompt ", Style::default().fg(ACCENT)));
    let prompt_text = if state.ui.prompt_input.is_empty() {
        state.persistent.config.tui.prompt_placeholder.clone()
    } else {
        state.ui.prompt_input.clone()
    };
    Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            prompt_text,
            Style::default()
                .fg(if state.ui.prompt_input.is_empty() {
                    MUTED
                } else {
                    TEXT
                })
                .add_modifier(if matches!(state.ui.input_mode, InputMode::Prompt) {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Plan",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" · "),
            Span::styled(
                truncate_display(&state.persistent.behavior.model, area_width as usize / 2),
                Style::default().fg(TEXT),
            ),
        ]),
    ]))
    .block(prompt_block)
    .wrap(Wrap { trim: true })
}

fn build_landing_footer<'a>() -> Paragraph<'a> {
    Paragraph::new(Text::from(vec![Line::from(vec![
        Span::styled(
            "tab",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" panels   ", Style::default().fg(HELP)),
        Span::styled(
            "i",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" prompt   ", Style::default().fg(HELP)),
        Span::styled(
            "enter",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" run", Style::default().fg(HELP)),
    ])]))
    .style(Style::default().bg(BG))
    .alignment(Alignment::Right)
}

fn build_landing_status<'a>(state: &'a AppState) -> Paragraph<'a> {
    Paragraph::new(Line::from(vec![Span::styled(
        status_summary(state),
        Style::default()
            .fg(status_color(state))
            .add_modifier(Modifier::BOLD),
    )]))
    .style(Style::default().bg(BG))
    .alignment(Alignment::Left)
}

fn build_compact_title<'a>(state: &'a AppState) -> Paragraph<'a> {
    Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            state.persistent.config.tui.landing_title.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "prompt-first terminal agent",
            Style::default().fg(MUTED),
        )),
    ]))
    .style(Style::default().bg(BG))
    .alignment(Alignment::Center)
}

fn build_compact_prompt<'a>(state: &'a AppState, area_width: u16) -> Paragraph<'a> {
    let prompt_text = if state.ui.prompt_input.is_empty() {
        state.persistent.config.tui.prompt_placeholder.clone()
    } else {
        state.ui.prompt_input.clone()
    };
    Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            prompt_text,
            Style::default()
                .fg(if state.ui.prompt_input.is_empty() {
                    MUTED
                } else {
                    TEXT
                })
                .add_modifier(if matches!(state.ui.input_mode, InputMode::Prompt) {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Plan",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" · "),
            Span::styled(
                truncate_display(&state.persistent.behavior.model, area_width as usize / 2),
                Style::default().fg(TEXT),
            ),
        ]),
    ]))
    .block(panel_block("Prompt"))
    .wrap(Wrap { trim: true })
}

fn build_compact_controls<'a>(is_portrait_like: bool) -> Paragraph<'a> {
    let controls = if is_portrait_like {
        vec![
            Line::from(vec![
                keycap("q"),
                label(" quit   "),
                keycap("i"),
                label(" prompt"),
            ]),
            Line::from(vec![
                keycap("Enter"),
                label(" run   "),
                keycap("Tab"),
                label(" panels"),
            ]),
        ]
    } else {
        vec![Line::from(vec![
            keycap("q"),
            label(" quit   "),
            keycap("i"),
            label(" prompt   "),
            keycap("Enter"),
            label(" run   "),
            keycap("Tab"),
            label(" panels"),
        ])]
    };
    Paragraph::new(Text::from(controls))
        .style(Style::default().bg(BG))
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true })
}

fn build_compact_status<'a>(state: &'a AppState) -> Paragraph<'a> {
    Paragraph::new(Line::from(vec![Span::styled(
        status_summary(state),
        Style::default()
            .fg(status_color(state))
            .add_modifier(Modifier::BOLD),
    )]))
    .style(Style::default().bg(BG))
    .alignment(Alignment::Left)
}

fn render_landing_compact(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let is_portrait_like = area.width < 56;
    let layout = build_landing_compact_layout(area, is_portrait_like);

    frame.render_widget(build_compact_title(state), layout.title);
    frame.render_widget(build_compact_prompt(state, area.width), layout.prompt);
    frame.render_widget(build_compact_controls(is_portrait_like), layout.controls);
    frame.render_widget(build_compact_status(state), layout.status);
}

fn render_landing_constrained(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let layout = build_landing_constrained_layout(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            truncate_display(
                &state.persistent.config.tui.landing_title,
                area.width as usize,
            ),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  prompt-first", Style::default().fg(MUTED)),
    ]))
    .style(Style::default().bg(BG))
    .alignment(Alignment::Left)
    .wrap(Wrap { trim: true });
    frame.render_widget(title, layout.title);

    let prompt_text = if state.ui.prompt_input.is_empty() {
        state.persistent.config.tui.prompt_placeholder.clone()
    } else {
        state.ui.prompt_input.clone()
    };
    let prompt = Paragraph::new(Text::from(vec![
        Line::from(Span::styled(
            prompt_text,
            Style::default()
                .fg(if state.ui.prompt_input.is_empty() {
                    MUTED
                } else {
                    TEXT
                })
                .add_modifier(if matches!(state.ui.input_mode, InputMode::Prompt) {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )),
        Line::from(vec![
            Span::styled(
                "Plan",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                truncate_display(
                    &state.persistent.behavior.model,
                    area.width.saturating_sub(7) as usize,
                ),
                Style::default().fg(TEXT),
            ),
        ]),
    ]))
    .block(panel_block("Prompt"))
    .wrap(Wrap { trim: true });
    frame.render_widget(prompt, layout.prompt);

    let help = Paragraph::new(Text::from(vec![Line::from(vec![
        keycap("q"),
        label(" quit  "),
        keycap("i"),
        label(" prompt  "),
        keycap("Enter"),
        label(" run  "),
        keycap("Tab"),
        label(" panels"),
    ])]))
    .style(Style::default().bg(BG))
    .wrap(Wrap { trim: true });
    frame.render_widget(help, layout.help);

    let status = Paragraph::new(Line::from(Span::styled(
        status_summary(state),
        Style::default()
            .fg(status_color(state))
            .add_modifier(Modifier::BOLD),
    )))
    .style(Style::default().bg(BG))
    .wrap(Wrap { trim: true });
    frame.render_widget(status, layout.status);
}

fn render_workspace(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    if is_constrained(area) {
        render_workspace_constrained(frame, state, area);
        return;
    }

    match responsive_workspace_mode(state.ui.layout, area) {
        LayoutMode::Wide => render_workspace_wide(frame, state, area),
        LayoutMode::Medium => render_workspace_medium(frame, state, area),
        LayoutMode::Compact => render_workspace_compact(frame, state, area),
    }
}

fn render_workspace_wide(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let layout = build_desktop_layout(area);
    frame.render_widget(header_widget(state), layout.header);
    render_conversation_widget(frame, state, layout.conversation);
    frame.render_widget(panel_widget(state), layout.panel);
    frame.render_widget(reasoning_widget(state), layout.reasoning);
    frame.render_widget(activity_widget(state), layout.activity);
    frame.render_widget(footer_widget(state), layout.footer);
}

fn render_workspace_medium(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let layout = build_medium_layout(area);
    frame.render_widget(header_widget(state), layout.header);
    render_conversation_widget(frame, state, layout.conversation);
    frame.render_widget(panel_tabs(state), layout.tabs);
    frame.render_widget(panel_widget(state), layout.panel);
    frame.render_widget(reasoning_widget(state), layout.reasoning);
    frame.render_widget(footer_widget(state), layout.footer);
}

fn render_workspace_compact(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let layout = build_compact_layout(area);
    frame.render_widget(header_widget(state), layout.header);
    frame.render_widget(panel_tabs(state), layout.tabs);
    if state.ui.active_panel == ActivePanel::Session {
        render_session_compact_widget(frame, state, layout.content);
    } else {
        frame.render_widget(panel_widget(state), layout.content);
    }
    frame.render_widget(footer_widget(state), layout.footer);
}

fn render_workspace_constrained(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let layout = build_constrained_layout(area);
    frame.render_widget(constrained_header_widget(state, area.width), layout.header);
    render_conversation_widget(frame, state, layout.content);

    if state.ui.active_panel != ActivePanel::Session {
        frame.render_widget(Clear, layout.popup);
        frame.render_widget(panel_widget(state), layout.popup);
    }

    frame.render_widget(constrained_footer_widget(state, area.width), layout.footer);
}

fn responsive_workspace_mode(mode: LayoutMode, area: Rect) -> LayoutMode {
    if area.height < DESKTOP_HEIGHT {
        return LayoutMode::Compact;
    }
    if mode == LayoutMode::Wide && area.width < DESKTOP_WIDTH {
        return LayoutMode::Medium;
    }
    mode
}

fn header_widget(state: &AppState) -> Paragraph<'_> {
    Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled(
                " kerux ",
                Style::default()
                    .fg(BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                truncate_display(&state.persistent.behavior.model, 42),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(state.ui.active_panel.title(), Style::default().fg(ACCENT)),
        ]),
        Line::from(vec![
            Span::styled("status ", Style::default().fg(MUTED)),
            Span::styled(progress_indicator(state), Style::default().fg(TEXT)),
            Span::raw("  "),
            Span::styled("session ", Style::default().fg(MUTED)),
            Span::styled(
                truncate_display(&state.session.title, 48),
                Style::default().fg(HELP),
            ),
        ]),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED))
            .style(Style::default().bg(PANEL_ALT)),
    )
}

fn constrained_header_widget(state: &AppState, width: u16) -> Paragraph<'_> {
    let model_width = width.saturating_sub(18) as usize;
    Paragraph::new(Line::from(vec![
        Span::styled(
            " kerux ",
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            truncate_display(&state.persistent.behavior.model, model_width),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(state.ui.active_panel.title(), Style::default().fg(ACCENT)),
        Span::raw(" "),
        Span::styled(
            truncate_display(&state.session.title, width.saturating_sub(30) as usize),
            Style::default().fg(HELP),
        ),
    ]))
    .style(Style::default().bg(PANEL_ALT))
    .wrap(Wrap { trim: true })
}

fn render_conversation_widget(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let block = panel_block("Conversation");
    let inner = block.inner(area);
    let lines = conversation_lines(state, inner.width);
    let max_scroll = max_wrapped_scroll(&lines, inner.width, inner.height);
    let scroll = if state.follow_conversation_tail() {
        max_scroll
    } else {
        state.conversation_scroll().min(max_scroll)
    };

    let widget = Paragraph::new(Text::from(lines))
        .block(block)
        .scroll((scroll, 0));
    frame.render_widget(widget, area);
}

fn conversation_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let entries: Vec<&TranscriptEntry> = state
        .session
        .transcript
        .iter()
        .rev()
        .take(8)
        .rev()
        .collect();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 && is_turn_start(entry.role) {
            lines.push(turn_separator(width));
        }
        lines.extend(turn_lines(entry, width));
        lines.push(Line::from(""));
    }

    if state.session.running {
        let content = if state.session.streaming_response.is_empty() {
            "Waiting for visible assistant output..."
        } else {
            &state.session.streaming_response
        };
        let mut streaming = TranscriptEntry::new("Assistant", content);
        streaming.tools = state.session.pending_tools.clone();
        lines.extend(turn_lines(&streaming, width));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No conversation yet. Press i and type a prompt.",
            Style::default().fg(MUTED),
        )));
    }

    lines
}

const GUTTER_WIDTH: usize = 9;

fn is_turn_start(role: &str) -> bool {
    role == "User" || role == "Shell"
}

fn turn_separator(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "┄".repeat(usize::from(width)),
        Style::default().fg(PANEL_ALT),
    ))
}

fn turn_lines(entry: &TranscriptEntry, width: u16) -> Vec<Line<'static>> {
    let body_width = usize::from(width).saturating_sub(GUTTER_WIDTH);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut first = true;
    for body_line in render_message_body(&entry.content) {
        for (index, wrapped) in wrap_line(body_line, body_width).into_iter().enumerate() {
            if first && index == 0 {
                let mut spans = glyph_spans(entry);
                spans.extend(wrapped.spans);
                lines.push(Line::from(spans));
            } else {
                let mut spans = vec![Span::raw(" ".repeat(GUTTER_WIDTH))];
                spans.extend(wrapped.spans);
                lines.push(Line::from(spans));
            }
        }
        first = false;
    }
    if lines.is_empty() {
        lines.push(Line::from(glyph_spans(entry)));
    }
    for tool in &entry.tools {
        lines.push(tool_subline(tool));
    }
    lines
}

fn glyph_spans(entry: &TranscriptEntry) -> Vec<Span<'static>> {
    let (glyph, color) = if is_turn_start(entry.role) {
        ("❯", SUCCESS)
    } else {
        ("✦", ACCENT)
    };
    vec![
        Span::styled(
            glyph.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            entry
                .timestamp
                .clone()
                .unwrap_or_else(|| "--:--".to_string()),
            Style::default().fg(MUTED),
        ),
        Span::raw("  "),
    ]
}

fn tool_subline(tool: &ToolCallLine) -> Line<'static> {
    let mut spans = vec![
        Span::raw(" ".repeat(GUTTER_WIDTH)),
        Span::styled(format!("┊ tool: {}", tool.name), Style::default().fg(MUTED)),
        Span::raw(" "),
        Span::styled(
            if tool.ok { "✓" } else { "✗" }.to_string(),
            Style::default().fg(if tool.ok { SUCCESS } else { ERROR }),
        ),
    ];
    if let Some(secs) = tool.duration_secs {
        spans.push(Span::styled(
            format!(" {:.1}s", secs),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let total: usize = line
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if width == 0 || total <= width {
        return vec![line];
    }

    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut col = 0usize;

    for span in line.spans {
        let style = span.style;
        for token in wrap_tokens(span.content.as_ref()) {
            let token_width = UnicodeWidthStr::width(token.as_str());
            if token_width == 0 {
                continue;
            }
            let is_space = token.bytes().all(|b| b == b' ');
            if col + token_width > width {
                if is_space {
                    continue;
                }
                if col > 0 {
                    rows.push(Vec::new());
                    col = 0;
                }
                if token_width > width {
                    let mut chunk = String::new();
                    let mut chunk_width = 0usize;
                    for ch in token.chars() {
                        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if col + chunk_width + ch_width > width && !chunk.is_empty() {
                            rows.last_mut()
                                .expect("rows is never empty")
                                .push(Span::styled(std::mem::take(&mut chunk), style));
                            rows.push(Vec::new());
                            col = 0;
                            chunk_width = 0;
                        }
                        chunk.push(ch);
                        chunk_width += ch_width;
                    }
                    if !chunk.is_empty() {
                        rows.last_mut()
                            .expect("rows is never empty")
                            .push(Span::styled(chunk, style));
                    }
                    col += chunk_width;
                    continue;
                }
            }
            if is_space && col == 0 {
                continue;
            }
            rows.last_mut()
                .expect("rows is never empty")
                .push(Span::styled(token, style));
            col += token_width;
        }
    }

    rows.into_iter()
        .filter(|row| !row.is_empty())
        .map(Line::from)
        .collect()
}

fn wrap_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_space: Option<bool> = None;
    for ch in text.chars() {
        let is_space = ch == ' ';
        if current_space.is_some_and(|prev| prev != is_space) {
            tokens.push(std::mem::take(&mut current));
        }
        current_space = Some(is_space);
        current.push(ch);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn reasoning_widget(state: &AppState) -> Paragraph<'_> {
    let lines = if state.session.reasoning.trim().is_empty() {
        if state.persistent.behavior.show_reasoning {
            vec![Line::from(Span::styled(
                "Reasoning pane waiting for structured thinking.",
                Style::default().fg(MUTED),
            ))]
        } else {
            vec![Line::from(Span::styled(
                "Reasoning display disabled.",
                Style::default().fg(MUTED),
            ))]
        }
    } else {
        render_reasoning_body(&state.session.reasoning)
    };

    Paragraph::new(Text::from(lines))
        .block(panel_block("Reasoning"))
        .wrap(Wrap { trim: true })
}

fn activity_widget(state: &AppState) -> Paragraph<'_> {
    let mut lines = Vec::new();
    for (index, entry) in state
        .session
        .activity
        .iter()
        .rev()
        .take(6)
        .rev()
        .enumerate()
    {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(render_activity_entry(entry));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No activity yet.",
            Style::default().fg(MUTED),
        )));
    }

    Paragraph::new(Text::from(lines))
        .block(panel_block("Activity"))
        .wrap(Wrap { trim: true })
}

fn render_activity_entry(entry: &crate::tui::state::ActivityItem) -> Vec<Line<'static>> {
    let color = tone_color(entry.tone);

    if is_tool_activity(&entry.label) {
        let title = truncate_text(&entry.label, 36);
        let body = truncate_text(&entry.body, 96);
        return vec![
            Line::from(vec![
                Span::styled("╭─ ", Style::default().fg(color)),
                Span::styled(
                    title,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("│ ", Style::default().fg(color)),
                Span::styled(body, Style::default().fg(TEXT)),
            ]),
            Line::from(Span::styled("╰─", Style::default().fg(color))),
        ];
    }

    if entry.label == "Thinking" {
        return vec![Line::from(vec![
            Span::styled("▎ ", Style::default().fg(MUTED)),
            Span::styled(
                "Thinking: ",
                Style::default().fg(HELP).add_modifier(Modifier::BOLD),
            ),
            Span::styled(truncate_text(&entry.body, 88), Style::default().fg(HELP)),
        ])];
    }

    vec![Line::from(vec![
        Span::styled("• ", Style::default().fg(color)),
        Span::styled(
            entry.label.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(": ", Style::default().fg(MUTED)),
        Span::styled(truncate_text(&entry.body, 72), Style::default().fg(TEXT)),
    ])]
}

fn is_tool_activity(label: &str) -> bool {
    label.starts_with("Tool ") || label == "Tool complete"
}

fn footer_widget(state: &AppState) -> Paragraph<'_> {
    let prompt = if state.ui.prompt_input.is_empty() {
        state.persistent.config.tui.prompt_placeholder.clone()
    } else {
        sliding_prompt_display(&state.ui.prompt_input, 180)
    };
    let mut footer_line = vec![Span::styled(
        state.ui.footer_help.clone(),
        Style::default().fg(MUTED),
    )];
    let status = status_summary(state);
    if let Some(notice) = &state.ui.footer_notice {
        if !notice.text.eq_ignore_ascii_case(&status) {
            footer_line.push(Span::raw("  "));
            footer_line.push(Span::styled("•", Style::default().fg(MUTED)));
            footer_line.push(Span::raw(" "));
            footer_line.push(Span::styled(
                notice.text.clone(),
                Style::default()
                    .fg(tone_color(notice.tone))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    footer_line.push(Span::raw("  "));
    footer_line.push(Span::styled(
        status,
        Style::default()
            .fg(status_color(state))
            .add_modifier(Modifier::BOLD),
    ));

    Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled(
                if matches!(state.ui.input_mode, InputMode::Prompt) {
                    "PROMPT"
                } else {
                    "COMMAND"
                },
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                prompt,
                Style::default().fg(if state.ui.prompt_input.is_empty() {
                    MUTED
                } else {
                    TEXT
                }),
            ),
        ]),
        Line::from(footer_line),
    ]))
    .block(panel_block("Input"))
    .wrap(Wrap { trim: true })
}

fn constrained_footer_widget(state: &AppState, width: u16) -> Paragraph<'_> {
    let status = status_summary(state);
    let prompt_width = width.saturating_sub(status.len() as u16 + 12) as usize;
    let prompt = if state.ui.prompt_input.is_empty() {
        truncate_display(
            &state.persistent.config.tui.prompt_placeholder,
            prompt_width,
        )
    } else {
        sliding_prompt_display(&state.ui.prompt_input, prompt_width)
    };

    Paragraph::new(Text::from(vec![Line::from(vec![
        Span::styled(
            if matches!(state.ui.input_mode, InputMode::Prompt) {
                "PROMPT "
            } else {
                "CMD "
            },
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            prompt,
            Style::default().fg(if state.ui.prompt_input.is_empty() {
                MUTED
            } else {
                TEXT
            }),
        ),
        Span::raw(" "),
        Span::styled(
            status,
            Style::default()
                .fg(status_color(state))
                .add_modifier(Modifier::BOLD),
        ),
    ])]))
    .style(Style::default().bg(PANEL_ALT))
    .wrap(Wrap { trim: true })
}

fn status_summary(state: &AppState) -> String {
    if state.session.running {
        format!(
            "step {} of {}",
            state.session.current_iteration.max(1),
            state.persistent.behavior.max_iterations
        )
    } else if state.session.error.is_some() {
        "run failed".to_string()
    } else if state.session.final_message.is_some() {
        "completed".to_string()
    } else {
        "idle".to_string()
    }
}

fn progress_indicator(state: &AppState) -> String {
    if !state.session.running {
        return state.session.status.clone();
    }

    let max = state.persistent.behavior.max_iterations.max(1);
    let step = state.session.current_iteration.max(1).min(max);
    let filled = (step * 10).div_ceil(max).min(10);
    format!(
        "[{}{}] step {}/{} - {}",
        "#".repeat(filled),
        "-".repeat(10 - filled),
        step,
        max,
        state.session.status
    )
}

fn status_color(state: &AppState) -> Color {
    if state.session.running {
        ACCENT
    } else if state.session.error.is_some() {
        ERROR
    } else if state.session.final_message.is_some() {
        SUCCESS
    } else {
        HELP
    }
}

fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Info => ACCENT,
        Tone::Success => SUCCESS,
        Tone::Warning => WARN,
        Tone::Error => ERROR,
    }
}

fn panel_tabs(state: &AppState) -> Tabs<'_> {
    let titles = ActivePanel::all()
        .iter()
        .map(|panel| Line::from(panel.title()))
        .collect::<Vec<_>>();
    let index = ActivePanel::all()
        .iter()
        .position(|panel| *panel == state.ui.active_panel)
        .unwrap_or(0);

    Tabs::new(titles)
        .select(index)
        .block(panel_block("Panels"))
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .style(Style::default().fg(MUTED))
}

fn panel_widget(state: &AppState) -> Paragraph<'_> {
    match state.ui.active_panel {
        ActivePanel::Session => session_summary_widget(state),
        ActivePanel::Mcp => mcp_widget(&state.persistent.mcp_servers, state.ui.selected_mcp),
        ActivePanel::Skills => skills_widget(
            &state.persistent.skills,
            state.ui.selected_skill,
            state.persistent.skills_root.display().to_string(),
        ),
        ActivePanel::Behavior => behavior_widget(state),
    }
}

fn session_summary_widget(state: &AppState) -> Paragraph<'_> {
    Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled("query ", Style::default().fg(MUTED)),
            Span::styled(
                truncate_display(&state.session.active_query, 120),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("responses ", Style::default().fg(MUTED)),
            Span::styled(
                state
                    .session
                    .transcript
                    .iter()
                    .filter(|entry| entry.role == "Assistant")
                    .count()
                    .to_string(),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("reasoning chars ", Style::default().fg(MUTED)),
            Span::styled(
                state.session.reasoning.chars().count().to_string(),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("context ", Style::default().fg(MUTED)),
            Span::styled(context_usage_summary(state), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("tokens/sec ", Style::default().fg(MUTED)),
            Span::styled(tokens_per_second_summary(state), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("cache ", Style::default().fg(MUTED)),
            Span::styled(cache_hit_summary(state), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("hud ", Style::default().fg(MUTED)),
            Span::styled(telemetry_hud_summary(state), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("auto compaction ", Style::default().fg(MUTED)),
            Span::styled(compaction_summary(state), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("spend ", Style::default().fg(MUTED)),
            Span::styled(cost_summary(state), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("needs rebuild ", Style::default().fg(MUTED)),
            Span::styled(
                state.persistent.needs_rebuild.to_string(),
                Style::default().fg(TEXT),
            ),
        ]),
    ]))
    .block(panel_block("Session"))
    .wrap(Wrap { trim: true })
}

fn context_usage_summary(state: &AppState) -> String {
    let telemetry = &state.session.telemetry;
    if !state.persistent.config.telemetry.enabled {
        return "disabled".to_string();
    }
    if telemetry.context_window == 0 || telemetry.total_tokens == 0 {
        return "waiting for token usage".to_string();
    }

    let used = telemetry.total_tokens;
    let window = telemetry.context_window;
    let percent = (used as f64 / window as f64 * 100.0).min(999.9);
    let remaining = 100.0_f64 - percent.min(100.0);
    let source = if telemetry.estimated { " est" } else { "" };
    format!("{used}/{window} tokens ({percent:.1}% full, {remaining:.1}% before full){source}")
}

fn tokens_per_second_summary(state: &AppState) -> String {
    let telemetry = &state.session.telemetry;
    if !state.persistent.config.telemetry.enabled {
        return "disabled".to_string();
    }
    match telemetry.tokens_per_second {
        Some(tps) => format!("{tps:.1} tok/s"),
        None => "waiting".to_string(),
    }
}

fn cache_hit_summary(state: &AppState) -> String {
    let telemetry = &state.session.telemetry;
    if !state.persistent.config.telemetry.enabled {
        return "disabled".to_string();
    }
    if telemetry.prompt_tokens == 0 {
        return "waiting".to_string();
    }
    let cached = telemetry.cached_prompt_tokens;
    let total_prompt = telemetry.prompt_tokens;
    let pct = (cached as f64 / total_prompt as f64 * 100.0).min(100.0);
    format!("{cached} tok ({pct:.1}%)")
}

fn compaction_summary(state: &AppState) -> String {
    let telemetry = &state.session.telemetry;
    if !state.persistent.config.telemetry.enabled {
        return "disabled".to_string();
    }
    if telemetry.context_window == 0 {
        return "waiting".to_string();
    }
    if telemetry.compacted {
        "applied for latest request".to_string()
    } else {
        "not needed".to_string()
    }
}

fn telemetry_hud_summary(state: &AppState) -> String {
    let telemetry = &state.session.telemetry;
    if !state.persistent.config.telemetry.enabled {
        return "disabled".to_string();
    }
    if telemetry.total_tokens == 0 {
        return "waiting for live metrics".to_string();
    }

    let context_pct = telemetry.context_window_usage_pct.unwrap_or_else(|| {
        if telemetry.context_window == 0 {
            0.0
        } else {
            telemetry.total_tokens as f64 / telemetry.context_window as f64 * 100.0
        }
    });
    format!(
        "{} tok · {context_pct:.1}% ctx · turn {} · {}",
        compact_token_count(telemetry.total_tokens),
        telemetry.turns_completed,
        cost_summary(state),
    )
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

fn cost_summary(state: &AppState) -> String {
    let settings = &state.persistent.config.telemetry;
    if !settings.enabled {
        return "disabled".to_string();
    }
    let cost = state.session.telemetry.total_cost;
    let rates_unset =
        settings.input_cost_per_million == 0.0 && settings.output_cost_per_million == 0.0;
    if rates_unset && cost <= 0.0 {
        return format!("{} rates not configured", settings.currency);
    }
    format!("{}{:.4}", currency_symbol(&settings.currency), cost)
}

fn currency_symbol(currency: &str) -> &str {
    match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        other => other,
    }
}

fn render_session_compact_widget(frame: &mut Frame<'_>, state: &AppState, area: Rect) {
    let block = panel_block("Session");
    let inner = block.inner(area);
    let body_width = usize::from(inner.width);

    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Conversation",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    for entry in state.session.transcript.iter().rev().take(4).rev() {
        lines.extend(turn_lines(entry, inner.width));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Reasoning",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    let reasoning = truncate_text(
        if state.session.reasoning.is_empty() {
            "Waiting for reasoning..."
        } else {
            &state.session.reasoning
        },
        300,
    );
    for wrapped in wrap_line(Line::from(reasoning), body_width) {
        lines.push(wrapped);
    }

    let max_scroll = max_wrapped_scroll(&lines, inner.width, inner.height);
    let scroll = if state.follow_conversation_tail() {
        max_scroll
    } else {
        state.conversation_scroll().min(max_scroll)
    };

    let widget = Paragraph::new(Text::from(lines))
        .block(block)
        .scroll((scroll, 0));
    frame.render_widget(widget, area);
}

fn max_wrapped_scroll(lines: &[Line<'_>], width: u16, height: u16) -> u16 {
    if width == 0 || height == 0 {
        return 0;
    }

    let available_width = usize::from(width.max(1));
    let total_rows = lines
        .iter()
        .map(|line| {
            let width = line.width();
            width.max(1).div_ceil(available_width)
        })
        .sum::<usize>();
    total_rows
        .saturating_sub(usize::from(height))
        .min(usize::from(u16::MAX)) as u16
}

fn mcp_widget(servers: &[McpServerItem], selected: usize) -> Paragraph<'_> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "a",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" add  ", Style::default().fg(MUTED)),
            Span::styled(
                "d",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" remove", Style::default().fg(MUTED)),
        ]),
        Line::from(""),
    ];

    if servers.is_empty() {
        lines.push(Line::from(Span::styled(
            "No MCP servers configured.",
            Style::default().fg(MUTED),
        )));
    } else {
        for (index, server) in servers.iter().enumerate() {
            lines.push(mcp_line(server, index == selected));
            if index == selected {
                lines.push(Line::from(vec![
                    Span::styled("    endpoint ", Style::default().fg(MUTED)),
                    Span::styled(
                        truncate_display(&server.endpoint, 96),
                        Style::default().fg(TEXT),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        if server.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        Style::default().fg(if server.enabled { SUCCESS } else { WARN }),
                    ),
                ]));
            }
        }
    }

    Paragraph::new(Text::from(lines))
        .block(panel_block("MCP Servers"))
        .wrap(Wrap { trim: true })
}

fn skills_widget(skills: &[SkillItem], selected: usize, skills_root: String) -> Paragraph<'_> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "n",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" new  ", Style::default().fg(MUTED)),
            Span::styled(
                "r",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" reload  ", Style::default().fg(MUTED)),
            Span::styled(
                "d",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" delete  ", Style::default().fg(MUTED)),
            Span::styled(
                "a",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" approve pending", Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            Span::styled("root ", Style::default().fg(MUTED)),
            Span::styled(
                truncate_display(&skills_root, 96),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(""),
    ];

    if skills.is_empty() {
        lines.push(Line::from(Span::styled(
            "No skills loaded.",
            Style::default().fg(MUTED),
        )));
    } else {
        for (index, skill) in skills.iter().enumerate() {
            lines.push(skill_line(skill, index == selected));
            if index == selected {
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default().fg(MUTED)),
                    Span::styled(
                        truncate_display(&skill.description, 140),
                        Style::default().fg(MUTED),
                    ),
                ]));
            }
        }
    }

    Paragraph::new(Text::from(lines))
        .block(panel_block("Skills"))
        .wrap(Wrap { trim: true })
}

fn behavior_widget(state: &AppState) -> Paragraph<'_> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "e",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" edit selected  ", Style::default().fg(MUTED)),
            Span::styled(
                "space",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" toggle bool", Style::default().fg(MUTED)),
        ]),
        Line::from(""),
    ];

    for (index, (key, value)) in state.behavior_rows().iter().enumerate() {
        let selected = index == state.ui.selected_behavior;
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "> " } else { "  " },
                Style::default().fg(if selected { ACCENT } else { MUTED }),
            ),
            Span::styled(
                format!("{key}: "),
                Style::default()
                    .fg(if selected { TEXT } else { MUTED })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(truncate_display(value, 120), Style::default().fg(TEXT)),
        ]));
    }

    Paragraph::new(Text::from(lines))
        .block(panel_block("Behavior"))
        .wrap(Wrap { trim: true })
}

fn render_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    help: &str,
    fields: &[crate::tui::forms::FormField],
    selected: usize,
) {
    let modal_area = centered_rect_percent(area, 88, 82, 92, 16);
    frame.render_widget(Clear, modal_area);

    let mut lines = vec![
        Line::from(Span::styled(help, Style::default().fg(MUTED))),
        Line::from(""),
    ];
    for (index, field) in fields.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                if index == selected { "> " } else { "  " },
                Style::default().fg(if index == selected { ACCENT } else { MUTED }),
            ),
            Span::styled(
                format!("{}: ", field.label),
                Style::default()
                    .fg(if index == selected { TEXT } else { MUTED })
                    .add_modifier(if index == selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(field.display_value(), Style::default().fg(TEXT)),
        ]));
    }

    let modal = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title(Span::styled(
                    truncate_display(title, 48),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .style(Style::default().bg(PANEL)),
        )
        .wrap(Wrap { trim: true });

    frame.render_widget(modal, modal_area);
}

fn mcp_line(server: &McpServerItem, selected: bool) -> Line<'static> {
    let status = if server.connected {
        "connected"
    } else {
        "offline"
    };
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default().fg(if selected { ACCENT } else { MUTED }),
        ),
        Span::styled(
            server.name.clone(),
            Style::default().fg(TEXT).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[{:?}]", server.transport).to_lowercase(),
            Style::default().fg(MUTED),
        ),
        Span::raw(" "),
        Span::styled(
            status,
            Style::default().fg(if server.connected { SUCCESS } else { WARN }),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{} tools", server.tool_count),
            Style::default().fg(MUTED),
        ),
    ])
}

fn skill_line(skill: &SkillItem, selected: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default().fg(if selected { ACCENT } else { MUTED }),
        ),
        Span::styled(
            skill.name.clone(),
            Style::default().fg(TEXT).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::raw(" "),
        Span::styled(skill.version.clone(), Style::default().fg(MUTED)),
        Span::raw(" "),
        Span::styled(
            if skill.pending {
                "pending"
            } else if skill.available {
                "ready"
            } else {
                "blocked"
            },
            Style::default().fg(if skill.pending {
                WARN
            } else if skill.available {
                SUCCESS
            } else {
                WARN
            }),
        ),
    ])
}

fn panel_block(title: &str) -> Block<'static> {
    Block::default()
        .title(Span::styled(title.to_string(), Style::default().fg(ACCENT)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .style(Style::default().bg(PANEL))
}

fn keycap(text: &str) -> Span<'static> {
    Span::styled(
        text.to_string(),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    )
}

fn label(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(HELP))
}

fn background_fill(area: Rect, color: Color) -> Paragraph<'static> {
    let blank_line = " ".repeat(area.width as usize);
    let lines = (0..area.height)
        .map(|_| Line::raw(blank_line.clone()))
        .collect::<Vec<_>>();

    Paragraph::new(Text::from(lines)).style(Style::default().bg(color))
}

fn render_message_body(text: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![Line::from("")];
    }

    let lines = text
        .split('\n')
        .map(|raw_line| raw_line.trim_end_matches('\r'))
        .collect::<Vec<_>>();
    let mut rendered = Vec::new();
    let mut table_mode = false;

    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            table_mode = false;
            rendered.push(Line::from(""));
            continue;
        }

        if is_table_alignment_row(line) {
            table_mode = true;
            continue;
        }

        let next_is_alignment = lines
            .get(index + 1)
            .is_some_and(|next| is_table_alignment_row(next));

        if looks_like_table_row(line) {
            rendered.push(render_table_row(line, next_is_alignment));
            table_mode = next_is_alignment || table_mode;
            continue;
        }

        if table_mode {
            if looks_like_table_row(line) {
                rendered.push(render_table_row(line, false));
                continue;
            }
            table_mode = false;
        }

        rendered.push(render_markdown_line(line));
    }

    rendered
}

fn render_reasoning_body(text: &str) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|raw_line| {
            let line = raw_line.trim_end_matches('\r');
            if line.trim().is_empty() {
                return Line::from(Span::styled("▎", Style::default().fg(MUTED)));
            }

            let mut spans = vec![Span::styled("▎ ", Style::default().fg(MUTED))];
            spans.extend(parse_inline_markdown(line, InlineStyle::Quote));
            Line::from(spans)
        })
        .collect()
}

fn render_markdown_line(line: &str) -> Line<'static> {
    if line.trim().is_empty() {
        return Line::from("");
    }

    if is_horizontal_rule(line) {
        return Line::from(Span::styled("─".repeat(24), Style::default().fg(MUTED)));
    }

    if let Some(rest) = heading_text(line) {
        return Line::from(vec![Span::styled(
            rest.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )]);
    }

    if let Some(rest) = blockquote_text(line) {
        let mut spans = vec![Span::styled("▎ ", Style::default().fg(MUTED))];
        spans.extend(parse_inline_markdown(rest, InlineStyle::Quote));
        return Line::from(spans);
    }

    if let Some((checked, body)) = task_list_item(line) {
        return Line::from(vec![
            Span::styled(
                if checked { "☑ " } else { "☐ " },
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(body.to_string(), Style::default().fg(TEXT)),
        ]);
    }

    if let Some((prefix, body)) = list_prefix(line) {
        let mut spans = vec![Span::styled(
            prefix.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )];
        spans.extend(parse_inline_markdown(body, InlineStyle::Default));
        return Line::from(spans);
    }

    Line::from(parse_inline_markdown(line, InlineStyle::Default))
}

fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let marker_count = trimmed.chars().take_while(|ch| *ch == '#').count();
    if marker_count == 0 {
        return None;
    }

    let remainder = trimmed[marker_count..].trim_start();
    if remainder.is_empty() {
        None
    } else {
        Some(remainder)
    }
}

fn list_prefix(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return Some(("• ", rest));
    }

    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        let prefix = &trimmed[..digits + 2];
        let body = &trimmed[digits + 2..];
        return Some((prefix, body));
    }

    None
}

#[derive(Clone, Copy)]
enum InlineStyle {
    Default,
    Quote,
    TableHeader,
    TableCell,
}

fn parse_inline_markdown(line: &str, style: InlineStyle) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut remaining = line;

    while !remaining.is_empty() {
        let bold_index = remaining.find("**").map(|index| (index, "**"));
        let italic_index = remaining.find('*').map(|index| (index, "*"));
        let code_index = remaining.find('`').map(|index| (index, "`"));
        let marker = [bold_index, italic_index, code_index]
            .into_iter()
            .flatten()
            .filter(|(index, token)| *token != "*" || !remaining[*index..].starts_with("**"))
            .min_by_key(|(index, _)| *index);

        let Some((index, token)) = marker else {
            spans.push(plain_span(remaining, style));
            break;
        };

        if index > 0 {
            spans.push(plain_span(&remaining[..index], style));
        }

        if token == "**" {
            let inner = &remaining[index + 2..];
            if let Some(end) = inner.find("**") {
                spans.push(Span::styled(
                    inner[..end].to_string(),
                    base_style(style).add_modifier(Modifier::BOLD),
                ));
                remaining = &inner[end + 2..];
                continue;
            }
        } else if token == "*" {
            let inner = &remaining[index + 1..];
            if let Some(end) = inner.find('*') {
                spans.push(Span::styled(
                    inner[..end].to_string(),
                    base_style(style).add_modifier(Modifier::ITALIC),
                ));
                remaining = &inner[end + 1..];
                continue;
            }
        } else {
            let inner = &remaining[index + 1..];
            if let Some(end) = inner.find('`') {
                spans.push(Span::styled(
                    inner[..end].to_string(),
                    Style::default()
                        .fg(ACCENT)
                        .bg(PANEL_ALT)
                        .add_modifier(Modifier::BOLD),
                ));
                remaining = &inner[end + 1..];
                continue;
            }
        }

        spans.push(plain_span(token, style));
        remaining = &remaining[index + token.len()..];
    }

    spans
}

fn plain_span(text: &str, style: InlineStyle) -> Span<'static> {
    Span::styled(text.to_string(), base_style(style))
}

fn base_style(style: InlineStyle) -> Style {
    match style {
        InlineStyle::Default => Style::default().fg(TEXT),
        InlineStyle::Quote => Style::default().fg(HELP),
        InlineStyle::TableHeader => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        InlineStyle::TableCell => Style::default().fg(TEXT),
    }
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|ch| ch == '-' || ch == '*' || ch == '_')
}

fn blockquote_text(line: &str) -> Option<&str> {
    line.trim_start().strip_prefix("> ").map(str::trim_start)
}

fn task_list_item(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("- [x] ")
        .or_else(|| trimmed.strip_prefix("* [x] "))
    {
        return Some((true, rest));
    }
    if let Some(rest) = trimmed
        .strip_prefix("- [ ] ")
        .or_else(|| trimmed.strip_prefix("* [ ] "))
    {
        return Some((false, rest));
    }
    None
}

fn looks_like_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.matches('|').count() >= 2 && !is_horizontal_rule(trimmed)
}

fn is_table_alignment_row(line: &str) -> bool {
    let trimmed = line.trim();
    looks_like_table_row(trimmed)
        && trimmed.trim_matches('|').split('|').all(|cell| {
            let cell = cell.trim();
            !cell.is_empty() && cell.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
        })
}

fn render_table_row(line: &str, header: bool) -> Line<'static> {
    let cells = line
        .trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect::<Vec<_>>();

    let mut spans = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(MUTED)));
        }
        spans.extend(parse_inline_markdown(
            cell,
            if header {
                InlineStyle::TableHeader
            } else {
                InlineStyle::TableCell
            },
        ));
    }

    Line::from(spans)
}

fn truncate_text(text: &str, max: usize) -> String {
    truncate_display(text, max)
}

pub(crate) fn sliding_prompt_display(text: &str, max_width: usize) -> String {
    let total_width = UnicodeWidthStr::width(text);
    if total_width <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let target_width = max_width.saturating_sub(3); // leave room for "..." prefix
    let mut current_width = 0;
    let mut rev_chars = Vec::new();

    for ch in text.chars().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > target_width {
            break;
        }
        current_width += ch_width;
        rev_chars.push(ch);
    }

    rev_chars.reverse();
    let tail: String = rev_chars.into_iter().collect();
    format!("...{tail}")
}

fn truncate_display(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let mut width = 0;
    let mut value = String::new();
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max_width - 3 {
            break;
        }
        width += ch_width;
        value.push(ch);
    }
    value.push_str("...");
    value
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    use kerux_core::config::AppConfig;

    use super::*;
    use crate::tui::state::{AppState, TelemetryState, ToolCallLine, TranscriptEntry, ViewMode};

    fn buffer_text(state: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    fn buffer(state: &AppState, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_rows(state: &AppState, width: u16, height: u16) -> Vec<String> {
        let buf = buffer(state, width, height);
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[test]
    fn landing_layout_renders_prompt_first_shell() {
        let state = AppState::new(AppConfig::default(), String::new(), false);
        let text = buffer_text(&state, 120, 36);
        assert!(text.contains("KERUX"));
        assert!(text.contains("prompt"));
    }

    #[test]
    fn compact_portrait_landing_wraps_controls() {
        let state = AppState::new(AppConfig::default(), String::new(), false);
        let text = buffer_text(&state, 40, 28);
        assert!(text.contains("quit"));
        assert!(text.contains("prompt"));
        assert!(text.contains("run"));
        assert!(text.contains("idle"));
    }

    #[test]
    fn compact_landscape_landing_keeps_controls_below_prompt() {
        let state = AppState::new(AppConfig::default(), String::new(), false);
        let text = buffer_text(&state, 72, 24);
        assert!(text.contains("panels"));
        assert!(text.contains("quit"));
        assert!(!text.contains("iter 0/"));
    }

    #[test]
    fn wide_landing_renders_single_idle_and_visible_helpers() {
        let state = AppState::new(AppConfig::default(), String::new(), false);
        let buffer = buffer(&state, 120, 36);
        let footer_row = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .find(|line| line.contains("idle") && line.contains("tab"))
            .unwrap();
        let idle_index = footer_row.find("idle").unwrap();
        let tab_index = footer_row.find("tab").unwrap();

        assert_eq!(footer_row.matches("idle").count(), 1);
        assert!(tab_index > idle_index);
        assert!(footer_row.contains("enter"));
    }

    #[test]
    fn landing_canvas_uses_black_background() {
        let state = AppState::new(AppConfig::default(), String::new(), false);
        let buffer = buffer(&state, 120, 36);
        assert!(buffer
            .content
            .iter()
            .all(|cell| cell.bg == BG || cell.bg == PANEL));
    }

    #[test]
    fn activity_panel_renders_run_failures() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.fail_run("OpenAI 401 unauthorized".to_string());

        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("Activity"));
        assert!(text.contains("Run failed: OpenAI 401 unauthorized"));
    }

    #[test]
    fn wide_workspace_renders_all_major_panels() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.session.title = "Fix mouse scrolling".to_string();
        state.set_layout_for_width(160);
        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("Conversation"));
        assert!(text.contains("Reasoning"));
        assert!(text.contains("Activity"));
        assert!(text.contains("Fix mouse scrolling"));
    }

    #[test]
    fn desktop_workspace_uses_split_layout_at_120_by_30() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(120);

        let text = buffer_text(&state, 120, 30);

        assert!(text.contains("Conversation"));
        assert!(text.contains("Reasoning"));
        assert!(text.contains("Activity"));
        assert!(!text.contains("Panels"));
    }

    #[test]
    fn medium_workspace_renders_tabs() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(110);
        let text = buffer_text(&state, 110, 36);
        assert!(text.contains("Panels"));
        assert!(text.contains("Session"));
    }

    #[test]
    fn compact_workspace_renders_single_panel_mode() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(80);
        let text = buffer_text(&state, 80, 32);
        assert!(text.contains("Session"));
        assert!(text.contains("Input"));
        assert!(text.contains("idle"));
        assert!(!text.contains("iter 0/"));
    }

    #[test]
    fn constrained_workspace_prioritizes_conversation_at_40_by_15() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(40);

        let text = buffer_text(&state, 40, 15);

        assert!(text.contains("Conversation"));
        assert!(text.contains("PROMPT"));
        assert!(text.contains("idle"));
        assert!(!text.contains("Panels"));
        assert!(!text.contains("Reasoning"));
        assert!(!text.contains("Activity"));
    }

    #[test]
    fn constrained_workspace_shows_secondary_panel_as_popup() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.ui.active_panel = ActivePanel::Mcp;
        state.set_layout_for_width(40);

        let text = buffer_text(&state, 40, 15);

        assert!(text.contains("MCP"));
        assert!(text.contains("No MCP servers configured."));
        assert!(!text.contains("Panels"));
    }

    #[test]
    fn renderer_handles_extreme_small_viewports() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;

        for (width, height) in [(1, 1), (20, 8), (40, 15), (64, 19)] {
            state.set_layout_for_width(width);
            let _ = buffer_text(&state, width, height);
        }
    }

    #[test]
    fn conversation_preserves_line_breaks() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.transcript.push(TranscriptEntry::new(
            "Assistant",
            "Line one\n\nLine two\n- item",
        ));

        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("Line one"));
        assert!(text.contains("Line two"));
        assert!(text.contains("• item"));
    }

    #[test]
    fn conversation_strips_common_markdown_markers() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.transcript.push(TranscriptEntry::new(
            "Assistant",
            "## Heading\n**Bold** and `code`",
        ));

        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("Heading"));
        assert!(text.contains("Bold and code"));
        assert!(!text.contains("## Heading"));
        assert!(!text.contains("**Bold**"));
        assert!(!text.contains("`code`"));
    }

    #[test]
    fn conversation_scroll_reveals_later_lines() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.transcript.push(TranscriptEntry::new(
            "Assistant",
            "entry 01\nentry 02\nentry 03\nentry 04\nentry 05\nentry 06\nentry 07\nentry 08\nentry 09\nentry 10\nentry 11\nentry 12",
        ));
        state.scroll_conversation_down(6);

        let text = buffer_text(&state, 160, 22);
        assert!(text.contains("entry 08"));
        assert!(!text.contains("entry 01"));
    }

    #[test]
    fn conversation_formats_tables_quotes_and_rules() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.transcript.push(TranscriptEntry::new(
            "Assistant",
            "---\n> quoted line\n| Tool | Use |\n| --- | --- |\n| echo | test |\n- [x] done",
        ));

        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("quoted line"));
        assert!(text.contains("Tool │ Use"));
        assert!(text.contains("echo │ test"));
        assert!(text.contains("☑ done"));
        assert!(!text.contains("| --- | --- |"));
    }

    #[test]
    fn reasoning_renders_as_blockquote() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.reasoning = "I should inspect files.\nThen patch the renderer.".to_string();

        let text = buffer_text(&state, 160, 40);

        assert!(text.contains("▎ I should inspect files."));
        assert!(text.contains("▎ Then patch the renderer."));
    }

    #[test]
    fn activity_renders_tool_calls_as_blocks() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.activity.clear();
        state.push_activity("Thinking", "Choosing a tool.", Tone::Info);
        state.push_activity("Tool file_read", r#"{"path":"README.md"}"#, Tone::Warning);

        let text = buffer_text(&state, 160, 40);

        assert!(text.contains("▎ Thinking: Choosing a tool."));
        assert!(text.contains("╭─ Tool file_read"));
        assert!(text.contains("│ {\"path\":\"README.md\"}"));
        assert!(text.contains("╰─"));
    }

    #[test]
    fn live_telemetry_hud_renders_metrics_and_indicators() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.telemetry = TelemetryState {
            prompt_tokens: 1200,
            completion_tokens: 300,
            total_tokens: 1500,
            context_window: 12400,
            compacted: false,
            estimated: false,
            total_cost: 0.0045,
            tokens_per_second: Some(42.5),
            turns_completed: 3,
            context_window_usage_pct: Some(12.5),
            cached_prompt_tokens: 400,
        };

        let text = buffer_text(&state, 160, 40);

        assert!(text.contains("42.5 tok/s"));
        assert!(text.contains("cache 400"));
        assert!(text.contains("$0.0045"));
        assert!(text.contains("1.5k tok"));
        assert!(text.contains("12.5% ctx"));
    }

    fn entry_with(role: &'static str, content: &str, timestamp: &str) -> TranscriptEntry {
        let mut entry = TranscriptEntry::new(role, content);
        entry.timestamp = Some(timestamp.to_string());
        entry
    }

    #[test]
    fn test_sliding_prompt_display() {
        assert_eq!(sliding_prompt_display("short text", 20), "short text");
        assert_eq!(
            sliding_prompt_display("abcdefghijklmnopqrstuvwxyz", 10),
            "...tuvwxyz"
        );
        assert_eq!(sliding_prompt_display("hello world", 3), "...");
    }

    #[test]
    fn conversation_renders_glyph_gutter_with_timestamps() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.transcript = vec![
            entry_with("User", "what's the crumb", "14:32"),
            entry_with("Assistant", "here it is — gnawed clean", "14:33"),
        ];

        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("❯ 14:32  what's the crumb"));
        assert!(text.contains("✦ 14:33  here it is — gnawed clean"));
        assert!(!text.contains("User"));
        assert!(!text.contains("Assistant"));
    }

    #[test]
    fn conversation_renders_tool_sublines_under_turn() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        let mut assistant = entry_with("Assistant", "here it is — gnawed clean", "14:33");
        assistant.tools = vec![
            ToolCallLine {
                name: "read_file".to_string(),
                ok: true,
                duration_secs: Some(0.2),
            },
            ToolCallLine {
                name: "shell".to_string(),
                ok: false,
                duration_secs: None,
            },
        ];
        state.session.transcript = vec![entry_with("User", "what's the crumb", "14:32"), assistant];

        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("┊ tool: read_file ✓ 0.2s"));
        assert!(text.contains("┊ tool: shell ✗"));
    }

    #[test]
    fn conversation_renders_turn_separator_between_turns_only() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.transcript = vec![
            entry_with("User", "first turn", "14:32"),
            entry_with("Assistant", "first answer", "14:33"),
            entry_with("User", "second turn", "14:40"),
            entry_with("Assistant", "second answer", "14:41"),
        ];

        let rows = buffer_rows(&state, 160, 40);
        let sep_count = rows
            .iter()
            .filter(|row| {
                let mut chars = row.chars();
                chars.next() == Some('│') && chars.next() == Some('┄')
            })
            .count();
        assert_eq!(sep_count, 1);
    }

    #[test]
    fn conversation_wraps_body_at_consistent_indent() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi \
                    omicron pi rho sigma tau upsilon phi chi psi omega";
        state.session.transcript = vec![entry_with("User", long, "14:32")];

        for width in [40u16, 80, 120] {
            state.set_layout_for_width(width);
            let rows = buffer_rows(&state, width, 30);
            let continuations = rows
                .iter()
                .filter(|row| {
                    let chars: Vec<char> = row.chars().collect();
                    chars.len() > 10
                        && chars[0] == '│'
                        && chars[1..10].iter().all(|ch| *ch == ' ')
                        && chars[10] != ' '
                })
                .count();
            assert!(
                continuations > 0,
                "width {} should wrap with a 9-column body indent",
                width
            );
        }
    }

    #[test]
    fn streaming_turn_shows_glyph_before_body() {
        let mut state = AppState::new(AppConfig::default(), "hello".to_string(), true);
        state.ui.view = ViewMode::Workspace;
        state.set_layout_for_width(160);
        state.session.transcript = vec![entry_with("User", "what's the crumb", "14:32")];
        state.session.running = true;
        state.session.streaming_response = "partial output".to_string();

        let text = buffer_text(&state, 160, 40);
        assert!(text.contains("✦"));
        assert!(text.contains("partial output"));
        assert!(!text.contains("Assistant"));
    }
}
