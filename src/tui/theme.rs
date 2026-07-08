use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};

use super::app::InteractionMode;

pub const BG: Color = Color::Rgb(12, 16, 24);
pub const SURFACE: Color = Color::Rgb(24, 30, 42);
pub const SURFACE_SOFT: Color = Color::Rgb(34, 42, 57);
pub const BORDER: Color = Color::Rgb(69, 82, 104);
pub const TEXT: Color = Color::Rgb(226, 232, 240);
pub const MUTED: Color = Color::Rgb(148, 163, 184);
pub const SUBTLE: Color = Color::Rgb(100, 116, 139);
pub const ACCENT: Color = Color::Rgb(56, 189, 248);
pub const ACCENT_DARK: Color = Color::Rgb(8, 47, 73);
pub const SUCCESS: Color = Color::Rgb(74, 222, 128);
pub const WARNING: Color = Color::Rgb(250, 204, 21);
pub const HIGHLIGHT: Color = Color::Rgb(251, 146, 60);
pub const DANGER: Color = Color::Rgb(248, 113, 113);
pub const PURPLE: Color = Color::Rgb(167, 139, 250);
pub const THINKING_TEXT: Color = Color::Rgb(71, 85, 105);

pub fn app_bg() -> Style {
    Style::default().fg(TEXT)
}

pub fn panel() -> Style {
    Style::default().fg(TEXT)
}

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn subtle() -> Style {
    Style::default().fg(SUBTLE)
}

pub fn assistant() -> Style {
    Style::default().fg(TEXT)
}

pub fn assistant_markdown() -> Style {
    highlight()
}

pub fn user() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn user_prompt() -> Style {
    Style::default().fg(MUTED)
}

pub fn warning() -> Style {
    Style::default().fg(WARNING)
}

pub fn highlight() -> Style {
    Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD)
}

pub fn thinking() -> Style {
    Style::default().fg(THINKING_TEXT)
}

pub fn thinking_label() -> Style {
    Style::default().fg(WARNING).add_modifier(Modifier::BOLD)
}

pub fn success() -> Style {
    Style::default().fg(SUCCESS)
}

pub fn danger() -> Style {
    Style::default().fg(DANGER)
}

pub fn tool_running() -> Style {
    Style::default().fg(PURPLE)
}

pub fn bullet(status: super::app::ToolStatus) -> Style {
    match status {
        super::app::ToolStatus::Running => tool_running(),
        super::app::ToolStatus::Success => success(),
        super::app::ToolStatus::Failed => danger(),
    }
    .add_modifier(Modifier::BOLD)
}

pub fn tool_result() -> Style {
    muted()
}

pub fn selected() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(ACCENT_DARK)
        .add_modifier(Modifier::BOLD)
}

pub fn brand() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn header() -> Style {
    Style::default().fg(TEXT)
}

pub fn header_dim() -> Style {
    Style::default().fg(MUTED)
}

pub fn mode(mode: InteractionMode) -> Style {
    let fg = match mode {
        InteractionMode::Default => MUTED,
        InteractionMode::Plan => WARNING,
        InteractionMode::Auto => SUCCESS,
        InteractionMode::AcceptEdits => ACCENT,
    };
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

pub fn mode_text(mode: InteractionMode) -> Style {
    match mode {
        InteractionMode::Default => muted(),
        InteractionMode::Plan => warning(),
        InteractionMode::Auto => success(),
        InteractionMode::AcceptEdits => Style::default().fg(ACCENT),
    }
    .add_modifier(Modifier::BOLD)
}

pub fn block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(title.into())
        .title_style(Style::default().fg(MUTED))
        .style(panel())
}
