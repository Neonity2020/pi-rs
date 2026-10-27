//! Renders a sample chat viewport (markdown + tool call + select list) into
//! a TestBackend buffer and prints it — a smoke preview of the TUI pipeline.

use pi_tui::components::{markdown_lines, SelectList, SelectItem};
use pi_tui::Theme;

fn main() {
	let theme = Theme::dark();
	let width = 78usize;

	let sample = "# Refactor plan\n\nMove the *streaming* path into `providers/mod.rs` and:\n\n1. extract an SSE decoder\n2. wire **incremental** tool-call deltas\n3. keep compat overrides\n\n> Note: session files stay camelCase on disk.\n\n```rust\nlet x = 42; // answer\n```\n\nSee [docs](https://pi.dev/docs) for details.\n";

	println!("── assistant markdown ──────────────────────────────");
	for line in markdown_lines(sample, width, &theme) {
		let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
		println!("{text}");
	}

	println!("\n── tool call (collapsed) ───────────────────────────");
	println!("✓ read {package}", package = "path=\"/tmp/a.txt\" limit=5");
	println!("    .DS_Store\t6148B");
	println!("    .env\t456B");
	println!("  … (8 more lines, ctrl+o to expand)");

	println!("\n── model selector overlay ──────────────────────────");
	let mut list = SelectList::new(
		vec![
			SelectItem {
				value: "mimo/mimo-v2.6-pro".into(),
				label: "mimo-v2.6-pro".into(),
				description: Some("[mimo] · 128k ctx · MiMo V2.6 Pro".into()),
			},
			SelectItem {
				value: "openai/gpt-5.2".into(),
				label: "gpt-5.2".into(),
				description: Some("[openai] · 400k ctx".into()),
			},
			SelectItem {
				value: "mock/mock-echo".into(),
				label: "mock-echo".into(),
				description: Some("[mock] · 128k ctx · Mock (built-in)".into()),
			},
		],
		10,
	);
	list.set_mark_value(Some("mimo/mimo-v2.6-pro".into()));
	list.set_filter("oai");
	for line in list.render_lines(width, &theme) {
		let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
		println!("{text}");
	}
}
