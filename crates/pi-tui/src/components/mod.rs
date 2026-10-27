//! Component model — counterpart of `pi/packages/tui/src/components/*`.
//!
//! pi's TS toolkit has its own component tree with layout nodes. On ratatui
//! we keep a minimal retained component contract: each component renders into
//! a rect and optionally consumes crossterm events.

pub mod editor;
pub mod markdown;
pub mod scroll_view;
pub mod select_list;
pub mod text;

use crossterm::event::Event;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

pub use editor::{Editor, EditorEvent};
pub use markdown::{markdown_lines, Markdown};
pub use scroll_view::ScrollView;
pub use select_list::{SelectAction, SelectItem, SelectList};
pub use text::Text;

/// Base component contract shared by all pi-tui widgets.
pub trait Component {
	/// Handle one terminal event; return true when consumed.
	fn handle_event(&mut self, event: &Event) -> bool;

	/// Render into `area` of the target buffer.
	fn render(&mut self, area: Rect, buf: &mut Buffer);

	/// Preferred height given a width, for layout negotiation.
	fn preferred_height(&self, width: u16) -> u16 {
		let _ = width;
		1
	}
}
