//! Theme — counterpart of `pi/packages/tui/src/colors.ts` and the
//! coding-agent's `theme/` directory (theme.schema.json). Full theme-file
//! loading lands with the interactive-mode polish pass.

use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Theme {
	pub name: String,
	/// User/echo text.
	pub user: Color,
	/// Assistant text.
	pub assistant: Color,
	/// Thinking/reasoning text.
	pub thinking: Color,
	/// Tool names and args.
	pub tool: Color,
	/// Tool result output.
	pub tool_output: Color,
	/// Errors and aborted runs.
	pub error: Color,
	/// Success indicators.
	pub success: Color,
	/// Borders and separators.
	pub border: Color,
	/// Prompts, hints, footer.
	pub accent: Color,
	/// Dim metadata (timestamps, token counts).
	pub muted: Color,
}

impl Default for Theme {
	fn default() -> Self {
		Self::dark()
	}
}

impl Theme {
	pub fn dark() -> Self {
		Self {
			name: "pi-dark".into(),
			user: Color::Blue,
			assistant: Color::White,
			thinking: Color::DarkGray,
			tool: Color::Yellow,
			tool_output: Color::Gray,
			error: Color::Red,
			success: Color::Green,
			border: Color::DarkGray,
			accent: Color::Cyan,
			muted: Color::DarkGray,
		}
	}

	pub fn light() -> Self {
		Self {
			name: "pi-light".into(),
			user: Color::Blue,
			assistant: Color::Black,
			thinking: Color::Indexed(244),
			tool: Color::Indexed(130),
			tool_output: Color::Indexed(238),
			error: Color::Red,
			success: Color::Green,
			border: Color::Indexed(250),
			accent: Color::Cyan,
			muted: Color::Indexed(249),
		}
	}
}
