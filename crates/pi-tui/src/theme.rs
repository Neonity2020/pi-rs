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
	// Markdown element colors — counterpart of pi's `md*` theme tokens
	// (system-theme.ts): mdHeading yellow, mdLink blue, mdCode/mdListBullet
	// violet, mdCodeBlock green, quote/hr/borders dim.
	/// Markdown headings (`mdHeading`).
	pub md_heading: Color,
	/// Markdown link text (`mdLink`).
	pub md_link: Color,
	/// Markdown link URL fallback (`mdLinkUrl`).
	pub md_link_url: Color,
	/// Markdown inline code (`mdCode`).
	pub md_code: Color,
	/// Markdown fenced code content (`mdCodeBlock`).
	pub md_code_block: Color,
	/// Markdown code fence lines (`mdCodeBlockBorder`).
	pub md_code_block_border: Color,
	/// Markdown blockquote text (`mdQuote`).
	pub md_quote: Color,
	/// Markdown blockquote border (`mdQuoteBorder`).
	pub md_quote_border: Color,
	/// Markdown horizontal rules (`mdHr`).
	pub md_hr: Color,
	/// Markdown list bullets/numbers (`mdListBullet`).
	pub md_list_bullet: Color,
	/// Background behind user messages (`userMessageBg`).
	pub user_bg: Color,
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
			md_heading: Color::Yellow,
			md_link: Color::Blue,
			md_link_url: Color::DarkGray,
			md_code: Color::Magenta,
			md_code_block: Color::Green,
			md_code_block_border: Color::DarkGray,
			md_quote: Color::Gray,
			md_quote_border: Color::DarkGray,
			md_hr: Color::DarkGray,
			md_list_bullet: Color::Magenta,
			user_bg: Color::Indexed(236),
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
			md_heading: Color::Indexed(130),
			md_link: Color::Blue,
			md_link_url: Color::Indexed(244),
			md_code: Color::Indexed(127),
			md_code_block: Color::Indexed(28),
			md_code_block_border: Color::Indexed(249),
			md_quote: Color::Indexed(238),
			md_quote_border: Color::Indexed(249),
			md_hr: Color::Indexed(249),
			md_list_bullet: Color::Indexed(127),
			user_bg: Color::Indexed(254),
		}
	}
}
