use unicode_width::UnicodeWidthChar;

/// Greedy word-aware wrap at `width` display columns.
///
/// Breaks on spaces when possible, hard-breaks otherwise, expands tabs to
/// four spaces. Preserves indentation of continuation lines.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let expanded = text.replace('\t', "    ");
    let mut out = Vec::new();
    for raw in expanded.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = raw.chars().collect();
        let mut start = 0;
        while start < chars.len() {
            let mut used = 0usize;
            let mut end = start;
            while end < chars.len() {
                let w = UnicodeWidthChar::width(chars[end]).unwrap_or(0);
                if used + w > width {
                    break;
                }
                used += w;
                end += 1;
            }
            if end == start {
                // A single character wider than the line: emit it anyway.
                end = start + 1;
            }
            if end < chars.len() && chars[end] != ' ' {
                // The cut lands inside a word: back up to the last space in
                // the segment, if there is one.
                if let Some(space) = chars[start..end].iter().rposition(|c| *c == ' ') {
                    if space > 0 {
                        end = start + space;
                    }
                }
            }
            let segment: String = chars[start..end].iter().collect();
            out.push(segment);
            start = end;
            // Skip the single space we broke on.
            if end < chars.len() && chars.get(end) == Some(&' ') {
                start = end + 1;
            }
        }
    }
    out
}

/// Display width of a string.
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(wrap("hello world", 20), vec!["hello world"]);
    }

    #[test]
    fn breaks_on_spaces() {
        assert_eq!(wrap("hello world again", 11), vec!["hello world", "again"]);
    }

    #[test]
    fn hard_breaks_long_words() {
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn preserves_newlines_and_indent() {
        assert_eq!(wrap("a\n  indented", 20), vec!["a", "  indented"]);
    }

    #[test]
    fn expands_tabs() {
        assert_eq!(wrap("\tx", 20), vec!["    x"]);
    }

    #[test]
    fn counts_wide_chars() {
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
    }
}
