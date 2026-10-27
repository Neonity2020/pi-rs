//! Fuzzy matching — port of `pi/packages/tui/src/fuzzy.ts`.
//!
//! Matches if all query characters appear in order (not necessarily
//! consecutive); lower score = better match. Whitespace/slash-separated query
//! tokens must all match.

/// A fuzzy match outcome: whether it matched and its ordering score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
	pub matches: bool,
	pub score: f64,
}

fn word_boundary(prev: char) -> bool {
	prev.is_whitespace() || matches!(prev, '-' | '_' | '.' | '/' | ':')
}

fn score_positions(text_lower: &[char], positions: &[usize]) -> f64 {
	let mut score = 0.0;
	let mut last_match: Option<usize> = None;
	let mut consecutive: i32 = 0;
	for &i in positions {
		if last_match == Some(i.wrapping_sub(1)) {
			consecutive += 1;
			score -= (consecutive * 5) as f64;
		} else {
			consecutive = 0;
			if let Some(last) = last_match {
				score += ((i - last - 1) * 2) as f64;
			}
		}
		if i == 0 || (i > 0 && word_boundary(text_lower[i - 1])) {
			score -= 10.0;
		}
		score += i as f64 * 0.1;
		last_match = Some(i);
	}
	score
}

fn match_query(normalized_query: &[char], text_lower: &[char]) -> FuzzyMatch {
	if normalized_query.is_empty() {
		return FuzzyMatch { matches: true, score: 0.0 };
	}
	if normalized_query.len() > text_lower.len() {
		return FuzzyMatch { matches: false, score: 0.0 };
	}

	// Greedy alignment: each query character at its first occurrence after the
	// previous match.
	let mut positions: Vec<usize> = Vec::with_capacity(normalized_query.len());
	let mut last_match: Option<usize> = None;
	for &qc in normalized_query {
		let start = last_match.map_or(0, |l| l + 1);
		let found = text_lower[start..].iter().position(|&c| c == qc).map(|p| p + start);
		match found {
			Some(i) => {
				positions.push(i);
				last_match = Some(i);
			}
			None => return FuzzyMatch { matches: false, score: 0.0 },
		}
	}

	let mut score = score_positions(text_lower, &positions);

	// The greedy alignment can scatter the query over an earlier, unrelated
	// part of the text. Also score every contiguous occurrence and keep the
	// best alignment.
	let mut start = 0usize;
	while let Some(rel) = find_subsequence(&text_lower[start..], normalized_query) {
		let at = start + rel;
		for (k, p) in positions.iter_mut().enumerate() {
			*p = at + k;
		}
		score = score.min(score_positions(text_lower, &positions));
		start = at + 1;
	}

	if normalized_query == text_lower {
		score -= 100.0;
	}

	FuzzyMatch { matches: true, score }
}

fn find_subsequence(haystack: &[char], needle: &[char]) -> Option<usize> {
	if needle.is_empty() || haystack.len() < needle.len() {
		return None;
	}
	haystack
		.windows(needle.len())
		.position(|window| window == needle)
}

/// Public fuzzy match entry point, with pi's letter/digit swap fallback
/// (e.g. "gpt45" also matches "gpt-4.5"-style ids spelled "4.5gpt"-less ids).
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
	let query_lower: Vec<char> = query.to_lowercase().chars().collect();
	let text_lower: Vec<char> = text.to_lowercase().chars().collect();

	let primary = match_query(&query_lower, &text_lower);
	if primary.matches {
		return primary;
	}

	// Swap a pure letters+digits query (letters then digits or vice versa) and
	// retry, since model ids often reorder those segments.
	let letters: String = query_lower.iter().filter(|c| c.is_ascii_lowercase()).collect();
	let digits: String = query_lower.iter().filter(|c| c.is_ascii_digit()).collect();
	let swapped = if !letters.is_empty() && !digits.is_empty() && letters.len() + digits.len() == query_lower.len() {
		format!("{digits}{letters}")
	} else {
		String::new()
	};
	if swapped.is_empty() {
		return primary;
	}
	let swapped_match = match_query(&swapped.chars().collect::<Vec<_>>(), &text_lower);
	if !swapped_match.matches {
		return primary;
	}
	FuzzyMatch { matches: true, score: swapped_match.score + 5.0 }
}

/// Filter items by fuzzy match quality (best matches first). All
/// whitespace/slash-separated tokens must match.
pub fn fuzzy_filter<'a, I, T>(items: I, query: &str, get_text: impl Fn(&T) -> String) -> Vec<&'a T>
where
	I: IntoIterator<Item = &'a T>,
{
	let query = query.trim();
	if query.is_empty() {
		return items.into_iter().collect();
	}
	let tokens: Vec<&str> = query.split(|c: char| c.is_whitespace() || c == '/').filter(|t| !t.is_empty()).collect();
	if tokens.is_empty() {
		return items.into_iter().collect();
	}

	let mut results: Vec<(&T, f64)> = Vec::new();
	for item in items {
		let text = get_text(item);
		let mut total = 0.0;
		let mut all_match = true;
		for token in &tokens {
			let m = fuzzy_match(token, &text);
			if m.matches {
				total += m.score;
			} else {
				all_match = false;
				break;
			}
		}
		if all_match {
			results.push((item, total));
		}
	}
	results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
	results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn subsequence_matches() {
		assert!(fuzzy_match("opus", "anthropic/claude-opus-4").matches);
		assert!(!fuzzy_match("xyz", "anthropic/claude-opus-4").matches);
	}

	#[test]
	fn empty_query_matches_all() {
		assert!(fuzzy_match("", "anything").matches);
	}

	#[test]
	fn exact_match_scores_best() {
		let exact = fuzzy_match("opus", "opus").score;
		let partial = fuzzy_match("opus", "openrouter opus").score;
		assert!(exact < partial);
	}

	#[test]
	fn tokenized_filter_requires_all_tokens() {
		let items = vec!["openai/gpt-5.2", "anthropic/claude-opus"];
		let got = fuzzy_filter(items.iter(), "openai 5", |s| s.to_string());
		assert_eq!(got.len(), 1);
		assert_eq!(*got[0], "openai/gpt-5.2");
	}
}
