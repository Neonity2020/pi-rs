//! Shared truncation utilities — port of
//! `pi/packages/coding-agent/src/core/tools/truncate.ts`.
//!
//! Truncation is based on two independent limits — whichever is hit first
//! wins: a line limit (default 2000) and a byte limit (default 50KB). Head
//! truncation keeps the first lines/bytes (file reads, grep/find/ls output);
//! tail truncation keeps the last ones (bash output, where the end carries
//! errors and final results). Neither returns partial lines, except the
//! bash tail-truncation edge case where a single line exceeds the byte
//! limit (its tail is kept and `last_line_partial` is set).

/// `DEFAULT_MAX_LINES` upstream.
pub const DEFAULT_MAX_LINES: usize = 2000;
/// `DEFAULT_MAX_BYTES` upstream: 50KB.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// `GREP_MAX_LINE_LENGTH` upstream: max chars per grep match line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which limit ended the truncation (`truncatedBy` upstream); `None` when the
/// content was not truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
	Lines,
	Bytes,
}

/// Port of the upstream `TruncationResult` shape (fields consumed by tool
/// details and the truncation notices).
#[derive(Debug, Clone)]
pub struct TruncationResult {
	/// The truncated content.
	pub content: String,
	/// Whether truncation occurred.
	pub truncated: bool,
	/// Which limit was hit, if truncated.
	pub truncated_by: Option<TruncatedBy>,
	/// Total number of lines in the original content.
	pub total_lines: usize,
	/// Total number of bytes in the original content.
	pub total_bytes: usize,
	/// Number of complete lines in the truncated output.
	pub output_lines: usize,
	/// Number of bytes in the truncated output.
	pub output_bytes: usize,
	/// Whether the last line was partially truncated (tail truncation only).
	pub last_line_partial: bool,
	/// Whether the first line exceeded the byte limit (head truncation only).
	pub first_line_exceeds_limit: bool,
}

impl TruncationResult {
	fn untruncated(content: &str) -> Self {
		let (total_lines, total_bytes) = count_lines_and_bytes(content);
		Self {
			content: content.to_string(),
			truncated: false,
			truncated_by: None,
			total_lines,
			total_bytes,
			output_lines: total_lines,
			output_bytes: total_bytes,
			last_line_partial: false,
			first_line_exceeds_limit: false,
		}
	}
}

/// JS-style line counting (truncate.ts `splitLinesForCounting`): split on
/// `\n`, and a trailing newline does not open an extra line. `"a\n"` is one
/// line, `""` is zero lines.
fn split_lines(content: &str) -> Vec<&str> {
	if content.is_empty() {
		return Vec::new();
	}
	let mut lines: Vec<&str> = content.split('\n').collect();
	if content.ends_with('\n') {
		lines.pop();
	}
	lines
}

fn count_lines_and_bytes(content: &str) -> (usize, usize) {
	(split_lines(content).len(), content.len())
}

/// `formatSize` upstream: `1023B`, `50.0KB`, `1.5MB`.
pub fn format_size(bytes: usize) -> String {
	if bytes < 1024 {
		format!("{bytes}B")
	} else if bytes < 1024 * 1024 {
		format!("{:.1}KB", bytes as f64 / 1024.0)
	} else {
		format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
	}
}

/// Truncate content from the head (keep first N lines/bytes). Suitable for
/// file reads where you want to see the beginning. Never returns partial
/// lines; if the first line alone exceeds the byte limit, returns empty
/// content with `first_line_exceeds_limit` set.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
	let total_bytes = content.len();
	let lines = split_lines(content);
	let total_lines = lines.len();

	if total_lines <= max_lines && total_bytes <= max_bytes {
		return TruncationResult::untruncated(content);
	}

	// First line alone exceeds the byte limit.
	if lines[0].len() > max_bytes {
		return TruncationResult {
			content: String::new(),
			truncated: true,
			truncated_by: Some(TruncatedBy::Bytes),
			total_lines,
			total_bytes,
			output_lines: 0,
			output_bytes: 0,
			last_line_partial: false,
			first_line_exceeds_limit: true,
		};
	}

	// Collect complete lines that fit.
	let mut output: Vec<&str> = Vec::new();
	let mut output_bytes = 0usize;
	let mut truncated_by = TruncatedBy::Lines;
	for (i, line) in lines.iter().enumerate().take(max_lines) {
		let line_bytes = line.len() + usize::from(i > 0); // +1 for newline
		if output_bytes + line_bytes > max_bytes {
			truncated_by = TruncatedBy::Bytes;
			break;
		}
		output_bytes += line_bytes;
		output.push(line);
	}
	if output.len() >= max_lines && output_bytes <= max_bytes {
		truncated_by = TruncatedBy::Lines;
	}

	let content_out = output.join("\n");
	TruncationResult {
		output_bytes: content_out.len(),
		content: content_out,
		truncated: true,
		truncated_by: Some(truncated_by),
		total_lines,
		total_bytes,
		output_lines: output.len(),
		last_line_partial: false,
		first_line_exceeds_limit: false,
	}
}

/// Truncate content from the tail (keep last N lines/bytes). Suitable for
/// bash output where you want to see the end. May return a partial first
/// line when the last line of the original content exceeds the byte limit.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
	let total_bytes = content.len();
	let lines = split_lines(content);
	let total_lines = lines.len();

	if total_lines <= max_lines && total_bytes <= max_bytes {
		return TruncationResult::untruncated(content);
	}

	// Work backwards from the end.
	let mut output_rev: Vec<String> = Vec::new();
	let mut output_bytes = 0usize;
	let mut truncated_by = TruncatedBy::Lines;
	let mut last_line_partial = false;
	for line in lines.iter().rev().take(max_lines) {
		let line_bytes = line.len() + usize::from(!output_rev.is_empty()); // +1 for newline
		if output_bytes + line_bytes > max_bytes {
			truncated_by = TruncatedBy::Bytes;
			// Edge case: no lines collected yet and this line exceeds the
			// byte limit — keep the end of the line (partial).
			if output_rev.is_empty() {
				let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
				output_bytes = truncated_line.len();
				output_rev.push(truncated_line);
				last_line_partial = true;
			}
			break;
		}
		output_bytes += line_bytes;
		output_rev.push((*line).to_string());
	}
	output_rev.reverse();
	if output_rev.len() >= max_lines && output_bytes <= max_bytes {
		truncated_by = TruncatedBy::Lines;
	}

	let content_out = output_rev.join("\n");
	TruncationResult {
		output_bytes: content_out.len(),
		content: content_out,
		truncated: true,
		truncated_by: Some(truncated_by),
		total_lines,
		total_bytes,
		output_lines: output_rev.len(),
		last_line_partial,
		first_line_exceeds_limit: false,
	}
}

/// Truncate a string to fit within a byte limit counting from the end,
/// never splitting a UTF-8 character (`truncateStringToBytesFromEnd`).
fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
	if s.len() <= max_bytes {
		return s.to_string();
	}
	let mut start = s.len() - max_bytes;
	while start < s.len() && !s.is_char_boundary(start) {
		start += 1;
	}
	s[start..].to_string()
}

/// Truncate a single line to max chars, adding a `[truncated]` suffix
/// (used for grep match lines). Char-based like upstream's `truncateLine`.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
	if line.chars().count() <= max_chars {
		return (line.to_string(), false);
	}
	let text: String = line.chars().take(max_chars).collect();
	(format!("{text}... [truncated]"), true)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn head_truncation_respects_both_limits() {
		// Line limit hits first.
		let content = (0..3000)
			.map(|i| format!("line{i}"))
			.collect::<Vec<_>>()
			.join("\n");
		let result = truncate_head(&content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(result.truncated);
		assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
		assert_eq!(result.output_lines, DEFAULT_MAX_LINES);
		assert_eq!(result.total_lines, 3000);
		assert_eq!(result.content.lines().count(), DEFAULT_MAX_LINES);

		// Byte limit hits first.
		let content = "x".repeat(100).to_string();
		let content = vec![content; 600].join("\n"); // ~60KB
		let result = truncate_head(&content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(result.truncated);
		assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
		assert!(result.output_bytes <= DEFAULT_MAX_BYTES);
	}

	#[test]
	fn head_truncation_flags_oversized_first_line() {
		let content = format!("{}\nsecond\n", "y".repeat(DEFAULT_MAX_BYTES + 1));
		let result = truncate_head(&content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(result.truncated);
		assert!(result.first_line_exceeds_limit);
		assert!(result.content.is_empty());
	}

	#[test]
	fn tail_truncation_keeps_newest_lines() {
		let content = (0..3000)
			.map(|i| format!("line{i}"))
			.collect::<Vec<_>>()
			.join("\n");
		let result = truncate_tail(&content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(result.truncated);
		assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
		assert_eq!(result.output_lines, DEFAULT_MAX_LINES);
		assert!(result.content.starts_with("line1000"));
		assert!(result.content.ends_with("line2999"));
		assert_eq!(result.total_lines, 3000);
	}

	#[test]
	fn tail_truncation_keeps_last_bytes() {
		let content = vec!["a".repeat(1000); 100].join("\n"); // ~100KB
		let result = truncate_tail(&content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(result.truncated);
		assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
		assert!(result.output_bytes <= DEFAULT_MAX_BYTES);
		assert!(result.content.ends_with(&"a".repeat(1000)));
	}

	#[test]
	fn tail_truncation_partial_last_line() {
		// One single line over the byte limit: keep its tail, mark partial.
		let content = "b".repeat(DEFAULT_MAX_BYTES + 10);
		let result = truncate_tail(&content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(result.truncated);
		assert!(result.last_line_partial);
		assert_eq!(result.output_bytes, DEFAULT_MAX_BYTES);
		assert!(result.content.starts_with("bb"));
	}

	#[test]
	fn tail_truncation_cut_on_utf8_boundary() {
		let content = format!("{}中文", "a".repeat(DEFAULT_MAX_BYTES - 1));
		let result = truncate_tail(&content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(result.last_line_partial);
		assert!(result.content.ends_with("中文"));
	}

	#[test]
	fn no_truncation_when_within_limits() {
		let result = truncate_tail("hello\nworld", DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(!result.truncated);
		assert_eq!(result.content, "hello\nworld");
		assert_eq!(result.total_lines, 2);
		let result = truncate_head("hello\nworld\n", DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
		assert!(!result.truncated);
		assert_eq!(result.total_lines, 2);
	}

	#[test]
	fn format_size_matches_upstream() {
		assert_eq!(format_size(1023), "1023B");
		assert_eq!(format_size(DEFAULT_MAX_BYTES), "50.0KB");
		assert_eq!(format_size(1024 * 1024), "1.0MB");
	}

	#[test]
	fn truncate_line_marks_long_lines() {
		let (text, was) = truncate_line(&"a".repeat(600), GREP_MAX_LINE_LENGTH);
		assert!(was);
		assert!(text.ends_with("... [truncated]"));
		assert_eq!(
			text.chars().count(),
			GREP_MAX_LINE_LENGTH + "... [truncated]".len()
		);
		let (text, was) = truncate_line("short", GREP_MAX_LINE_LENGTH);
		assert!(!was);
		assert_eq!(text, "short");
	}
}
