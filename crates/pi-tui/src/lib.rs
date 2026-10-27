//! pi-tui — terminal UI toolkit for pi-rs, built on ratatui + crossterm.
//!
//! Counterpart of `pi/packages/tui`: the TS pi ships its own immediate-mode
//! terminal toolkit (components, editor, layout). pi-rs keeps the same
//! concepts — [`Component`](components::Component), an emacs-style
//! [`Editor`](components::Editor), a scrolling
//! [`ScrollView`](components::ScrollView), themes, keybindings — but maps
//! them onto ratatui widgets instead of reimplementing the render loop.

pub mod components;
pub mod fuzzy;
pub mod keys;
pub mod terminal;
pub mod theme;

pub use components::{Component, Editor, ScrollView, Text};
pub use terminal::{TerminalGuard, init_terminal, restore_terminal};
pub use theme::Theme;
