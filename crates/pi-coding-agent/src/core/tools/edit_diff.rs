//! Diff/edit computation utilities — port of
//! `pi/packages/coding-agent/src/core/tools/edit-diff.ts` (the subset the
//! edit tool needs).
//!
//! Accepted difference vs upstream: the upstream matcher falls back to
//! fuzzy matching (NFKC normalization, smart quotes/dashes/spaces, trailing
//! whitespace) when an exact `oldText` match fails. pi-rs supports exact
//! matching only for now — a non-matching `oldText` is a "not found" error.

/// `detectLineEnding`: the first `\r\n` wins over the first `\n`, whichever
/// comes first in the content.
pub fn detect_line_ending(content: &str) -> &'static str {
	match (content.find("\r\n"), content.find('\n')) {
		(Some(crlf), Some(lf)) if crlf < lf => "\r\n",
		_ => "\n",
	}
}

/// `normalizeToLF`: `\r\n` and lone `\r` all become `\n`.
pub fn normalize_to_lf(text: &str) -> String {
	if !text.contains('\r') {
		return text.to_string();
	}
	text.replace("\r\n", "\n").replace('\r', "\n")
}

/// `restoreLineEndings`: convert `\n` back to the file's original ending.
pub fn restore_line_endings(text: &str, ending: &str) -> String {
	if ending == "\r\n" {
		text.replace('\n', "\r\n")
	} else {
		text.to_string()
	}
}

/// `splitBom` (utils/text.ts): a leading U+FEFF is returned separately so it
/// can be written back verbatim (the model never includes it in `oldText`).
pub fn split_bom(content: &str) -> (&str, &str) {
	match content.strip_prefix('\u{feff}') {
		Some(rest) => ("\u{feff}", rest),
		None => ("", content),
	}
}

/// One targeted replacement (`Edit` upstream).
#[derive(Debug, Clone)]
pub struct Edit {
	pub old_text: String,
	pub new_text: String,
}

/// Apply one or more exact-text replacements to LF-normalized content.
///
/// All edits are matched against the same original content (never
/// incrementally). Each `oldText` must occur exactly once; occurrences are
/// reported as errors with the upstream message shapes. Replacements are
/// applied in reverse match order so offsets remain stable. Returns
/// `(baseContent, newContent)`, both LF-normalized.
pub fn apply_edits_to_normalized_content(
	normalized_content: &str,
	edits: &[Edit],
	path: &str,
) -> Result<(String, String), String> {
	let normalized_edits: Vec<Edit> = edits
		.iter()
		.map(|edit| Edit {
			old_text: normalize_to_lf(&edit.old_text),
			new_text: normalize_to_lf(&edit.new_text),
		})
		.collect();

	let total = normalized_edits.len();
	for (i, edit) in normalized_edits.iter().enumerate() {
		if edit.old_text.is_empty() {
			return Err(empty_old_text_error(path, i, total));
		}
	}

	struct Matched {
		edit_index: usize,
		match_index: usize,
		match_length: usize,
		new_text: String,
	}

	let mut matched: Vec<Matched> = Vec::with_capacity(normalized_edits.len());
	for (i, edit) in normalized_edits.iter().enumerate() {
		let match_index = normalized_content.find(&edit.old_text);
		let Some(match_index) = match_index else {
			return Err(not_found_error(path, i, total));
		};
		let occurrences = normalized_content.matches(&edit.old_text).count();
		if occurrences > 1 {
			return Err(duplicate_error(path, i, total, occurrences));
		}
		matched.push(Matched {
			edit_index: i,
			match_index,
			match_length: edit.old_text.len(),
			new_text: edit.new_text.clone(),
		});
	}

	matched.sort_by_key(|m| m.match_index);
	for pair in matched.windows(2) {
		let (previous, current) = (&pair[0], &pair[1]);
		if previous.match_index + previous.match_length > current.match_index {
			return Err(format!(
				"edits[{}] and edits[{}] overlap in {}. Merge them into one edit or target disjoint regions.",
				previous.edit_index, current.edit_index, path
			));
		}
	}

	let base_content = normalized_content.to_string();
	let mut new_content = base_content.clone();
	for replacement in matched.iter().rev() {
		new_content.replace_range(
			replacement.match_index..replacement.match_index + replacement.match_length,
			&replacement.new_text,
		);
	}

	if base_content == new_content {
		return Err(no_change_error(path, total));
	}

	Ok((base_content, new_content))
}

fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
	if total_edits == 1 {
		return format!(
			"Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
		);
	}
	format!(
		"Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
	)
}

fn duplicate_error(
	path: &str,
	edit_index: usize,
	total_edits: usize,
	occurrences: usize,
) -> String {
	if total_edits == 1 {
		return format!(
			"Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
		);
	}
	format!(
		"Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
	)
}

fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
	if total_edits == 1 {
		return format!("oldText must not be empty in {path}.");
	}
	format!("edits[{edit_index}].oldText must not be empty in {path}.")
}

fn no_change_error(path: &str, total_edits: usize) -> String {
	if total_edits == 1 {
		return format!(
			"No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
		);
	}
	format!("No changes made to {path}. The replacements produced identical content.")
}

// -------------------------------------------------------------------------------------------
// Line diff (stand-in for the `diff` npm package used upstream).
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
	Same(usize, usize), // old index, new index
	Del(usize),         // old index
	Add(usize),         // new index
}

/// Split content into lines the way JS `split("\n")` does: a trailing
/// newline yields a final empty piece.
fn split_lines_js(content: &str) -> Vec<&str> {
	content.split('\n').collect()
}

/// Line diff of `old` vs `new` producing ordered runs (`diffLines` upstream).
/// Common prefix/suffix is trimmed, the middle matched with an LCS dynamic
/// program (bounded; very large middles fall back to a full remove+add).
fn diff_line_ops(old: &[&str], new: &[&str]) -> Vec<Op> {
	let mut ops = Vec::new();
	let mut prefix = 0;
	while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
		ops.push(Op::Same(prefix, prefix));
		prefix += 1;
	}
	let mut suffix = 0;
	while suffix < old.len() - prefix
		&& suffix < new.len() - prefix
		&& old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
	{
		suffix += 1;
	}
	let mid_old = &old[prefix..old.len() - suffix];
	let mid_new = &new[prefix..new.len() - suffix];

	// LCS table (guard against pathological sizes).
	const MAX_CELLS: usize = 4_000_000;
	if !mid_old.is_empty()
		&& !mid_new.is_empty()
		&& mid_old.len().saturating_mul(mid_new.len()) <= MAX_CELLS
	{
		let rows = mid_old.len();
		let cols = mid_new.len();
		let mut table = vec![0u32; (rows + 1) * (cols + 1)];
		let at = |r: usize, c: usize| r * (cols + 1) + c;
		for r in (0..rows).rev() {
			for c in (0..cols).rev() {
				table[at(r, c)] = if mid_old[r] == mid_new[c] {
					table[at(r + 1, c + 1)] + 1
				} else {
					table[at(r + 1, c)].max(table[at(r, c + 1)])
				};
			}
		}
		let mut r = 0;
		let mut c = 0;
		let mut middle = Vec::new();
		while r < rows && c < cols {
			if mid_old[r] == mid_new[c] {
				middle.push(Op::Same(prefix + r, prefix + c));
				r += 1;
				c += 1;
			} else if table[at(r + 1, c)] >= table[at(r, c + 1)] {
				middle.push(Op::Del(prefix + r));
				r += 1;
			} else {
				middle.push(Op::Add(prefix + c));
				c += 1;
			}
		}
		while r < rows {
			middle.push(Op::Del(prefix + r));
			r += 1;
		}
		while c < cols {
			middle.push(Op::Add(prefix + c));
			c += 1;
		}
		// Reorder adjacent del/add runs so deletions precede additions,
		// matching the conventional diff output shape.
		let mut reordered: Vec<Op> = Vec::with_capacity(middle.len());
		let mut i = 0;
		while i < middle.len() {
			if matches!(middle[i], Op::Add(_)) {
				let start = i;
				while i < middle.len() && matches!(middle[i], Op::Add(_)) {
					i += 1;
				}
				let add_end = i;
				let del_start = i;
				while i < middle.len() && matches!(middle[i], Op::Del(_)) {
					i += 1;
				}
				if i > del_start {
					reordered.extend(middle[del_start..i].iter().copied());
				}
				reordered.extend(middle[start..add_end].iter().copied());
			} else {
				reordered.push(middle[i]);
				i += 1;
			}
		}
		ops.extend(reordered);
	} else if !mid_old.is_empty() || !mid_new.is_empty() {
		ops.extend((0..mid_old.len()).map(|i| Op::Del(prefix + i)));
		ops.extend((0..mid_new.len()).map(|i| Op::Add(prefix + i)));
	}
	for s in 0..suffix {
		ops.push(Op::Same(old.len() - suffix + s, new.len() - suffix + s));
	}
	ops
}

/// A run of consecutive diff ops with the same status (`diffLines` part).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiffPartKind {
	Same,
	Added,
	Removed,
}

#[derive(Debug, Clone)]
pub struct DiffPart {
	pub kind: DiffPartKind,
	/// Old-file lines (for Same/Removed) or new-file lines (for Added).
	pub lines: Vec<String>,
}

/// `Diff.diffLines(old, new)` equivalent: ordered parts, each carrying its
/// lines without trailing newlines.
pub fn diff_parts(old: &str, new: &str) -> Vec<DiffPart> {
	let old_lines = split_lines_js(old);
	let new_lines = split_lines_js(new);
	let ops = diff_line_ops(&old_lines, &new_lines);
	let mut parts: Vec<DiffPart> = Vec::new();
	for op in ops {
		let (kind, line) = match op {
			Op::Same(o, _) => (DiffPartKind::Same, old_lines[o]),
			Op::Del(o) => (DiffPartKind::Removed, old_lines[o]),
			Op::Add(n) => (DiffPartKind::Added, new_lines[n]),
		};
		match parts.last_mut() {
			Some(part) if part.kind == kind => part.lines.push(line.to_string()),
			_ => parts.push(DiffPart {
				kind,
				lines: vec![line.to_string()],
			}),
		}
	}
	parts
}

/// `generateUnifiedPatch` equivalent: a standard unified diff with
/// `--- path` / `+++ path` headers and `contextLines` context lines.
///
/// Accepted difference vs upstream: upstream calls jsdiff's
/// `createTwoFilesPatch(..., { headerOptions: FILE_HEADERS_ONLY })`; the
/// exact jsdiff framing (separator rules, hunk header count elision) may
/// differ in details from this standard-format patch.
pub fn generate_unified_patch(
	path: &str,
	old_content: &str,
	new_content: &str,
	context_lines: usize,
) -> String {
	let parts = diff_parts(old_content, new_content);
	if parts.iter().all(|p| p.kind == DiffPartKind::Same) {
		return String::new();
	}

	// Flatten to per-line ops.
	enum Line {
		Same(String),
		Del(String),
		Add(String),
	}
	let old_lines = split_lines_js(old_content);
	let new_lines = split_lines_js(new_content);
	let ops = diff_line_ops(&old_lines, &new_lines);
	let lines: Vec<Line> = ops
		.into_iter()
		.map(|op| match op {
			Op::Same(o, _) => Line::Same(old_lines[o].to_string()),
			Op::Del(o) => Line::Del(old_lines[o].to_string()),
			Op::Add(n) => Line::Add(new_lines[n].to_string()),
		})
		.collect();

	// Hunk ranges: every change expanded by `context_lines` on each side,
	// merged when ranges touch or overlap.
	let is_same = |line: &Line| matches!(line, Line::Same(_));
	let mut ranges: Vec<(usize, usize)> = Vec::new();
	for i in 0..lines.len() {
		if is_same(&lines[i]) {
			continue;
		}
		let start = i.saturating_sub(context_lines);
		let end = (i + context_lines + 1).min(lines.len());
		match ranges.last_mut() {
			Some(last) if start <= last.1 => last.1 = last.1.max(end),
			_ => ranges.push((start, end)),
		}
	}

	let mut out = String::new();
	out.push_str(&format!("--- {path}\n"));
	out.push_str(&format!("+++ {path}\n"));
	for (start, end) in ranges {
		let mut old_no = 1usize;
		let mut new_no = 1usize;
		for line in &lines[..start] {
			match line {
				Line::Add(_) => new_no += 1,
				Line::Del(_) => old_no += 1,
				Line::Same(_) => {
					old_no += 1;
					new_no += 1;
				}
			}
		}
		let old_start = old_no;
		let new_start = new_no;
		let old_count = lines[start..end]
			.iter()
			.filter(|l| !matches!(l, Line::Add(_)))
			.count();
		let new_count = lines[start..end]
			.iter()
			.filter(|l| !matches!(l, Line::Del(_)))
			.count();
		// jsdiff hunk headers always carry the count (even 1); a zero-length
		// side reports the line number before the change.
		let old_header = if old_count == 0 {
			old_start.saturating_sub(1)
		} else {
			old_start
		};
		let new_header = if new_count == 0 {
			new_start.saturating_sub(1)
		} else {
			new_start
		};
		out.push_str(&format!(
			"@@ -{old_header},{old_count} +{new_header},{new_count} @@\n"
		));
		for line in &lines[start..end] {
			match line {
				Line::Same(text) => out.push_str(&format!(" {text}\n")),
				Line::Del(text) => out.push_str(&format!("-{text}\n")),
				Line::Add(text) => out.push_str(&format!("+{text}\n")),
			}
		}
	}
	out
}

/// `generateDiffString` equivalent: a display-oriented diff with padded line
/// numbers and `contextLines` context between changes. Returns the diff text
/// and the first changed line number (in the new file).
pub fn generate_diff_string(
	old_content: &str,
	new_content: &str,
	context_lines: usize,
) -> (String, Option<usize>) {
	let parts = diff_parts(old_content, new_content);
	let mut output: Vec<String> = Vec::new();

	let old_lines = split_lines_js(old_content);
	let new_lines = split_lines_js(new_content);
	let max_line_num = old_lines.len().max(new_lines.len());
	let line_num_width = max_line_num.to_string().len();

	let mut old_line_num = 1usize;
	let mut new_line_num = 1usize;
	let mut last_was_change = false;
	let mut first_changed_line: Option<usize> = None;

	let push_context_line = |output: &mut Vec<String>,
	                         line: &str,
	                         old_line_num: &mut usize,
	                         new_line_num: &mut usize| {
		output.push(format!(
			" {:>width$} {line}",
			old_line_num,
			width = line_num_width
		));
		*old_line_num += 1;
		*new_line_num += 1;
	};

	for (i, part) in parts.iter().enumerate() {
		let raw = &part.lines;
		if matches!(part.kind, DiffPartKind::Added | DiffPartKind::Removed) {
			if first_changed_line.is_none() {
				first_changed_line = Some(new_line_num);
			}
			for line in raw {
				if matches!(part.kind, DiffPartKind::Added) {
					output.push(format!(
						"+{:>width$} {line}",
						new_line_num,
						width = line_num_width
					));
					new_line_num += 1;
				} else {
					output.push(format!(
						"-{:>width$} {line}",
						old_line_num,
						width = line_num_width
					));
					old_line_num += 1;
				}
			}
			last_was_change = true;
		} else {
			let next_part_is_change = i < parts.len() - 1
				&& matches!(
					parts[i + 1].kind,
					DiffPartKind::Added | DiffPartKind::Removed
				);
			let has_leading_change = last_was_change;
			let has_trailing_change = next_part_is_change;

			if has_leading_change && has_trailing_change {
				if raw.len() <= context_lines * 2 {
					for line in raw {
						push_context_line(&mut output, line, &mut old_line_num, &mut new_line_num);
					}
				} else {
					let (leading, rest) = raw.split_at(context_lines);
					let skipped = rest.len() - context_lines;
					for line in leading {
						push_context_line(&mut output, line, &mut old_line_num, &mut new_line_num);
					}
					output.push(format!(" {:>width$} ...", "", width = line_num_width));
					old_line_num += skipped;
					new_line_num += skipped;
					for line in &rest[skipped..] {
						push_context_line(&mut output, line, &mut old_line_num, &mut new_line_num);
					}
				}
			} else if has_leading_change {
				let shown = raw.len().min(context_lines);
				for line in &raw[..shown] {
					push_context_line(&mut output, line, &mut old_line_num, &mut new_line_num);
				}
				let skipped = raw.len() - shown;
				if skipped > 0 {
					output.push(format!(" {:>width$} ...", "", width = line_num_width));
					old_line_num += skipped;
					new_line_num += skipped;
				}
			} else if has_trailing_change {
				let skipped = raw.len().saturating_sub(context_lines);
				if skipped > 0 {
					output.push(format!(" {:>width$} ...", "", width = line_num_width));
					old_line_num += skipped;
					new_line_num += skipped;
				}
				for line in &raw[skipped..] {
					push_context_line(&mut output, line, &mut old_line_num, &mut new_line_num);
				}
			} else {
				// Skip these context lines entirely.
				old_line_num += raw.len();
				new_line_num += raw.len();
			}
			last_was_change = false;
		}
	}

	(output.join("\n"), first_changed_line)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn detects_line_endings() {
		assert_eq!(detect_line_ending("a\r\nb\nc"), "\r\n");
		assert_eq!(detect_line_ending("a\nb\r\nc"), "\n");
		assert_eq!(detect_line_ending("no newlines"), "\n");
		assert_eq!(detect_line_ending(""), "\n");
	}

	#[test]
	fn normalizes_and_restores() {
		assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
		assert_eq!(restore_line_endings("a\nb", "\r\n"), "a\r\nb");
		assert_eq!(restore_line_endings("a\nb", "\n"), "a\nb");
	}

	#[test]
	fn splits_bom() {
		let (bom, text) = split_bom("\u{feff}hello");
		assert_eq!(bom, "\u{feff}");
		assert_eq!(text, "hello");
		let (bom, text) = split_bom("plain");
		assert_eq!(bom, "");
		assert_eq!(text, "plain");
	}

	#[test]
	fn applies_multiple_disjoint_edits() {
		let content = "alpha\nbeta\ngamma\nbeta\ndelta\n".to_string();
		let edits = vec![
			Edit {
				old_text: "alpha".into(),
				new_text: "ALPHA".into(),
			},
			Edit {
				old_text: "gamma\nbeta".into(),
				new_text: "GAMMA\nBETA".into(),
			},
		];
		let (base, new) = apply_edits_to_normalized_content(&content, &edits, "f.txt").unwrap();
		assert_eq!(base, content);
		assert_eq!(new, "ALPHA\nbeta\nGAMMA\nBETA\ndelta\n");
	}

	#[test]
	fn rejects_duplicate_and_overlapping_edits() {
		let content = "one two one".to_string();
		let err = apply_edits_to_normalized_content(
			&content,
			&[Edit {
				old_text: "one".into(),
				new_text: "1".into(),
			}],
			"f.txt",
		)
		.unwrap_err();
		assert!(err.contains("Found 2 occurrences of the text"), "{err}");

		let edits = vec![
			Edit {
				old_text: "one t".into(),
				new_text: "X".into(),
			},
			Edit {
				old_text: "two o".into(),
				new_text: "Y".into(),
			},
		];
		let err = apply_edits_to_normalized_content(&content, &edits, "f.txt").unwrap_err();
		assert!(err.contains("overlap in f.txt"), "{err}");
	}

	#[test]
	fn rejects_missing_empty_and_noop_edits() {
		let err = apply_edits_to_normalized_content(
			"abc",
			&[Edit {
				old_text: "zzz".into(),
				new_text: "y".into(),
			}],
			"f.txt",
		)
		.unwrap_err();
		assert!(
			err.starts_with("Could not find the exact text in f.txt."),
			"{err}"
		);
		let err = apply_edits_to_normalized_content(
			"abc",
			&[Edit {
				old_text: String::new(),
				new_text: "y".into(),
			}],
			"f.txt",
		)
		.unwrap_err();
		assert_eq!(err, "oldText must not be empty in f.txt.");
		let err = apply_edits_to_normalized_content(
			"abc",
			&[Edit {
				old_text: "abc".into(),
				new_text: "abc".into(),
			}],
			"f.txt",
		)
		.unwrap_err();
		assert!(err.starts_with("No changes made to f.txt."), "{err}");
	}

	#[test]
	fn generates_display_diff_with_context() {
		let old = (0..20)
			.map(|i| format!("line{i}"))
			.collect::<Vec<_>>()
			.join("\n");
		let mut new: Vec<String> = (0..20).map(|i| format!("line{i}")).collect();
		new[10] = "changed".into();
		let (diff, first) = generate_diff_string(&old, &new.join("\n"), 4);
		let lines: Vec<&str> = diff.lines().collect();
		// Header lines around the change: line6..line9 context, -line10, +changed.
		assert_eq!(first, Some(11));
		assert!(
			lines
				.iter()
				.any(|l| l.starts_with('-') && l.ends_with("line10")),
			"{diff}"
		);
		assert!(
			lines
				.iter()
				.any(|l| l.starts_with('+') && l.ends_with("changed")),
			"{diff}"
		);
		assert!(lines.iter().any(|l| l.ends_with("line9")), "{diff}");
		assert!(
			lines.iter().any(|l| l.trim_end().ends_with("...")),
			"{diff}"
		);
		// Line numbers are padded to the width of the max line number.
		assert!(lines[0].starts_with("  "));
	}

	#[test]
	fn generates_unified_patch() {
		let old = "a\nb\nc\nd\ne\nf";
		let new = "a\nb\nC\nd\ne\nf";
		let patch = generate_unified_patch("f.txt", old, new, 4);
		assert!(patch.starts_with("--- f.txt\n+++ f.txt\n"), "{patch}");
		assert!(patch.contains("@@ -1,6 +1,6 @@"), "{patch}");
		assert!(patch.contains("-c\n+C\n"));
		// Identical contents produce no hunks.
		assert_eq!(generate_unified_patch("f.txt", old, old, 4), "");
	}

	#[test]
	fn unified_patch_handles_insertion_and_deletion() {
		let patch = generate_unified_patch("f.txt", "a\nb", "a\nb\nX", 4);
		assert!(patch.contains("@@ -1,2 +1,3 @@"), "{patch}");
		assert!(patch.contains("+X"));
		let patch = generate_unified_patch("f.txt", "a\nb\nX", "a\nb", 4);
		assert!(patch.contains("@@ -1,3 +1,2 @@"), "{patch}");
		assert!(patch.contains("-X"));
	}
}
