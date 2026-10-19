//! pi-tui — UI state and rendering for pi-rs.
//!
//! Owns no I/O: the `coding` layer feeds it `UiEvent`s, reads back
//! `Action`s from key presses, and drives the terminal.

mod app;
mod commands;
mod markdown;
mod render;
mod theme;
mod wrap;

pub use app::{
    Action, App, ImageAttachment, Item, StatusInfo, Submission, ToolEntry, ToolStatus, UiEvent,
};
pub use commands::{help_text, Command, Completion, COMMANDS};
pub use render::render;

pub use crossterm;
pub use ratatui;

/// 12345 → "12.3k"
pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
