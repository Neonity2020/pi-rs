//! Frontmatter parsing — port of `pi/packages/coding-agent/src/utils/frontmatter.ts`.
//!
//! A file starts with frontmatter when it begins with `---` and a later line
//! closes with `---` (`\n---`); the YAML between the fences parses into a
//! mapping and the body is the text after the closing fence, trimmed. No
//! frontmatter (or an unterminated one) yields an empty mapping and the whole
//! (normalized) content as the body.

/// Parsed frontmatter: the YAML mapping (empty when absent) and the body.
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
	pub data: serde_yaml::Mapping,
	pub body: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FrontmatterError(pub String);

impl Frontmatter {
	/// String value of a frontmatter key (non-string values are `None`,
	/// matching upstream's `typeof check === "string"` guards).
	pub fn get_str(&self, key: &str) -> Option<&str> {
		self.data
			.get(serde_yaml::Value::String(key.into()))
			.and_then(|value| value.as_str())
	}

	/// Boolean value of a frontmatter key.
	pub fn get_bool(&self, key: &str) -> Option<bool> {
		self.data
			.get(serde_yaml::Value::String(key.into()))
			.and_then(|value| value.as_bool())
	}
}

/// `stripBom` (utils/text.ts): a leading U+FEFF is dropped, then CRLF/CR
/// normalize to LF.
fn normalize(content: &str) -> String {
	content
		.trim_start_matches('\u{feff}')
		.replace("\r\n", "\n")
		.replace('\r', "\n")
}

/// `extractFrontmatter`: split `---\nYAML\n---\n…` into the YAML string and
/// the body. `yaml_string` is `None` when there is no (complete) frontmatter.
fn extract(content: &str) -> (Option<String>, String) {
	let normalized = normalize(content);
	if !normalized.starts_with("---") {
		return (None, normalized);
	}
	let Some(end) = normalized[3..].find("\n---").map(|i| i + 3) else {
		return (None, normalized);
	};
	// The opening fence is `---` + one newline character (byte 4 upstream);
	// all offsets are ASCII so byte slicing is safe. `---\n---` puts the
	// closing fence at byte 3 — upstream's `slice(4, 3)` degrades to an empty
	// YAML document, so clamp instead of panicking.
	let yaml = normalized[4..end.max(4)].to_string();
	let body = normalized[end + 4..].trim().to_string();
	(Some(yaml), body)
}

/// `parseFrontmatter`: parse the YAML block; a parse error is returned (the
/// caller decides whether it is fatal or a diagnostic). An empty/`null` YAML
/// document yields an empty mapping.
pub fn parse_frontmatter(content: &str) -> Result<Frontmatter, FrontmatterError> {
	let (yaml_string, body) = extract(content);
	let Some(yaml) = yaml_string else {
		return Ok(Frontmatter {
			data: serde_yaml::Mapping::default(),
			body,
		});
	};
	let value: serde_yaml::Value =
		serde_yaml::from_str(&yaml).map_err(|error| FrontmatterError(error.to_string()))?;
	let data = match value {
		serde_yaml::Value::Null => serde_yaml::Mapping::default(),
		serde_yaml::Value::Mapping(map) => map,
		// A scalar/sequence document has no properties for our lookups
		// (upstream `(parsed ?? {}) as T` degrades the same way).
		_ => serde_yaml::Mapping::default(),
	};
	Ok(Frontmatter { data, body })
}

/// `stripFrontmatter`: the body only (errors degrade to the raw content —
/// upstream `stripFrontmatter` would throw on bad YAML, but the only call
/// site (`_expandSkillCommand`) treats any failure as pass-through; degrading
/// keeps that path infallible, a declared deviation).
pub fn strip_frontmatter(content: &str) -> String {
	parse_frontmatter(content)
		.map(|fm| fm.body)
		.unwrap_or_else(|_| normalize(content))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn without_frontmatter_returns_empty_map_and_full_body() {
		let fm = parse_frontmatter("just text\nmore").unwrap();
		assert!(fm.data.is_empty());
		assert_eq!(fm.body, "just text\nmore");
	}

	#[test]
	fn unterminated_fence_is_not_frontmatter() {
		let fm = parse_frontmatter("---\nname: x\nno closer").unwrap();
		assert!(fm.data.is_empty());
		assert_eq!(fm.body, "---\nname: x\nno closer");
	}

	#[test]
	fn parses_yaml_and_trims_body() {
		let fm = parse_frontmatter("---\nname: my-skill\ndescription: does things\ndisable-model-invocation: true\n---\n\nBody here.\n").unwrap();
		assert_eq!(fm.get_str("name"), Some("my-skill"));
		assert_eq!(fm.get_str("description"), Some("does things"));
		assert_eq!(fm.get_bool("disable-model-invocation"), Some(true));
		assert_eq!(fm.body, "Body here.");
	}

	#[test]
	fn bad_yaml_is_an_error_not_a_panic() {
		let error = parse_frontmatter("---\n: : :\n  - {\n---\nbody").unwrap_err();
		assert!(!error.0.is_empty());
	}

	#[test]
	fn normalizes_newlines_and_strips_bom() {
		let fm = parse_frontmatter("\u{feff}---\r\nname: x\r\n---\r\nbody\r\n").unwrap();
		assert_eq!(fm.get_str("name"), Some("x"));
		assert_eq!(fm.body, "body");
	}

	#[test]
	fn empty_yaml_document_maps_to_empty_frontmatter() {
		let fm = parse_frontmatter("---\n---\nbody").unwrap();
		assert!(fm.data.is_empty());
		assert_eq!(fm.body, "body");
	}

	#[test]
	fn strip_frontmatter_returns_body_and_degrades_on_bad_yaml() {
		assert_eq!(strip_frontmatter("---\nname: x\n---\nkeep me"), "keep me");
		// Bad YAML: the normalized raw content comes back (pass-through).
		assert!(strip_frontmatter("---\n: : {\n---\nkeep").contains("keep"));
		assert_eq!(strip_frontmatter("plain"), "plain");
	}
}
