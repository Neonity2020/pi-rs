//! A small, dark-terminal-first palette shared by the transcript and Markdown.
//!
//! Colors stay deliberately muted: structure comes from spacing and weight,
//! while hue is reserved for focus, state, and semantic meaning.

use ratatui::style::Color;

pub const TEXT: Color = Color::Rgb(218, 221, 230);
pub const MUTED: Color = Color::Rgb(119, 126, 145);
pub const BORDER: Color = Color::Rgb(70, 78, 96);
pub const PRIMARY: Color = Color::Rgb(125, 170, 255);
pub const SECONDARY: Color = Color::Rgb(126, 207, 219);
pub const PURPLE: Color = Color::Rgb(190, 155, 255);
pub const SUCCESS: Color = Color::Rgb(126, 211, 142);
pub const WARNING: Color = Color::Rgb(229, 192, 123);
pub const ERROR: Color = Color::Rgb(239, 113, 113);
pub const SELECTED: Color = Color::Rgb(39, 44, 56);
