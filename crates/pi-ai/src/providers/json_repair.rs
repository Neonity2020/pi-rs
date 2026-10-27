//! Tool-call argument JSON handling shared by the provider adapters — port of
//! `pi/packages/ai/src/utils/json-parse.ts` (`parseJsonWithRepair`,
//! `repairJson`, `parseToolCallArguments`) and the snapshot/finish semantics of
//! `pi/packages/ai/src/utils/streaming-json.ts` (`ToolCallArgumentsStream`).

use serde_json::{Value, json};

/// Upper bound for walking back over a trailing partial token in
/// [`lenient_partial_json`], so adversarial input cannot turn quadratic.
const MAX_PARTIAL_REPAIR_STEPS: usize = 1024;

/// Port of `parseToolCallArguments` in
/// `pi/packages/ai/src/utils/json-parse.ts`: empty text yields `{}`; text that
/// is not a JSON object is reported via `arguments_error` instead of failing
/// the stream.
pub(crate) fn parse_tool_call_arguments(text: &str) -> (Value, Option<String>) {
	if text.trim().is_empty() {
		return (json!({}), None);
	}
	match parse_json_with_repair(text) {
		Ok(value) if value.is_object() => (value, None),
		Ok(_) => (
			json!({}),
			Some(format!(
				"Tool arguments must be a JSON object\n\nReceived arguments:\n{text}"
			)),
		),
		Err(error) => (
			json!({}),
			Some(format!("{error}\n\nReceived arguments:\n{text}")),
		),
	}
}

pub(crate) fn parse_json_with_repair(text: &str) -> Result<Value, String> {
	let original = match serde_json::from_str::<Value>(text) {
		Ok(value) => return Ok(value),
		Err(error) => error,
	};
	let repaired = repair_json(text);
	if repaired == text {
		return Err(original.to_string());
	}
	serde_json::from_str(&repaired).map_err(|error| error.to_string())
}

/// Port of `repairJson` in `pi/packages/ai/src/utils/json-parse.ts`: escapes
/// raw control characters inside strings and doubles backslashes before
/// invalid escape characters.
fn repair_json(json: &str) -> String {
	let chars: Vec<char> = json.chars().collect();
	let mut repaired = String::with_capacity(json.len());
	let mut in_string = false;
	let mut index = 0;
	while index < chars.len() {
		let current = chars[index];
		if !in_string {
			repaired.push(current);
			if current == '"' {
				in_string = true;
			}
			index += 1;
			continue;
		}
		match current {
			'"' => {
				repaired.push(current);
				in_string = false;
			}
			'\\' => match chars.get(index + 1).copied() {
				None => repaired.push_str("\\\\"),
				Some('u') => {
					let digits: String = chars
						.get(index + 2..index + 6)
						.unwrap_or(&[])
						.iter()
						.collect();
					if digits.chars().count() == 4 && digits.chars().all(|c| c.is_ascii_hexdigit())
					{
						repaired.push_str("\\u");
						repaired.push_str(&digits);
						index += 5;
					} else {
						repaired.push_str("\\\\");
					}
				}
				Some(next) if matches!(next, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => {
					repaired.push('\\');
					repaired.push(next);
					index += 1;
				}
				Some(_) => repaired.push_str("\\\\"),
			},
			control if (control as u32) < 0x20 => repaired.push_str(&escape_control(control)),
			other => repaired.push(other),
		}
		index += 1;
	}
	repaired
}

fn escape_control(character: char) -> String {
	match character {
		'\u{8}' => "\\b".to_string(),
		'\u{c}' => "\\f".to_string(),
		'\n' => "\\n".to_string(),
		'\r' => "\\r".to_string(),
		'\t' => "\\t".to_string(),
		other => format!("\\u{:04x}", other as u32),
	}
}

/// Best-effort preview of streamed tool-call arguments: closes strings and
/// containers left open by the partial text and drops dangling partial
/// members, mirroring the snapshot rules of `StreamingJsonParser` in
/// `pi/packages/ai/src/utils/streaming-json.ts`. Non-object previews collapse
/// to `{}` like `ToolCallArgumentsStream::partial`; the authoritative value is
/// parsed once by [`parse_tool_call_arguments`] when the call finishes.
pub(crate) fn lenient_partial_json(text: &str) -> Value {
	let bytes = text.as_bytes();
	let mut stack: Vec<u8> = Vec::new();
	let mut in_string = false;
	let mut escaped = false;
	for &byte in bytes {
		if in_string {
			if escaped {
				escaped = false;
			} else {
				match byte {
					b'\\' => escaped = true,
					b'"' => in_string = false,
					_ => {}
				}
			}
		} else {
			match byte {
				b'"' => in_string = true,
				b'{' | b'[' => stack.push(byte),
				b'}' | b']' => {
					stack.pop();
				}
				_ => {}
			}
		}
	}
	// Walk back over a trailing partial token (an unterminated literal, say)
	// until some prefix closes into valid JSON.
	let mut end = bytes.len();
	for _ in 0..=MAX_PARTIAL_REPAIR_STEPS {
		if let Some(value) = closed_prefix(bytes, end, in_string, &stack) {
			return if value.is_object() { value } else { json!({}) };
		}
		if end == 0 {
			break;
		}
		end -= 1;
		while end > 0 && !text.is_char_boundary(end) {
			end -= 1;
		}
	}
	json!({})
}

fn closed_prefix(bytes: &[u8], end: usize, in_string: bool, stack: &[u8]) -> Option<Value> {
	let mut candidate = Vec::with_capacity(end + stack.len() + 2);
	candidate.extend_from_slice(&bytes[..end]);
	if in_string {
		candidate.push(b'"');
	}
	while matches!(
		candidate.last(),
		Some(b',') | Some(b':') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
	) {
		candidate.pop();
	}
	for &open in stack.iter().rev() {
		candidate.push(if open == b'{' { b'}' } else { b']' });
	}
	serde_json::from_slice(&candidate).ok()
}
