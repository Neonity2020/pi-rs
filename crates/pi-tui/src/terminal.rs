//! Terminal lifecycle — the counterpart of `pi/packages/tui/src/terminal.ts`.
//!
//! Owns raw mode, the alternate screen, and mouse capture; restores on drop
//! or via [`restore_terminal`] (installed as a panic hook by
//! [`init_terminal`]).

use std::io::{self, Stdout};

use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags};

use ratatui::Terminal as RatatuiTerminal;
use ratatui::prelude::CrosstermBackend;

pub type DefaultTerminal = RatatuiTerminal<CrosstermBackend<Stdout>>;

/// RAII guard: initializes the terminal and restores it on drop.
pub struct TerminalGuard {
	pub terminal: DefaultTerminal,
	restored: bool,
}

impl TerminalGuard {
	pub fn new() -> io::Result<Self> {
		let terminal = init_terminal()?;
		Ok(Self {
			terminal,
			restored: false,
		})
	}

	pub fn draw(&mut self, render: impl FnOnce(&mut ratatui::Frame)) -> io::Result<()> {
		self.terminal.draw(render).map(|_| ())
	}

	/// Restore once, idempotently (used on normal exit and panics).
	pub fn restore(&mut self) {
		if !self.restored {
			self.restored = true;
			restore_terminal();
		}
	}
}

impl Drop for TerminalGuard {
	fn drop(&mut self) {
		self.restore();
	}
}

/// Enter raw mode + alternate screen and build the ratatui terminal.
pub fn init_terminal() -> io::Result<DefaultTerminal> {
	let mut stdout = io::stdout();
	crossterm::terminal::enable_raw_mode()?;
	crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
	let _ = crossterm::execute!(
		stdout,
		crossterm::event::EnableMouseCapture,
		crossterm::event::PushKeyboardEnhancementFlags(
			KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
		)
	);
	let backend = CrosstermBackend::new(stdout);
	RatatuiTerminal::new(backend)
}

/// Leave alternate screen, disable mouse capture, restore cooked mode.
pub fn restore_terminal() {
	let _ = crossterm::execute!(
		io::stdout(),
		crossterm::event::DisableMouseCapture,
		PopKeyboardEnhancementFlags,
		crossterm::terminal::LeaveAlternateScreen
	);
	let _ = crossterm::terminal::disable_raw_mode();
}

/// Convenience: install a panic hook that restores the terminal, then init.
/// The TS pi does the same in `Terminal` construction.
pub fn init_terminal_with_panic_hook() -> io::Result<TerminalGuard> {
	let default_panic = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		restore_terminal();
		default_panic(info);
	}));
	TerminalGuard::new()
}
