//! Inline ratatui rendering: finalized transcript blocks are inserted into
//! native scrollback by `mod.rs`, while this module draws only the live tail,
//! spinner, composer, status, and small pickers in the inline viewport.

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BorderType, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{AppState, MsgBlock, PendingConfirm, PendingSessionPicker, ToolStatus};
use super::format::{diff_lines_for_modal, truncate_summary, wrap_text, DiffLineKind};
use super::theme;

const MAX_INPUT_LINES: u16 = 5;
const MIN_INLINE_HEIGHT: u16 = 6;
const MAX_INLINE_HEIGHT: u16 = 15;
pub(crate) const LIVE_TAIL_BUDGET: usize = 8;

pub fn draw(frame: &mut Frame, state: &AppState) {
    draw_inline(frame, state);
}

pub fn draw_inline(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let width = area.width.max(1) as usize;
    let input_width = area.width.saturating_sub(4).max(1) as usize;
    let (visual_input, _) = input_layout(&state.input, state.cursor, input_width);
    let input_lines = visual_input.len().max(1).min(MAX_INPUT_LINES as usize) as u16;
    let suggestions = state.suggestions();
    let suggestion_height = if suggestions.is_empty() {
        0
    } else {
        suggestions.len().min(4) as u16 + 1
    };
    let picker_height = if let Some(confirm) = &state.confirm {
        confirm.option_count() as u16 + 1
    } else if let Some(picker) = &state.session_picker {
        picker.sessions.len().min(3) as u16 + 2
    } else {
        0
    };
    let spinner_height = if state.processing || state.status.spinner_line.is_some() {
        1
    } else {
        0
    };
    let fixed_height = spinner_height + suggestion_height + picker_height + input_lines + 3;
    let tail_height = area.height.saturating_sub(fixed_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(tail_height),
            Constraint::Length(spinner_height),
            Constraint::Length(suggestion_height),
            Constraint::Length(picker_height),
            Constraint::Length(input_lines + 2),
            Constraint::Length(1),
        ])
        .split(area);

    draw_live_tail(frame, chunks[0], state, width);
    if spinner_height > 0 {
        draw_spinner(frame, chunks[1], state);
    }
    if suggestion_height > 0 {
        draw_suggestions(frame, chunks[2], state);
    }
    if let Some(confirm) = &state.confirm {
        draw_confirm_menu(frame, chunks[3], confirm);
    } else if let Some(picker) = &state.session_picker {
        draw_session_picker(frame, chunks[3], picker);
    }
    draw_input(frame, chunks[4], state);
    draw_status(frame, chunks[5], state);
}

pub(crate) fn desired_inline_height(state: &AppState, width: usize) -> u16 {
    let input_width = width.saturating_sub(4).max(1);
    let (visual_input, _) = input_layout(&state.input, state.cursor, input_width);
    let input_lines = visual_input.len().max(1).min(MAX_INPUT_LINES as usize) as u16;
    let suggestions = state.suggestions();
    let suggestion_height = if suggestions.is_empty() {
        0
    } else {
        suggestions.len().min(4) as u16 + 1
    };
    let picker_height = if let Some(confirm) = &state.confirm {
        confirm.option_count() as u16 + 1
    } else if let Some(picker) = &state.session_picker {
        picker.sessions.len().min(3) as u16 + 2
    } else {
        0
    };
    let spinner_height = if state.processing || state.status.spinner_line.is_some() {
        1
    } else {
        0
    };
    let live_height = live_tail_lines(state, width)
        .len()
        .min(LIVE_TAIL_BUDGET) as u16;
    (live_height + spinner_height + suggestion_height + picker_height + input_lines + 3)
        .clamp(MIN_INLINE_HEIGHT, MAX_INLINE_HEIGHT)
}

pub(crate) fn block_lines(
    msg: &MsgBlock,
    width: usize,
    show_thinking: bool,
) -> Vec<Line<'static>> {
    match msg {
        MsgBlock::User(text) => user_lines(text, width),
        MsgBlock::Assistant(text) if text.is_empty() => Vec::new(),
        MsgBlock::Assistant(text) => assistant_lines(text, width),
        MsgBlock::Thinking(text) if text.trim().is_empty() => Vec::new(),
        MsgBlock::Thinking(_) if !show_thinking => thinking_placeholder_lines(width),
        MsgBlock::Thinking(text) => thinking_lines(text, width),
        MsgBlock::Tool {
            status,
            call,
            result,
        } => tool_lines(*status, call, result.as_deref(), width),
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

pub(crate) fn live_tail_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (idx, msg) in state.messages.iter().enumerate().skip(state.committed_blocks) {
        if idx == state.committed_blocks && state.live_committed_chars > 0 {
            if let MsgBlock::Assistant(text) = msg {
                if let Some(tail) = text.get(state.live_committed_chars..) {
                    lines.extend(assistant_lines(tail, width));
                }
                continue;
            }
        }
        lines.extend(block_lines(msg, width, state.show_thinking));
    }
    lines
}

pub(crate) fn confirm_transcript_lines(
    pending: &PendingConfirm,
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = wrap_text(&pending.req.prompt, width)
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                line,
                theme::assistant().add_modifier(Modifier::BOLD),
            ))
        })
        .collect();

    if let Some(details) = &pending.req.details {
        lines.push(Line::from(""));
        lines.extend(
            wrap_text(details, width)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, theme::warning()))),
        );
    }

    if let Some(command) = &pending.req.command {
        lines.push(Line::from(""));
        lines.extend(
            wrap_text(command, width.saturating_sub(2))
                .into_iter()
                .map(|line| Line::from(Span::styled(format!("$ {line}"), theme::warning()))),
        );
    }

    if let Some(diff) = &pending.req.diff {
        lines.push(Line::from(""));
        for (kind, line) in diff_lines_for_modal(diff) {
            let style = match kind {
                DiffLineKind::Add => theme::success(),
                DiffLineKind::Del => theme::danger(),
                DiffLineKind::Meta => theme::brand(),
                DiffLineKind::Context => theme::muted(),
            };
            lines.extend(
                wrap_text(line, width)
                    .into_iter()
                    .map(|wrapped| Line::from(Span::styled(wrapped, style))),
            );
        }
    }

    lines
}

fn user_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    wrap_text(text, width.saturating_sub(2).max(1))
        .into_iter()
        .map(|line| {
            Line::from(vec![
                Span::styled("> ", theme::user_prompt()),
                Span::styled(line, theme::user_prompt()),
            ])
        })
        .collect()
}

fn assistant_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.chars().next().is_some_and(|c| c.is_whitespace()) {
            lines.extend(wrap_preserving_spaces(raw_line, theme::assistant(), width));
        } else {
            lines.extend(wrap_styled_segments(parse_assistant_markup(raw_line), width));
        }
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

fn tool_lines(
    status: ToolStatus,
    call: &str,
    result: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let call_width = width.saturating_sub(2).max(1);
    for (idx, wrapped) in wrap_text(call, call_width).into_iter().enumerate() {
        if idx == 0 {
            lines.push(Line::from(vec![
                Span::styled("● ", theme::bullet(status)),
                Span::styled(wrapped, theme::assistant()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(wrapped, theme::assistant()),
            ]));
        }
    }
    if let Some(result) = result {
        let summary = truncate_summary(result, width.saturating_sub(5).max(1));
        for (idx, wrapped) in wrap_text(&summary, width.saturating_sub(5).max(1))
            .into_iter()
            .enumerate()
        {
            let prefix = if idx == 0 { "  ⎿  " } else { "     " };
            lines.push(Line::from(vec![
                Span::styled(prefix, theme::tool_result()),
                Span::styled(wrapped, theme::tool_result()),
            ]));
        }
    }
    lines
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

fn wrap_preserving_spaces(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![Line::from("")];
    }
    let mut wrapped = Vec::new();
    let mut current = Vec::new();
    for ch in text.chars() {
        if current.len() == width {
            wrapped.push(std::mem::take(&mut current));
        }
        current.push(StyledChar { ch, style });
    }
    wrapped.push(current);
    wrapped.into_iter().map(line_from_styled_chars).collect()
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

fn draw_live_tail(frame: &mut Frame, area: Rect, state: &AppState, width: usize) {
    if area.height == 0 {
        return;
    }
    let lines = live_tail_lines(state, width);
    let start = lines.len().saturating_sub(area.height as usize);
    frame.render_widget(
        Paragraph::new(lines[start..].to_vec()).wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_spinner(frame: &mut Frame, area: Rect, state: &AppState) {
    let work = state
        .status
        .spinner_line
        .clone()
        .unwrap_or_else(|| "Working...".to_string());
    let suffix = if state.processing {
        " (esc to interrupt)"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("✻ ", theme::thinking_label()),
            Span::styled(format!("{work}{suffix}"), theme::muted()),
        ])),
        area,
    );
}

fn draw_suggestions(frame: &mut Frame, area: Rect, state: &AppState) {
    let suggestions = state.suggestions();
    let selected = state
        .suggestion_idx
        .min(suggestions.len().saturating_sub(1));
    let start = selected.saturating_sub(3);
    let lines: Vec<Line<'static>> = suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(4)
        .map(|(idx, suggestion)| {
            let style = if idx == selected {
                theme::selected()
            } else {
                theme::assistant()
            };
            Line::from(vec![
                Span::styled(if idx == selected { "› " } else { "  " }, style),
                Span::styled(format!("{:<28}", suggestion.label), style),
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
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let width = area.width.saturating_sub(4).max(1) as usize;
    let (lines, (cursor_row, cursor_col)) = input_layout(&state.input, state.cursor, width);
    let inner_height = area.height.saturating_sub(2).max(1) as usize;
    let start = cursor_row.saturating_sub(inner_height.saturating_sub(1));
    let end = (start + inner_height).min(lines.len());
    let display: Vec<String> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            if start + idx == 0 {
                format!("> {line}")
            } else {
                format!("  {line}")
            }
        })
        .collect();
    let title = if state.processing {
        " esc interrupt  ctrl+o thinking "
    } else {
        " enter send  shift+tab mode  ctrl+o thinking  alt+enter newline "
    };
    let p = Paragraph::new(display.join("\n"))
        .style(theme::assistant())
        .block(theme::block(title).border_type(BorderType::Rounded))
        .wrap(Wrap { trim: false });
    frame.render_widget(p, area);

    if !state.processing && state.confirm.is_none() && state.session_picker.is_none() {
        let row = cursor_row.saturating_sub(start) as u16;
        let col = (cursor_col as u16).min(area.width.saturating_sub(4));
        frame.set_cursor_position(Position::new(area.x + 3 + col, area.y + 1 + row));
    }
}

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
    match state.mode {
        super::app::InteractionMode::Plan => {
            spans.push(Span::styled(
                "⏸ plan mode on (shift+tab to cycle)",
                theme::mode_text(state.mode),
            ));
        }
        mode => {
            spans.push(Span::styled("mode ", theme::subtle()));
            spans.push(Span::styled(mode.label(), theme::mode_text(mode)));
        }
    }
    push_status_part(&mut spans, format!("{}/{}", state.provider, state.model));
    if let (Some(used), Some(cap)) = (state.status.rate_used, state.status.rate_capacity) {
        push_status_part(&mut spans, format!("rate {used}/{cap}"));
    } else if let Some(used) = state.status.rate_used {
        push_status_part(&mut spans, format!("rate {used}/∞"));
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
    push_status_part(&mut spans, token_text);
    push_status_part(
        &mut spans,
        "shift+tab mode  ctrl+o thinking  ctrl+c exit/cancel  /help".to_string(),
    );
    frame.render_widget(Paragraph::new(Line::from(spans)).style(theme::muted()), area);
}

fn push_status_part(spans: &mut Vec<Span<'static>>, text: String) {
    spans.push(Span::styled("  ·  ", theme::subtle()));
    spans.push(Span::styled(text, theme::muted()));
}

fn draw_confirm_menu(frame: &mut Frame, area: Rect, pending: &PendingConfirm) {
    let mut lines = vec![Line::from(Span::styled(
        format!("approve: {}", pending.req.tool),
        theme::warning().add_modifier(Modifier::BOLD),
    ))];
    for (idx, label) in option_labels(pending).iter().enumerate() {
        let selected = idx == pending.selected;
        lines.push(Line::from(Span::styled(
            format!("{} {label}", if selected { "›" } else { " " }),
            if selected {
                theme::selected()
            } else {
                theme::assistant()
            },
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn option_labels(pending: &PendingConfirm) -> Vec<&'static str> {
    if pending.option_count() == 2 {
        vec!["Allow once", "Deny"]
    } else if pending.req.tool == "run_command" {
        vec!["Allow once", "Allow this exact command for session", "Deny"]
    } else {
        vec!["Allow once", "Allow this tool for session", "Deny"]
    }
}

fn draw_session_picker(frame: &mut Frame, area: Rect, picker: &PendingSessionPicker) {
    let mut lines = vec![Line::from(Span::styled(
        "resume session",
        theme::assistant().add_modifier(Modifier::BOLD),
    ))];
    let start = picker.selected.saturating_sub(1);
    for (idx, session) in picker.sessions.iter().enumerate().skip(start).take(3) {
        let selected = idx == picker.selected;
        lines.push(Line::from(Span::styled(
            format!(
                "{} {}. {} mensagens · {} · {}",
                if selected { "›" } else { " " },
                idx + 1,
                session.messages,
                session.model,
                session.preview
            ),
            if selected {
                theme::selected()
            } else {
                theme::assistant()
            },
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[cfg(test)]
mod tests {
    use super::{assistant_lines, block_lines, desired_inline_height, draw_inline, input_layout};
    use crate::session_store::SessionSummary;
    use crate::tui::app::{AppState, MsgBlock, PendingConfirm, PendingSessionPicker, ToolStatus};
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
    fn approval_menu_is_rendered_inline() {
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
            printed: true,
            selected: 0,
        });
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_inline(frame, &state)).unwrap();
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
        assert_eq!(line_text(&lines[0]), "Exemplo");

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
        assert_eq!(line_text(&lines[0]), "texto um e dois fim");

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
        assert_eq!(line_text(&lines[0]), "foo **bar");
    }

    #[test]
    fn indented_hashes_are_not_treated_as_headings() {
        let lines = assistant_lines("    ## comentario", 80);
        assert_eq!(line_text(&lines[0]), "    ## comentario");
    }

    #[test]
    fn block_lines_uses_claude_code_style_prefixes() {
        let user = block_lines(&MsgBlock::User("hello".into()), 80, true);
        assert_eq!(line_text(&user[0]), "> hello");

        let tool = block_lines(
            &MsgBlock::Tool {
                status: ToolStatus::Success,
                call: "read_file(src/lib.rs)".into(),
                result: Some("ok in 3ms".into()),
            },
            80,
            true,
        );
        assert_eq!(line_text(&tool[0]), "● read_file(src/lib.rs)");
        assert_eq!(line_text(&tool[1]), "  ⎿  ok in 3ms");
    }

    #[test]
    fn thinking_block_shows_placeholder_when_toggled_off() {
        let lines = block_lines(&MsgBlock::Thinking("private thought".into()), 80, false);
        assert_eq!(line_text(&lines[0]), "thinking...");
    }

    #[test]
    fn thinking_label_and_body_use_distinct_styles() {
        let lines = block_lines(&MsgBlock::Thinking("private thought".into()), 80, true);
        assert_eq!(line_text(&lines[0]), "thinking: private thought");
        assert_eq!(lines[0].spans[0].style, theme::thinking_label());
        assert_eq!(lines[0].spans[1].style, theme::thinking());
    }

    #[test]
    fn draw_inline_renders_live_thinking_text() {
        let mut state = AppState::new("model".into(), ".".into(), ".".into());
        state.processing = true;
        state.messages.push(MsgBlock::Thinking("private thought".into()));

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_inline(frame, &state)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains("thinking: private thought"));
    }

    #[test]
    fn desired_inline_height_grows_with_multiline_input_and_caps() {
        let mut state = AppState::new("model".into(), ".".into(), ".".into());
        let short = desired_inline_height(&state, 80);
        state.set_input("a\nb\nc\nd\ne\nf".into());
        let tall = desired_inline_height(&state, 80);

        assert!(tall > short);
        assert!(tall <= 15);
    }

    #[test]
    fn session_picker_is_rendered_inline() {
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
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_inline(frame, &state)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("resume session"));
        assert!(rendered.contains("primeira pergunta"));
    }
}
