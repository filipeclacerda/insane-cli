//! ratatui rendering (SPEC-UX B2/B3): header, scrollable conversation,
//! multi-line input, status bar, and a centered confirmation modal.

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use super::app::{AppState, MsgBlock, PendingSessionPicker, ToolStatus};
use super::format::{diff_lines_for_modal, wrap_text, DiffLineKind};
use super::theme;

/// Input box height in lines, growing with content up to this cap
/// (SPEC-UX B2).
const MAX_INPUT_LINES: u16 = 5;

pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    frame.render_widget(ratatui::widgets::Clear, area);
    if let Some(pending) = &state.confirm {
        let max_height = area.height.saturating_sub(4).max(1);
        let min_height = 8.min(max_height);
        let approval_height = (area.height.saturating_mul(45) / 100).clamp(min_height, max_height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(approval_height),
                Constraint::Length(1),
            ])
            .split(area);
        draw_header(frame, chunks[0], state);
        draw_conversation(frame, chunks[1], state);
        draw_approval_panel(frame, chunks[2], pending);
        draw_status(frame, chunks[3], state);
        return;
    }
    if let Some(picker) = &state.session_picker {
        let max_height = area.height.saturating_sub(4).max(1);
        let min_height = 7.min(max_height);
        let picker_height = (picker.sessions.len() as u16 + 5).clamp(min_height, max_height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(picker_height),
                Constraint::Length(1),
            ])
            .split(area);
        draw_header(frame, chunks[0], state);
        draw_conversation(frame, chunks[1], state);
        draw_session_picker(frame, chunks[2], picker);
        draw_status(frame, chunks[3], state);
        return;
    }
    let input_width = area.width.saturating_sub(2).max(1) as usize;
    let (visual_input, _) = input_layout(&state.input, state.cursor, input_width);
    let input_lines = visual_input.len().max(1).min(MAX_INPUT_LINES as usize) as u16;
    let suggestions = state.suggestions();
    let suggestion_height = if suggestions.is_empty() {
        0
    } else {
        suggestions.len().min(6) as u16 + 2
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                 // header
            Constraint::Min(3),                    // conversation
            Constraint::Length(suggestion_height), // slash palette
            Constraint::Length(input_lines + 2),   // input (border top/bottom)
            Constraint::Length(1),                 // status
        ])
        .split(area);

    draw_header(frame, chunks[0], state);
    draw_conversation(frame, chunks[1], state);
    if suggestion_height > 0 {
        draw_suggestions(frame, chunks[2], state);
    }
    draw_input(frame, chunks[3], state);
    draw_status(frame, chunks[4], state);
}

fn draw_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let line = Line::from(vec![
        Span::styled(" insane-cli ", theme::brand()),
        Span::styled(format!(" {} ", state.mode.label()), theme::mode(state.mode)),
        Span::styled(
            format!("  {}/{}  ", state.provider, state.model),
            theme::header(),
        ),
        Span::styled(format!(" {} ", state.cwd_display), theme::header_dim()),
    ]);
    let p = Paragraph::new(line).style(theme::header());
    frame.render_widget(p, area);
}

#[derive(Clone)]
struct StyledSegment {
    text: String,
    style: Style,
}

#[derive(Clone)]
struct StyledChar {
    ch: char,
    style: Style,
}

fn block_lines(msg: &MsgBlock, width: usize, show_thinking: bool) -> Vec<Line<'static>> {
    match msg {
        MsgBlock::User(text) => wrap_text(&format!("you: {text}"), width)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, theme::user())))
            .collect(),
        MsgBlock::Assistant(text) if text.is_empty() => Vec::new(),
        MsgBlock::Assistant(text) => assistant_lines(text, width),
        MsgBlock::Thinking(text) if text.trim().is_empty() => Vec::new(),
        MsgBlock::Thinking(_) if !show_thinking => thinking_placeholder_lines(width),
        MsgBlock::Thinking(text) => thinking_lines(text, width),
        MsgBlock::Tool { status, line } => {
            let (marker, style) = match status {
                ToolStatus::Running => ("\u{25c7}", theme::tool_running()),
                ToolStatus::Success => ("\u{2514}", theme::success()),
                ToolStatus::Failed => ("\u{2514}", theme::danger()),
            };
            wrap_text(line, width)
                .into_iter()
                .enumerate()
                .map(|(idx, l)| {
                    let prefix = if idx == 0 { marker } else { " " };
                    Line::from(Span::styled(format!("  {prefix} {l}"), style))
                })
                .collect()
        }
        MsgBlock::Warn(text) => wrap_text(&format!("! {text}"), width)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, theme::warning())))
            .collect(),
        MsgBlock::TurnSummary(text) => wrap_text(text, width)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, theme::subtle())))
            .collect(),
    }
}

fn assistant_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for (idx, raw_line) in text.split('\n').enumerate() {
        let mut segments = Vec::new();
        if idx == 0 {
            segments.push(StyledSegment {
                text: "insane: ".to_string(),
                style: theme::assistant(),
            });
        }
        segments.extend(parse_assistant_markup(raw_line));
        lines.extend(wrap_styled_segments(segments, width));
    }
    lines
}

fn thinking_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for (idx, raw_line) in text.split('\n').enumerate() {
        let mut segments = Vec::new();
        if idx == 0 {
            segments.push(StyledSegment {
                text: "thinking: ".to_string(),
                style: theme::thinking_label(),
            });
        }
        segments.push(StyledSegment {
            text: raw_line.to_string(),
            style: theme::thinking(),
        });
        lines.extend(wrap_styled_segments(segments, width));
    }
    lines
}

fn thinking_placeholder_lines(width: usize) -> Vec<Line<'static>> {
    wrap_styled_segments(
        vec![StyledSegment {
            text: "thinking...".to_string(),
            style: theme::thinking_label(),
        }],
        width.max(1),
    )
}

fn parse_assistant_markup(line: &str) -> Vec<StyledSegment> {
    let heading_marks = line.chars().take_while(|c| *c == '#').count();
    if heading_marks >= 2 {
        let stripped = line[heading_marks..].trim_start();
        return parse_bold_segments(
            stripped,
            theme::assistant_markdown(),
            theme::assistant_markdown(),
        );
    }
    parse_bold_segments(line, theme::assistant(), theme::assistant_markdown())
}

fn parse_bold_segments(text: &str, normal: Style, highlight: Style) -> Vec<StyledSegment> {
    let mut segments = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("**") {
        let after_open = start + 2;
        let Some(end_rel) = rest[after_open..].find("**") else {
            push_segment(&mut segments, rest, normal);
            return segments;
        };
        push_segment(&mut segments, &rest[..start], normal);
        let end = after_open + end_rel;
        push_segment(&mut segments, &rest[after_open..end], highlight);
        rest = &rest[end + 2..];
    }
    push_segment(&mut segments, rest, normal);
    segments
}

fn push_segment(segments: &mut Vec<StyledSegment>, text: &str, style: Style) {
    if !text.is_empty() {
        segments.push(StyledSegment {
            text: text.to_string(),
            style,
        });
    }
}

fn wrap_styled_segments(segments: Vec<StyledSegment>, width: usize) -> Vec<Line<'static>> {
    let mut chars = Vec::new();
    for segment in segments {
        for ch in segment.text.chars() {
            chars.push(StyledChar {
                ch,
                style: segment.style,
            });
        }
    }
    wrap_styled_chars(chars, width)
}

fn wrap_styled_chars(chars: Vec<StyledChar>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if chars.is_empty() {
        return vec![Line::from("")];
    }

    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut separator_style = None;
    for styled in chars {
        if styled.ch == ' ' {
            words.push((separator_style.take(), word));
            word = Vec::new();
            separator_style = Some(styled.style);
        } else {
            word.push(styled);
        }
    }
    words.push((separator_style, word));

    let mut wrapped = Vec::new();
    let mut current = Vec::new();
    for (separator_style, mut word) in words {
        loop {
            let candidate_len = if current.is_empty() {
                word.len()
            } else {
                current.len() + 1 + word.len()
            };
            if candidate_len <= width {
                if !current.is_empty() {
                    current.push(StyledChar {
                        ch: ' ',
                        style: separator_style.unwrap_or_else(theme::assistant),
                    });
                }
                current.append(&mut word);
                break;
            }

            if current.is_empty() {
                let tail = word.split_off(width.min(word.len()));
                wrapped.push(word);
                if tail.is_empty() {
                    break;
                }
                word = tail;
                continue;
            }

            wrapped.push(std::mem::take(&mut current));
        }
    }
    wrapped.push(current);
    wrapped.into_iter().map(line_from_styled_chars).collect()
}

fn line_from_styled_chars(chars: Vec<StyledChar>) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();

    for styled in chars {
        match current_style {
            Some(style) if style == styled.style => current_text.push(styled.ch),
            Some(style) => {
                spans.push(Span::styled(std::mem::take(&mut current_text), style));
                current_style = Some(styled.style);
                current_text.push(styled.ch);
            }
            None => {
                current_style = Some(styled.style);
                current_text.push(styled.ch);
            }
        }
    }

    if let Some(style) = current_style {
        spans.push(Span::styled(current_text, style));
    }
    Line::from(spans)
}

fn draw_conversation(frame: &mut Frame, area: Rect, state: &AppState) {
    let width = area.width.saturating_sub(2).max(1) as usize;
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    for msg in &state.messages {
        all_lines.extend(block_lines(msg, width, state.show_thinking));
    }

    let viewport_height = area.height.saturating_sub(2) as usize;
    let total = all_lines.len();
    let scroll = super::format::clamp_scroll(total, viewport_height, state.scroll);
    // `scroll` is "lines scrolled up from the bottom".
    let bottom_excluded = scroll;
    let end = total.saturating_sub(bottom_excluded);
    let start = end.saturating_sub(viewport_height);
    let visible: Vec<Line<'static>> = all_lines[start..end].to_vec();

    let title = if scroll > 0 {
        "conversation (scrolled)"
    } else {
        "conversation"
    };
    let p = Paragraph::new(visible)
        .block(theme::block(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

fn draw_suggestions(frame: &mut Frame, area: Rect, state: &AppState) {
    let suggestions = state.suggestions();
    let selected = state
        .suggestion_idx
        .min(suggestions.len().saturating_sub(1));
    let start = selected.saturating_sub(5);
    let lines: Vec<Line<'static>> = suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(6)
        .map(|(idx, suggestion)| {
            let style = if idx == selected {
                theme::selected()
            } else {
                theme::assistant()
            };
            Line::from(vec![
                Span::styled(format!(" {:<32}", suggestion.label), style),
                Span::styled(
                    format!("  {}", suggestion.description),
                    if idx == selected {
                        style
                    } else {
                        theme::subtle()
                    },
                ),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .block(theme::block(
                " commands / @files  \u{2191}/\u{2193} select  Tab complete ",
            )),
        area,
    );
}

fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let width = area.width.saturating_sub(2).max(1) as usize;
    let (lines, (cursor_row, cursor_col)) = input_layout(&state.input, state.cursor, width);
    let inner_height = area.height.saturating_sub(2).max(1) as usize;
    let start = cursor_row.saturating_sub(inner_height.saturating_sub(1));
    let end = (start + inner_height).min(lines.len());
    let text = lines[start..end].join("\n");
    let title = if state.processing {
        "input (turn in progress -- Ctrl+C cancel, Ctrl+O toggle thinking)"
    } else {
        "input  Enter send  Alt+Enter newline  Shift+Tab mode  Ctrl+O toggle thinking"
    };
    let p = Paragraph::new(text)
        .style(theme::assistant())
        .block(theme::block(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);

    if !state.processing && state.confirm.is_none() && state.session_picker.is_none() {
        let row = cursor_row.saturating_sub(start) as u16;
        let col = (cursor_col as u16).min(area.width.saturating_sub(2));
        frame.set_cursor_position(Position::new(area.x + 1 + col, area.y + 1 + row));
    }
}

/// Hard-wraps input so rendered text and cursor coordinates use exactly the
/// same rules. `wrap_text` is word-oriented and therefore unsuitable for an
/// editor cursor in the middle of a word.
fn input_layout(input: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let mut lines = vec![String::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut cursor_pos = (0usize, 0usize);

    for (idx, ch) in input.chars().enumerate() {
        if idx == cursor {
            cursor_pos = (row, col);
        }
        if ch == '\n' {
            lines.push(String::new());
            row += 1;
            col = 0;
            continue;
        }
        if col == width {
            lines.push(String::new());
            row += 1;
            col = 0;
            if idx == cursor {
                cursor_pos = (row, col);
            }
        }
        lines[row].push(ch);
        col += 1;
    }
    if cursor >= input.chars().count() {
        if col == width {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        cursor_pos = (row, col);
    }
    (lines, cursor_pos)
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut spans = Vec::new();
    if let Some(line) = &state.status.spinner_line {
        spans.push(Span::styled(line.clone(), theme::muted()));
    }
    if !spans.is_empty() {
        spans.push(Span::styled("  \u{b7}  ", theme::subtle()));
    }
    spans.push(Span::styled("mode ", theme::subtle()));
    spans.push(Span::styled(
        state.mode.label(),
        theme::mode_text(state.mode),
    ));
    if let (Some(used), Some(cap)) = (state.status.rate_used, state.status.rate_capacity) {
        push_status_part(&mut spans, format!("rate {used}/{cap}"));
    } else if let Some(used) = state.status.rate_used {
        push_status_part(&mut spans, format!("rate {used}/∞"));
    }
    if state.status.min_interval_ms > 0 {
        push_status_part(
            &mut spans,
            format!("pace {}ms", state.status.min_interval_ms),
        );
    }
    if state.status.next_request_ms > 0 {
        push_status_part(
            &mut spans,
            format!("next {}ms", state.status.next_request_ms),
        );
    }
    let token_text = match (state.status.tokens_this_turn, state.status.tokens_total) {
        (Some(tok), total) if total > 0 => format!(
            "tok {} / total {}",
            crate::agent::format_token_count(tok as u64),
            crate::agent::format_token_count(total)
        ),
        (Some(tok), _) => format!("tok {}", crate::agent::format_token_count(tok as u64)),
        (None, total) if total > 0 => {
            format!("tok -- / total {}", crate::agent::format_token_count(total))
        }
        (None, _) => "tok --".to_string(),
    };
    let command_hint =
        format!("{token_text}  Shift+Tab mode  Ctrl+O toggle thinking  Ctrl+C cancel/exit  /help");
    push_status_part(&mut spans, command_hint);
    let p = Paragraph::new(Line::from(spans)).style(theme::muted());
    frame.render_widget(p, area);
}

fn push_status_part(spans: &mut Vec<Span<'static>>, text: String) {
    spans.push(Span::styled("  \u{b7}  ", theme::subtle()));
    spans.push(Span::styled(text, theme::muted()));
}

fn draw_approval_panel(frame: &mut Frame, area: Rect, pending: &super::app::PendingConfirm) {
    let width = area.width.saturating_sub(4).max(1) as usize;
    let mut lines: Vec<Line<'static>> = wrap_text(&pending.req.prompt, width)
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                line,
                theme::assistant().add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    lines.push(Line::from(""));

    if let Some(details) = &pending.req.details {
        for line in wrap_text(details, width) {
            lines.push(Line::from(Span::styled(line, theme::warning())));
        }
        lines.push(Line::from(""));
    }

    if let Some(command) = &pending.req.command {
        for line in wrap_text(command, width) {
            lines.push(Line::from(Span::styled(
                format!("$ {line}"),
                theme::warning(),
            )));
        }
    }
    if let Some(diff) = &pending.req.diff {
        for (kind, line) in diff_lines_for_modal(diff) {
            let color = match kind {
                DiffLineKind::Add => theme::SUCCESS,
                DiffLineKind::Del => theme::DANGER,
                DiffLineKind::Meta => theme::ACCENT,
                DiffLineKind::Context => theme::MUTED,
            };
            for wrapped in wrap_text(line, width) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(color).bg(theme::BG),
                )));
            }
        }
    }

    let option_labels: Vec<&str> = if pending.option_count() == 2 {
        vec!["Allow once", "Deny"]
    } else if pending.req.tool == "run_command" {
        vec!["Allow once", "Allow this exact command for session", "Deny"]
    } else {
        vec!["Allow once", "Allow this tool for session", "Deny"]
    };

    let viewport_height = area.height.saturating_sub(option_labels.len() as u16 + 4) as usize;
    let total = lines.len();
    let scroll = super::format::clamp_scroll(total, viewport_height, pending.scroll);
    let start = scroll;
    let end = (start + viewport_height).min(total);
    let mut visible = lines[start..end].to_vec();
    visible.push(Line::from(Span::styled("─".repeat(width), theme::subtle())));
    for (idx, label) in option_labels.iter().enumerate() {
        let selected = idx == pending.selected;
        visible.push(Line::from(Span::styled(
            format!(" {} {label}", if selected { "›" } else { " " }),
            if selected {
                theme::selected()
            } else {
                theme::assistant()
            },
        )));
    }

    let p = Paragraph::new(visible)
        .style(theme::panel())
        .block(theme::block(format!(
            " approve: {}  ↑/↓ select  Enter confirm  Esc deny  PgUp/PgDn preview ",
            pending.req.tool
        )))
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

fn draw_session_picker(frame: &mut Frame, area: Rect, picker: &PendingSessionPicker) {
    let width = area.width.saturating_sub(4).max(1) as usize;
    let mut lines = vec![Line::from(Span::styled(
        "Escolha uma sessao para retomar",
        theme::assistant().add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::from(""));

    for (idx, session) in picker.sessions.iter().enumerate() {
        let selected = idx == picker.selected;
        let marker = if selected { "›" } else { " " };
        let text = format!(
            " {} {}. {} mensagens · {} · {}",
            marker,
            idx + 1,
            session.messages,
            session.model,
            session.preview
        );
        for wrapped in wrap_text(&text, width) {
            lines.push(Line::from(Span::styled(
                wrapped,
                if selected {
                    theme::selected()
                } else {
                    theme::assistant()
                },
            )));
        }
    }

    let p = Paragraph::new(lines)
        .style(theme::panel())
        .block(theme::block(
            " resume  ↑/↓ select  Enter resume  Esc close ",
        ))
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::{assistant_lines, block_lines, draw, input_layout};
    use crate::session_store::SessionSummary;
    use crate::tui::app::{AppState, MsgBlock, PendingConfirm, PendingSessionPicker};
    use crate::tui::theme;
    use crate::ui::ConfirmRequest;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use ratatui::Terminal;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn input_cursor_tracks_middle_and_hard_wrap() {
        let (lines, cursor) = input_layout("abcdef", 4, 3);
        assert_eq!(lines, vec!["abc", "def"]);
        assert_eq!(cursor, (1, 1));
    }

    #[test]
    fn input_cursor_tracks_newline() {
        let (_, cursor) = input_layout("ab\ncd", 3, 10);
        assert_eq!(cursor, (1, 0));
    }

    #[test]
    fn approval_is_rendered_in_bottom_layout() {
        let mut state = AppState::new("model".into(), ".".into(), ".".into());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        state.confirm = Some(PendingConfirm {
            req: ConfirmRequest {
                tool: "edit_file".into(),
                prompt: "Apply edit?".into(),
                details: None,
                diff: Some("-old\n+new".into()),
                command: None,
            },
            responder: tx,
            scroll: 0,
            selected: 0,
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("approve: edit_file"));
        assert!(rendered.contains("Allow once"));
    }

    #[test]
    fn assistant_markdown_heading_is_rendered_without_marker_and_highlighted() {
        let lines = assistant_lines("## Exemplo", 80);
        assert_eq!(line_text(&lines[0]), "insane: Exemplo");

        let highlighted: String = lines[0]
            .spans
            .iter()
            .filter(|span| span.style == theme::highlight())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(highlighted, "Exemplo");
    }

    #[test]
    fn assistant_markdown_bold_segments_drop_markers() {
        let lines = assistant_lines("texto **um** e **dois** fim", 80);
        assert_eq!(line_text(&lines[0]), "insane: texto um e dois fim");

        let highlighted: String = lines[0]
            .spans
            .iter()
            .filter(|span| span.style == theme::highlight())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(highlighted, "umdois");
    }

    #[test]
    fn assistant_markdown_incomplete_marker_is_preserved() {
        let lines = assistant_lines("foo **bar", 80);
        assert_eq!(line_text(&lines[0]), "insane: foo **bar");
    }

    #[test]
    fn indented_hashes_are_not_treated_as_headings() {
        let lines = assistant_lines("    ## comentario", 80);
        assert_eq!(line_text(&lines[0]), "insane:     ## comentario");
    }

    #[test]
    fn assistant_markdown_markers_are_stripped_in_rendered_conversation() {
        let mut state = AppState::new("model".into(), ".".into(), ".".into());
        state.messages.push(MsgBlock::Assistant(
            "## Plano\nUse **cargo test** agora".into(),
        ));

        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains("insane: Plano"));
        assert!(rendered.contains("Use cargo test agora"));
        assert!(!rendered.contains("## Plano"));
        assert!(!rendered.contains("**cargo test**"));
    }

    #[test]
    fn thinking_block_shows_placeholder_when_toggled_off() {
        let mut state = AppState::new("model".into(), ".".into(), ".".into());
        state
            .messages
            .push(MsgBlock::Thinking("private thought".into()));
        state.show_thinking = false;
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let hidden: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!hidden.contains("private thought"));
        assert!(hidden.contains("thinking..."));

        state.show_thinking = true;
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let visible: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(visible.contains("thinking: private thought"));
    }

    #[test]
    fn thinking_label_and_body_use_distinct_styles() {
        let lines = block_lines(&MsgBlock::Thinking("private thought".into()), 80, true);
        assert_eq!(line_text(&lines[0]), "thinking: private thought");
        assert_eq!(lines[0].spans[0].style, theme::thinking_label());
        assert_eq!(lines[0].spans[1].style, theme::thinking());
    }

    #[test]
    fn thinking_placeholder_uses_label_style() {
        let lines = block_lines(&MsgBlock::Thinking("private thought".into()), 80, false);
        assert_eq!(line_text(&lines[0]), "thinking...");
        assert_eq!(lines[0].spans[0].style, theme::thinking_label());
    }

    #[test]
    fn session_picker_is_rendered_in_bottom_layout() {
        let mut state = AppState::new("model".into(), ".".into(), ".".into());
        state.session_picker = Some(PendingSessionPicker {
            sessions: vec![SessionSummary {
                index: 0,
                model: "model-a".into(),
                messages: 3,
                preview: "primeira pergunta".into(),
            }],
            selected: 0,
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &state)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("resume"));
        assert!(rendered.contains("primeira pergunta"));
    }
}
