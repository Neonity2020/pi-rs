//! Session persistence — port of
//! `pi/packages/coding-agent/src/core/session-manager.ts` in the upstream
//! **v3 wire format** (`docs/session-format.md`).
//!
//! The log is append-only JSONL: a `session` header line, then one entry per
//! recorded event. Entries form a tree via `id`/`parentId` (upstream v2+); the
//! manager tracks the leaf, and every append becomes a child of it. Context
//! rebuild walks the leaf-to-root path (`project_entries`, the port of
//! `buildContextEntries` + `buildSessionProjection`), honouring the newest
//! compaction and the latest `context_edit` per target.
//!
//! Wire compatibility:
//! - v3 files are read and written directly (11 upstream entry types; unknown
//!   entry types round-trip verbatim through [`EntryKind::Unknown`]).
//! - v1/v2 files are migrated on open exactly like upstream
//!   `migrateToCurrentVersion` (v1→v2 adds id/parentId and converts
//!   `firstKeptEntryIndex`; v2→v3 renames the `hookMessage` message role to
//!   `custom`) and the file is rewritten in place.
//! - Legacy pi-rs files (old `type:"header"` flat log) are detected and
//!   rejected with a clear error; they are not migrated (the pre-v3 Rust
//!   layout never shipped, so no data migration is warranted).
//!
//! Layout deviation (accepted, pre-existing): one directory per session,
//! `session.jsonl` inside, ids from directory names — upstream stores one
//! `<timestamp>_<id>.jsonl` file per cwd-grouped directory. The header
//! carries a real UUIDv7 id so upstream-shaped consumers can identify the
//! session; resume-by-id still keys on directory names.
//!
//! Durability: each entry is written and fsynced (`sync_data`) before
//! `append` returns — deliberately stronger than upstream's `appendFileSync`
//! (see the P0 audit). The log is ordered but not transactional: a crash
//! mid-write can leave a partial final line; reopening terminates it with a
//! newline and readers skip lines that fail to parse.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use pi_ai::{Message, Usage};

use crate::config;

/// Upstream `CURRENT_SESSION_VERSION` (`session-manager.ts:41`).
pub const CURRENT_SESSION_VERSION: u32 = 3;

/// Verbatim from `core/messages.ts` (`BRANCH_SUMMARY_PREFIX`/`_SUFFIX`): the
/// LLM-visible form of a replayed `branch_summary` entry.
pub const BRANCH_SUMMARY_PREFIX: &str =
	"The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// `SessionHeader` upstream (`session-manager.ts:43-50`): the first line,
/// metadata only — not part of the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionHeader {
	/// v1 sessions carry no version field (reads as 1).
	pub version: Option<u32>,
	pub id: String,
	/// ISO 8601 timestamp.
	pub timestamp: String,
	pub cwd: String,
	/// Path of the parent session (`/fork`/`/clone` lineage).
	pub parent_session: Option<String>,
}

impl SessionHeader {
	pub fn effective_version(&self) -> u32 {
		self.version.unwrap_or(1)
	}

	pub fn to_value(&self) -> Value {
		let mut object = json!({
			"type": "session",
			"version": self.version.unwrap_or(CURRENT_SESSION_VERSION),
			"id": self.id,
			"timestamp": self.timestamp,
			"cwd": self.cwd,
		});
		if let Some(parent) = &self.parent_session {
			object["parentSession"] = Value::String(parent.clone());
		}
		object
	}

	fn from_value(value: &Value) -> Option<Self> {
		Some(Self {
			version: value
				.get("version")
				.and_then(Value::as_u64)
				.map(|v| v as u32),
			id: value.get("id")?.as_str()?.to_string(),
			timestamp: value
				.get("timestamp")
				.and_then(Value::as_str)
				.unwrap_or_default()
				.to_string(),
			cwd: value
				.get("cwd")
				.and_then(Value::as_str)
				.unwrap_or_default()
				.to_string(),
			parent_session: value
				.get("parentSession")
				.and_then(Value::as_str)
				.map(str::to_owned),
		})
	}
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

/// `SessionEntryBase` upstream: tree linkage + entry timestamp (ISO 8601).
/// Fields are optional on the in-memory type and filled by
/// [`SessionManager::append`] right before the line is written, so written
/// entries always carry the full upstream shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntryBase {
	pub id: Option<String>,
	pub parent_id: Option<String>,
	pub timestamp: Option<String>,
}

/// Content replacement of a `context_edit` entry — upstream
/// `ContextEditEntry.replacement` is `null` (omit) or `{ content }` (replace
/// the target message content only).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextReplacement {
	pub content: Value,
}

/// One session log entry. Variants mirror the 11 upstream entry types
/// (`session-manager.ts:57-194`); the `type` tag and the camelCase field
/// names match the wire format exactly. `Unknown` preserves entries from
/// newer upstream versions or extensions verbatim (append-only passthrough).
///
/// [`EntryKind::BashExecution`] is the twelfth shape pi-rs writes itself: it
/// serializes to the upstream *message* entry with `message.role ==
/// "bashExecution"` (`BashExecutionMessage` in `core/messages.ts`), which our
/// [`Message`] union cannot represent as a variant.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
	/// `message`: an `AgentMessage`. Messages our `Message` union cannot
	/// represent (e.g. the upstream `custom` role) fall back to `Unknown`.
	Message { message: Box<Message> },
	/// `model_change`: `{ provider, modelId }` (upstream shape — not a key).
	ModelChange { provider: String, model_id: String },
	/// `thinking_level_change`.
	ThinkingLevelChange { thinking_level: String },
	/// `usage`: model-attributed usage that never enters LLM context.
	Usage {
		kind: String,
		provider: String,
		model: String,
		usage: Usage,
		note: Option<String>,
	},
	/// `compaction` (`CompactionEntry`): the summary replaces entries before
	/// `first_kept_entry_id`. `firstKeptEntryId ?? id` upstream — the
	/// keep-nothing form stores the entry's own id.
	Compaction {
		summary: String,
		first_kept_entry_id: String,
		tokens_before: u64,
		details: Option<Value>,
		usage: Option<Usage>,
		from_hook: Option<bool>,
		system_message: Option<pi_ai::SystemMessage>,
	},
	/// `branch_summary` (`BranchSummaryEntry`).
	BranchSummary {
		from_id: String,
		summary: String,
		details: Option<Value>,
		usage: Option<Usage>,
		from_hook: Option<bool>,
	},
	/// `custom` (`CustomEntry`): extension state; never enters LLM context.
	Custom {
		custom_type: String,
		data: Option<Value>,
	},
	/// `custom_message` (`CustomMessageEntry`): extension-injected context.
	CustomMessage {
		custom_type: String,
		content: Value,
		display: bool,
		details: Option<Value>,
	},
	/// `context_edit`: append-only edit of one earlier entry's context
	/// contribution. `None` replacement omits the target.
	ContextEdit {
		target_id: String,
		replacement: Option<ContextReplacement>,
	},
	/// `label`: user bookmark on an entry (`None` clears).
	Label {
		target_id: String,
		label: Option<String>,
	},
	/// `session_info`: display name metadata.
	SessionInfo { name: Option<String> },
	/// Upstream `bashExecution` message (`core/messages.ts:29`), persisted as
	/// a *message* entry — `{"type":"message","message":{"role":
	/// "bashExecution", ...}}`. `exit_code` is `None` when the command was
	/// killed/cancelled; `exclude_from_context` mirrors the `!!` prefix.
	BashExecution {
		command: String,
		output: String,
		exit_code: Option<i64>,
		cancelled: bool,
		truncated: bool,
		full_output_path: Option<String>,
		exclude_from_context: bool,
	},
	/// Unrecognized entry (newer upstream, extension, or a known type whose
	/// fields failed to parse). Preserved verbatim; contributes no context.
	Unknown(Value),
}

impl EntryKind {
	pub fn type_name(&self) -> &'static str {
		match self {
			EntryKind::Message { .. } => "message",
			EntryKind::ModelChange { .. } => "model_change",
			EntryKind::ThinkingLevelChange { .. } => "thinking_level_change",
			EntryKind::Usage { .. } => "usage",
			EntryKind::Compaction { .. } => "compaction",
			EntryKind::BranchSummary { .. } => "branch_summary",
			EntryKind::Custom { .. } => "custom",
			EntryKind::CustomMessage { .. } => "custom_message",
			EntryKind::ContextEdit { .. } => "context_edit",
			EntryKind::Label { .. } => "label",
			EntryKind::SessionInfo { .. } => "session_info",
			// Upstream wire type is `message` (a message entry whose role is
			// `bashExecution`), not a distinct `type` tag.
			EntryKind::BashExecution { .. } => "message",
			EntryKind::Unknown(_) => "unknown",
		}
	}
}

/// A typed session entry: tree base + content.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
	pub base: EntryBase,
	pub kind: EntryKind,
}

impl SessionEntry {
	/// Content-only constructor; id/parentId/timestamp are assigned by
	/// [`SessionManager::append`].
	pub fn new(kind: EntryKind) -> Self {
		Self {
			base: EntryBase::default(),
			kind,
		}
	}

	/// Compaction entry with a fresh pre-assigned id; keep-nothing form
	/// (`firstKeptEntryId = id`, upstream `appendCompaction`'s
	/// `firstKeptEntryId ?? id`).
	pub fn compaction(summary: impl Into<String>, tokens_before: u64) -> Self {
		let id = generate_entry_id(&|_| false);
		Self::new(EntryKind::Compaction {
			summary: summary.into(),
			first_kept_entry_id: id.clone(),
			tokens_before,
			details: None,
			usage: None,
			from_hook: None,
			system_message: None,
		})
		.with_id(id)
	}

	pub fn model_change(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
		Self::new(EntryKind::ModelChange {
			provider: provider.into(),
			model_id: model_id.into(),
		})
	}

	pub fn thinking_level_change(level: impl Into<String>) -> Self {
		Self::new(EntryKind::ThinkingLevelChange {
			thinking_level: level.into(),
		})
	}

	pub fn context_edit(target_id: impl Into<String>) -> Self {
		Self::new(EntryKind::ContextEdit {
			target_id: target_id.into(),
			replacement: None,
		})
	}

	pub fn with_id(mut self, id: String) -> Self {
		self.base.id = Some(id);
		self
	}

	pub fn id(&self) -> Option<&str> {
		self.base.id.as_deref()
	}

	pub fn parent_id(&self) -> Option<&str> {
		self.base.parent_id.as_deref()
	}

	/// Serialize to the upstream wire shape (one JSON object per line).
	pub fn to_value(&self) -> Value {
		let mut object = Map::new();
		object.insert("type".into(), Value::String(self.kind.type_name().into()));
		if let Some(id) = &self.base.id {
			object.insert("id".into(), Value::String(id.clone()));
		}
		object.insert(
			"parentId".into(),
			self.base
				.parent_id
				.clone()
				.map(Value::String)
				.unwrap_or(Value::Null),
		);
		if let Some(timestamp) = &self.base.timestamp {
			object.insert("timestamp".into(), Value::String(timestamp.clone()));
		}
		let mut value = Value::Object(object);
		match &self.kind {
			EntryKind::Message { message } => {
				value["message"] = serde_json::to_value(message.as_ref()).unwrap_or(Value::Null);
			}
			EntryKind::ModelChange { provider, model_id } => {
				value["provider"] = Value::String(provider.clone());
				value["modelId"] = Value::String(model_id.clone());
			}
			EntryKind::ThinkingLevelChange { thinking_level } => {
				value["thinkingLevel"] = Value::String(thinking_level.clone());
			}
			EntryKind::Usage {
				kind,
				provider,
				model,
				usage,
				note,
			} => {
				value["kind"] = Value::String(kind.clone());
				value["provider"] = Value::String(provider.clone());
				value["model"] = Value::String(model.clone());
				value["usage"] = serde_json::to_value(usage).unwrap_or(Value::Null);
				if let Some(note) = note {
					value["note"] = Value::String(note.clone());
				}
			}
			EntryKind::Compaction {
				summary,
				first_kept_entry_id,
				tokens_before,
				details,
				usage,
				from_hook,
				system_message,
			} => {
				value["summary"] = Value::String(summary.clone());
				value["firstKeptEntryId"] = Value::String(first_kept_entry_id.clone());
				value["tokensBefore"] = json!(tokens_before);
				if let Some(details) = details {
					value["details"] = details.clone();
				}
				if let Some(usage) = usage {
					value["usage"] = serde_json::to_value(usage).unwrap_or(Value::Null);
				}
				if let Some(from_hook) = from_hook {
					value["fromHook"] = json!(from_hook);
				}
				if let Some(system_message) = system_message {
					value["systemMessage"] =
						serde_json::to_value(system_message).unwrap_or(Value::Null);
				}
			}
			EntryKind::BranchSummary {
				from_id,
				summary,
				details,
				usage,
				from_hook,
			} => {
				value["fromId"] = Value::String(from_id.clone());
				value["summary"] = Value::String(summary.clone());
				if let Some(details) = details {
					value["details"] = details.clone();
				}
				if let Some(usage) = usage {
					value["usage"] = serde_json::to_value(usage).unwrap_or(Value::Null);
				}
				if let Some(from_hook) = from_hook {
					value["fromHook"] = json!(from_hook);
				}
			}
			EntryKind::Custom { custom_type, data } => {
				value["customType"] = Value::String(custom_type.clone());
				if let Some(data) = data {
					value["data"] = data.clone();
				}
			}
			EntryKind::CustomMessage {
				custom_type,
				content,
				display,
				details,
			} => {
				value["customType"] = Value::String(custom_type.clone());
				value["content"] = content.clone();
				value["display"] = json!(display);
				if let Some(details) = details {
					value["details"] = details.clone();
				}
			}
			EntryKind::ContextEdit {
				target_id,
				replacement,
			} => {
				value["targetId"] = Value::String(target_id.clone());
				value["replacement"] = match replacement {
					None => Value::Null,
					Some(replacement) => json!({ "content": replacement.content }),
				};
			}
			EntryKind::Label { target_id, label } => {
				value["targetId"] = Value::String(target_id.clone());
				if let Some(label) = label {
					value["label"] = Value::String(label.clone());
				}
			}
			EntryKind::SessionInfo { name } => {
				if let Some(name) = name {
					value["name"] = Value::String(name.clone());
				}
			}
			EntryKind::BashExecution {
				command,
				output,
				exit_code,
				cancelled,
				truncated,
				full_output_path,
				exclude_from_context,
			} => {
				let mut message = json!({
					"role": "bashExecution",
					"command": command,
					"output": output,
					"exitCode": exit_code,
					"cancelled": cancelled,
					"truncated": truncated,
					"timestamp": pi_ai::types::now_ms(),
				});
				if let Some(path) = full_output_path {
					message["fullOutputPath"] = Value::String(path.clone());
				}
				if *exclude_from_context {
					message["excludeFromContext"] = json!(true);
				}
				value["message"] = message;
			}
			EntryKind::Unknown(raw) => return raw.clone(),
		}
		value
	}

	/// Parse one wire entry. `None` for the header type (handled separately).
	pub fn from_value(value: &Value) -> Option<Self> {
		let kind = match value.get("type").and_then(Value::as_str)? {
			"session" => return None,
			"message" => {
				match value
					.get("message")
					.map(|message| serde_json::from_value::<Message>(message.clone()))
				{
					Some(Ok(message)) => EntryKind::Message {
						message: Box::new(message),
					},
					// Our Message union cannot represent the upstream
					// `bashExecution` role; parse it into the typed variant
					// (round-trips the exact upstream field names).
					_ => parse_bash_execution_message(value)
						.unwrap_or_else(|| EntryKind::Unknown(value.clone())),
				}
			}
			"model_change" => match (
				value.get("provider").and_then(Value::as_str),
				value.get("modelId").and_then(Value::as_str),
			) {
				(Some(provider), Some(model_id)) => EntryKind::ModelChange {
					provider: provider.to_string(),
					model_id: model_id.to_string(),
				},
				_ => EntryKind::Unknown(value.clone()),
			},
			"thinking_level_change" => match value.get("thinkingLevel").and_then(Value::as_str) {
				Some(level) => EntryKind::ThinkingLevelChange {
					thinking_level: level.to_string(),
				},
				None => EntryKind::Unknown(value.clone()),
			},
			"usage" => {
				let parsed_usage = value
					.get("usage")
					.map(|usage| serde_json::from_value::<Usage>(usage.clone()));
				match (
					value.get("kind").and_then(Value::as_str),
					value.get("provider").and_then(Value::as_str),
					value.get("model").and_then(Value::as_str),
					parsed_usage,
				) {
					(Some(kind), Some(provider), Some(model), Some(Ok(usage))) => {
						EntryKind::Usage {
							kind: kind.to_string(),
							provider: provider.to_string(),
							model: model.to_string(),
							usage,
							note: value.get("note").and_then(Value::as_str).map(str::to_owned),
						}
					}
					_ => EntryKind::Unknown(value.clone()),
				}
			}
			"compaction" => match (
				value.get("summary").and_then(Value::as_str),
				value.get("firstKeptEntryId").and_then(Value::as_str),
				value.get("tokensBefore").and_then(Value::as_u64),
			) {
				(Some(summary), Some(first_kept), Some(tokens_before)) => EntryKind::Compaction {
					summary: summary.to_string(),
					first_kept_entry_id: first_kept.to_string(),
					tokens_before,
					details: value.get("details").cloned().filter(Value::is_object),
					usage: value
						.get("usage")
						.and_then(|usage| serde_json::from_value::<Usage>(usage.clone()).ok()),
					from_hook: value.get("fromHook").and_then(Value::as_bool),
					system_message: value.get("systemMessage").and_then(|system| {
						serde_json::from_value::<pi_ai::SystemMessage>(system.clone()).ok()
					}),
				},
				_ => EntryKind::Unknown(value.clone()),
			},
			"branch_summary" => match (
				value.get("fromId").and_then(Value::as_str),
				value.get("summary").and_then(Value::as_str),
			) {
				(Some(from_id), Some(summary)) => EntryKind::BranchSummary {
					from_id: from_id.to_string(),
					summary: summary.to_string(),
					details: value.get("details").cloned().filter(Value::is_object),
					usage: value
						.get("usage")
						.and_then(|usage| serde_json::from_value::<Usage>(usage.clone()).ok()),
					from_hook: value.get("fromHook").and_then(Value::as_bool),
				},
				_ => EntryKind::Unknown(value.clone()),
			},
			"custom" => match value.get("customType").and_then(Value::as_str) {
				Some(custom_type) => EntryKind::Custom {
					custom_type: custom_type.to_string(),
					data: value.get("data").cloned(),
				},
				None => EntryKind::Unknown(value.clone()),
			},
			"custom_message" => match value.get("customType").and_then(Value::as_str) {
				Some(custom_type) => EntryKind::CustomMessage {
					custom_type: custom_type.to_string(),
					content: value.get("content").cloned().unwrap_or(Value::Null),
					display: value
						.get("display")
						.and_then(Value::as_bool)
						.unwrap_or(true),
					details: value.get("details").cloned().filter(Value::is_object),
				},
				None => EntryKind::Unknown(value.clone()),
			},
			"context_edit" => match value.get("targetId").and_then(Value::as_str) {
				Some(target_id) => EntryKind::ContextEdit {
					target_id: target_id.to_string(),
					replacement: parse_context_replacement(value.get("replacement")),
				},
				None => EntryKind::Unknown(value.clone()),
			},
			"label" => EntryKind::Label {
				target_id: value
					.get("targetId")
					.and_then(Value::as_str)
					.unwrap_or_default()
					.to_string(),
				label: value
					.get("label")
					.and_then(Value::as_str)
					.map(str::to_owned),
			},
			"session_info" => EntryKind::SessionInfo {
				name: value.get("name").and_then(Value::as_str).map(str::to_owned),
			},
			_ => EntryKind::Unknown(value.clone()),
		};
		Some(Self {
			base: EntryBase {
				id: value.get("id").and_then(Value::as_str).map(str::to_owned),
				parent_id: match value.get("parentId") {
					Some(Value::String(parent)) => Some(parent.clone()),
					_ => None,
				},
				timestamp: value
					.get("timestamp")
					.and_then(Value::as_str)
					.map(str::to_owned),
			},
			kind,
		})
	}
}

fn parse_context_replacement(value: Option<&Value>) -> Option<ContextReplacement> {
	match value {
		None | Some(Value::Null) => None,
		Some(value) => value
			.get("content")
			.map(|content| ContextReplacement {
				content: content.clone(),
			})
			.or_else(|| {
				Some(ContextReplacement {
					content: value.clone(),
				})
			}),
	}
}

/// Parse an upstream `bashExecution` message entry (`BashExecutionMessage` in
/// `core/messages.ts`): required `command`/`output`; everything else follows
/// the upstream optionality (`exitCode` null, missing `fullOutputPath`).
fn parse_bash_execution_message(value: &Value) -> Option<EntryKind> {
	let message = value.get("message")?;
	if message.get("role").and_then(Value::as_str) != Some("bashExecution") {
		return None;
	}
	let command = message.get("command").and_then(Value::as_str)?;
	let output = message.get("output").and_then(Value::as_str)?;
	Some(EntryKind::BashExecution {
		command: command.to_string(),
		output: output.to_string(),
		exit_code: message.get("exitCode").and_then(Value::as_i64),
		cancelled: message
			.get("cancelled")
			.and_then(Value::as_bool)
			.unwrap_or(false),
		truncated: message
			.get("truncated")
			.and_then(Value::as_bool)
			.unwrap_or(false),
		full_output_path: message
			.get("fullOutputPath")
			.and_then(Value::as_str)
			.map(str::to_owned),
		exclude_from_context: message
			.get("excludeFromContext")
			.and_then(Value::as_bool)
			.unwrap_or(false),
	})
}

/// `bashExecutionToText` (`core/messages.ts:82`): the user-message text a
/// non-excluded bash execution contributes to the model context.
pub fn bash_execution_to_text(
	command: &str,
	output: &str,
	exit_code: Option<i64>,
	cancelled: bool,
	truncated: bool,
	full_output_path: Option<&str>,
) -> String {
	let mut text = format!("Ran `{command}`\n");
	if output.is_empty() {
		text.push_str("(no output)");
	} else {
		text.push_str(&format!("```\n{output}\n```"));
	}
	if cancelled {
		text.push_str("\n\n(command cancelled)");
	} else if let Some(code) = exit_code
		&& code != 0
	{
		text.push_str(&format!("\n\nCommand exited with code {code}"));
	}
	if truncated && let Some(path) = full_output_path {
		text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
	}
	text
}

// ---------------------------------------------------------------------------
// Tree traversal (`getTree` / `getEntry` / `getChildren`)
// ---------------------------------------------------------------------------

/// `SessionTreeNode` upstream (`session-manager.ts:198`): one entry plus its
/// children in the session tree. Deviation: the upstream `label`/
/// `labelTimestamp` fields are omitted — pi-rs parses `label` entries but
/// keeps no resolved-label map, so there is nothing to resolve.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTreeNode {
	pub entry: SessionEntry,
	pub children: Vec<SessionTreeNode>,
}

impl SessionTreeNode {
	/// Serialize to the upstream wire shape (`{entry, children}`).
	pub fn to_value(&self) -> Value {
		json!({
			"entry": self.entry.to_value(),
			"children": self.children.iter().map(|node| node.to_value()).collect::<Vec<_>>(),
		})
	}
}

// ---------------------------------------------------------------------------
// Migrations (upstream `migrateV1ToV2` / `migrateV2ToV3`, Value-level)
// ---------------------------------------------------------------------------

/// Bring raw file entries to the current version, mirroring upstream
/// `migrateToCurrentVersion`. Returns true when any migration was applied.
fn migrate_to_current_version(entries: &mut [Value]) -> bool {
	let version = entries
		.iter()
		.find(|entry| entry.get("type").and_then(Value::as_str) == Some("session"))
		.and_then(|header| header.get("version"))
		.and_then(Value::as_u64)
		.unwrap_or(1) as u32;
	if version >= CURRENT_SESSION_VERSION {
		return false;
	}
	if version < 2 {
		migrate_v1_to_v2(entries);
	}
	if version < 3 {
		migrate_v2_to_v3(entries);
	}
	true
}

/// v1 → v2: add id/parentId tree structure; convert `firstKeptEntryIndex` to
/// `firstKeptEntryId` on compaction entries. Index arithmetic matches
/// upstream: `entries` includes the header at position 0.
fn migrate_v1_to_v2(entries: &mut [Value]) {
	let mut seen: HashSet<String> = HashSet::new();
	let mut previous_id: Option<String> = None;
	for entry in entries.iter_mut() {
		if entry.get("type").and_then(Value::as_str) == Some("session") {
			entry["version"] = json!(2);
			continue;
		}
		let id = loop {
			let candidate = generate_entry_id(&|id| seen.contains(id));
			if !seen.contains(&candidate) {
				break candidate;
			}
		};
		seen.insert(id.clone());
		entry["id"] = Value::String(id.clone());
		entry["parentId"] = previous_id
			.clone()
			.map(Value::String)
			.unwrap_or(Value::Null);
		previous_id = Some(id);
	}
	// Second pass: firstKeptEntryIndex → firstKeptEntryId (ids are assigned).
	let ids: Vec<Option<String>> = entries
		.iter()
		.map(|entry| entry.get("id").and_then(Value::as_str).map(str::to_owned))
		.collect();
	let session_lines: HashSet<usize> = entries
		.iter()
		.enumerate()
		.filter(|(_, entry)| entry.get("type").and_then(Value::as_str) == Some("session"))
		.map(|(index, _)| index)
		.collect();
	for entry in entries.iter_mut() {
		if entry.get("type").and_then(Value::as_str) != Some("compaction") {
			continue;
		}
		if let Some(index) = entry
			.get("firstKeptEntryIndex")
			.and_then(Value::as_u64)
			.map(|index| index as usize)
			&& !session_lines.contains(&index)
			&& let Some(Some(target)) = ids.get(index)
		{
			entry["firstKeptEntryId"] = Value::String(target.clone());
		}
		if let Some(object) = entry.as_object_mut() {
			object.remove("firstKeptEntryIndex");
		}
	}
}

/// v2 → v3: rename the `hookMessage` message role to `custom`.
fn migrate_v2_to_v3(entries: &mut [Value]) {
	for entry in entries.iter_mut() {
		match entry.get("type").and_then(Value::as_str) {
			Some("session") => entry["version"] = json!(3),
			Some("message")
				if entry
					.get("message")
					.and_then(|message| message.get("role"))
					.and_then(Value::as_str)
					== Some("hookMessage") =>
			{
				entry["message"]["role"] = Value::String("custom".into());
			}
			_ => {}
		}
	}
}

/// Time-plus-random entry id — upstream `generateId`: 8 hex chars from a
/// random UUID, collision-checked against the live id set.
fn generate_entry_id(taken: &impl Fn(&str) -> bool) -> String {
	for _ in 0..100 {
		let candidate = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
		if !taken(&candidate) {
			return candidate;
		}
	}
	uuid::Uuid::new_v4().simple().to_string()
}

/// ISO 8601 timestamp (`new Date().toISOString()` shape, UTC, ms precision)
/// without pulling in a time crate.
pub fn iso_timestamp(ms: u64) -> String {
	let secs = ms / 1000;
	let millis = ms % 1000;
	let days = secs / 86400;
	let rem = secs % 86400;
	let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
	let (y, mth, d) = civil_from_days(days as i64);
	format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

/// `YYYYMMDD-HHMMSS` for session directory names.
fn chrono_like_timestamp() -> String {
	let ms = pi_ai::types::now_ms();
	let secs = ms / 1000;
	let days = secs / 86400;
	let rem = secs % 86400;
	let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
	let (y, mth, d) = civil_from_days(days as i64);
	format!("{y:04}{mth:02}{d:02}-{h:02}{m:02}{s:02}")
}

/// Days since epoch → civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
	let y = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let mth = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = if mth <= 2 { y + 1 } else { y };
	(y, mth as u32, d as u32)
}

// ---------------------------------------------------------------------------
// Projection (`buildContextEntries` + `buildSessionProjection`)
// ---------------------------------------------------------------------------

/// Compaction-aware, edit-aware model context rebuilt from the entry tree
/// (`buildSessionProjection` upstream): the leaf-to-root path with the newest
/// compaction applied and the latest `context_edit` per target honored.
#[derive(Debug, Clone, Default)]
pub struct SessionProjection {
	/// Model-visible messages with their owning entry id.
	pub messages: Vec<(Option<String>, Message)>,
	/// Earliest transcript index with trustworthy provider usage (set past a
	/// replayed compaction summary; see [`super::context_usage`]).
	pub usage_valid_from: usize,
	/// Latest `thinking_level_change` on the leaf path.
	pub thinking_level: Option<String>,
	/// Latest `model_change` on the leaf path: `(provider, modelId)`.
	pub model: Option<(String, String)>,
}

/// Root-to-leaf path from `leaf_id`, mirroring `buildSessionPath`: `None`
/// means "before any entry" (upstream `leafId === null` → empty context);
/// walk parent links to the root, stopping at a broken chain (orphans become
/// their own root).
pub fn build_session_path(entries: &[SessionEntry], leaf_id: Option<&str>) -> Vec<SessionEntry> {
	let by_id: HashMap<&str, &SessionEntry> = entries
		.iter()
		.filter_map(|entry| entry.id().map(|id| (id, entry)))
		.collect();
	let mut leaf = leaf_id.and_then(|id| by_id.get(id).copied());
	let mut path = Vec::new();
	let mut visited: HashSet<String> = HashSet::new();
	while let Some(entry) = leaf {
		if let Some(id) = entry.id()
			&& !visited.insert(id.to_string())
		{
			break; // cycle guard for corrupt files
		}
		path.push(entry.clone());
		leaf = entry
			.parent_id()
			.and_then(|parent| by_id.get(parent).copied());
	}
	path.reverse();
	path
}

/// Active, compaction-aware entry list plus the boundary of the retained
/// pre-compaction range (`buildContextEntries` upstream, plus the kept/after
/// split the port needs for usage-trust accounting).
#[derive(Debug, Clone)]
pub struct ContextSelection {
	pub entries: Vec<SessionEntry>,
	/// Entries retained from before the newest compaction (they directly
	/// follow the compaction entry itself). 0 without a compaction or for a
	/// keep-nothing cut.
	pub kept_count: usize,
}

/// Active, compaction-aware entry list (`buildContextEntries` upstream):
/// the newest compaction on the path contributes itself plus its kept range
/// (`firstKeptEntryId` up to the compaction, minus system messages) and
/// everything after it; older entries are omitted.
pub fn build_context_entries(path: Vec<SessionEntry>) -> Vec<SessionEntry> {
	build_context_selection(path).entries
}

pub fn build_context_selection(path: Vec<SessionEntry>) -> ContextSelection {
	let Some(compaction_index) = path
		.iter()
		.rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
	else {
		return ContextSelection {
			entries: path,
			kept_count: 0,
		};
	};
	let first_kept = match &path[compaction_index].kind {
		EntryKind::Compaction {
			first_kept_entry_id,
			..
		} => first_kept_entry_id.clone(),
		_ => unreachable!("rposition matched a compaction"),
	};
	let mut context: Vec<SessionEntry> = Vec::new();
	context.push(path[compaction_index].clone());
	let mut found_first_kept = false;
	let mut kept_count = 0usize;
	for entry in &path[..compaction_index] {
		if entry.id() == Some(first_kept.as_str()) {
			found_first_kept = true;
		}
		if found_first_kept
			&& !matches!(
				&entry.kind,
				EntryKind::Message { message } if matches!(**message, Message::System(_))
			) {
			context.push(entry.clone());
			kept_count += 1;
		}
	}
	context.extend(path[compaction_index + 1..].iter().cloned());
	ContextSelection {
		entries: context,
		kept_count,
	}
}

/// Project entries into the model context (upstream `buildSessionProjection`),
/// following the last entry as the leaf.
pub fn project_entries(entries: &[SessionEntry]) -> SessionProjection {
	let leaf_id = entries
		.last()
		.and_then(|entry| entry.id())
		.map(str::to_owned);
	project_entries_with_leaf(entries, leaf_id.as_deref())
}

/// [`project_entries`] with an explicit leaf (tree navigation: `branch()`).
pub fn project_entries_with_leaf(
	entries: &[SessionEntry],
	leaf_id: Option<&str>,
) -> SessionProjection {
	let path = build_session_path(entries, leaf_id);
	let selection = build_context_selection(path);
	let context_entries = &selection.entries;

	// Settings from the full path (`getSessionContextSettings`).
	let mut projection = SessionProjection::default();
	for entry in context_entries.iter() {
		match &entry.kind {
			EntryKind::ThinkingLevelChange { thinking_level } => {
				projection.thinking_level = Some(thinking_level.clone());
			}
			EntryKind::ModelChange { provider, model_id } => {
				projection.model = Some((provider.clone(), model_id.clone()));
			}
			_ => {}
		}
	}

	// Latest context edit per target within the selected range.
	let mut edits: HashMap<String, Option<ContextReplacement>> = HashMap::new();
	for entry in context_entries.iter() {
		if let EntryKind::ContextEdit {
			target_id,
			replacement,
		} = &entry.kind
		{
			edits.insert(target_id.clone(), replacement.clone());
		}
	}

	let starts_with_compaction = matches!(
		context_entries.first().map(|entry| &entry.kind),
		Some(EntryKind::Compaction { .. })
	);
	for (index, entry) in context_entries.iter().enumerate() {
		// Only the newest compaction (projected first) contributes a summary;
		// older ones inside a retained range contribute nothing (upstream
		// `buildSessionProjection` index > 0 rule).
		if matches!(entry.kind, EntryKind::Compaction { .. }) && index > 0 {
			continue;
		}
		for mut message in entry_to_context_messages(entry) {
			if let Some(edit) = entry.id().and_then(|id| edits.get(id)) {
				message = match edit {
					None => continue, // omitted from context
					Some(replacement) => apply_replacement(&message, &replacement.content),
				};
			}
			projection
				.messages
				.push((entry.id().map(str::to_owned), message));
		}
		// Provider usage captured before the compaction — including the kept
		// tail, which was measured against a smaller context — is distrusted
		// (upstream `estimateProjectedContextTokens` invalidates usage behind
		// a compaction). Valid usage resumes with the first post-compaction
		// message.
		if starts_with_compaction && index == selection.kept_count {
			projection.usage_valid_from = projection.messages.len();
		}
	}
	// An omission shrinks the transcript; a floor past the end is meaningless.
	projection.usage_valid_from = projection.usage_valid_from.min(projection.messages.len());
	projection
}

/// Project one selected entry into model-visible messages
/// (`sessionEntryToContextMessages` upstream, reduced to the roles our
/// `Message` union can represent).
fn entry_to_context_messages(entry: &SessionEntry) -> Vec<Message> {
	match &entry.kind {
		EntryKind::Message { message } => vec![(**message).clone()],
		// Upstream `convertToLlm` (`core/messages.ts:152`): bash executions
		// replay as user text unless the `!!` exclusion flag is set.
		EntryKind::BashExecution {
			command,
			output,
			exit_code,
			cancelled,
			truncated,
			full_output_path,
			exclude_from_context,
		} => match exclude_from_context {
			true => Vec::new(),
			false => vec![Message::user(bash_execution_to_text(
				command,
				output,
				*exit_code,
				*cancelled,
				*truncated,
				full_output_path.as_deref(),
			))],
		},
		EntryKind::Compaction {
			summary,
			system_message,
			..
		} => {
			let mut messages = Vec::new();
			if let Some(system) = system_message {
				messages.push(Message::System(system.clone()));
			}
			messages.push(super::compaction::compaction_summary_message(summary));
			messages
		}
		EntryKind::BranchSummary { summary, .. } => {
			vec![Message::user(format!(
				"{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
			))]
		}
		EntryKind::CustomMessage { content, .. } => vec![Message::user(value_as_text(content))],
		_ => Vec::new(),
	}
}

/// Flatten a `custom_message` content value (string or text-block array) to
/// the text our user-message form carries.
fn value_as_text(value: &Value) -> String {
	match value {
		Value::String(text) => text.clone(),
		Value::Array(blocks) => blocks
			.iter()
			.filter_map(|block| block.get("text").and_then(Value::as_str))
			.collect::<Vec<_>>()
			.join(" "),
		_ => String::new(),
	}
}

/// Apply a non-null context-edit replacement: swap only the message content,
/// keeping role and metadata (upstream `projectContextEntry`). Falls back to
/// the original message when the replacement cannot be represented.
fn apply_replacement(message: &Message, content: &Value) -> Message {
	let mut value = match serde_json::to_value(message) {
		Ok(value) => value,
		Err(_) => return message.clone(),
	};
	value["content"] = content.clone();
	serde_json::from_value::<Message>(value).unwrap_or_else(|_| message.clone())
}

// ---------------------------------------------------------------------------
// File loading
// ---------------------------------------------------------------------------

/// Parse all well-formed JSON lines (malformed/torn lines are skipped, as
/// upstream `parseSessionEntries` does).
pub fn parse_file_lines(text: &str) -> Vec<Value> {
	text.lines()
		.filter(|line| !line.trim().is_empty())
		.filter_map(|line| serde_json::from_str::<Value>(line).ok())
		.collect()
}

/// Load a session file, migrate it in memory to v3, and parse typed entries.
/// Returns `(header, entries)`; the header is `None` for files without one.
pub fn load_session_file(path: &Path) -> (Option<SessionHeader>, Vec<SessionEntry>) {
	let Ok(text) = std::fs::read_to_string(path) else {
		return (None, Vec::new());
	};
	let mut values = parse_file_lines(&text);
	let header = values
		.iter()
		.find(|value| value.get("type").and_then(Value::as_str) == Some("session"))
		.and_then(SessionHeader::from_value);
	migrate_to_current_version(&mut values);
	let entries = values
		.iter()
		.filter(|value| value.get("type").and_then(Value::as_str) != Some("session"))
		.filter_map(SessionEntry::from_value)
		.collect();
	(header, entries)
}

/// Read the first session header of a directory's log without a full parse
/// (upstream `readSessionHeader` for discovery).
pub fn peek_session_header(dir: &Path) -> Option<SessionHeader> {
	let text = std::fs::read_to_string(dir.join("session.jsonl")).ok()?;
	parse_file_lines(&text)
		.iter()
		.find(|value| value.get("type").and_then(Value::as_str) == Some("session"))
		.and_then(SessionHeader::from_value)
}

/// Load and project a session directory's log (resume path): migrate in
/// memory, follow the tree from the leaf, apply compaction and edits.
pub fn project_file(dir: &Path) -> SessionProjection {
	let (_, entries) = load_session_file(&dir.join("session.jsonl"));
	project_entries(&entries)
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct SessionManager {
	session_dir: PathBuf,
	file: Option<std::fs::File>,
	header: Option<SessionHeader>,
	/// All non-header entries in file order; mirrors the log.
	entries: Vec<SessionEntry>,
	by_id: HashMap<String, usize>,
	leaf_id: Option<String>,
	pending_sync: Option<String>,
	pending_metadata: bool,
	pending_message_id: Option<String>,
	#[cfg(test)]
	fail_next_sync: bool,
	write_broken: bool,
	pub entries_written: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
	#[error("io error: {0}")]
	Io(#[from] std::io::Error),
	#[error("serialization error: {0}")]
	Serialization(#[from] serde_json::Error),
	#[error("session already has a header")]
	AlreadyInitialized,
	#[error("no session directory (sessions disabled)")]
	Disabled,
	/// Pre-v3 Rust flat log (`type:"header"`): detected, reported, untouched.
	#[error("{0}")]
	LegacyFormat(String),
	/// A non-empty log without a valid upstream session header.
	#[error("{0}")]
	InvalidFormat(String),
	/// Tree navigation named an entry the log does not carry
	/// (upstream `branch`/`createBranchedSession` throw).
	#[error("entry {0} not found")]
	EntryNotFound(String),
}

impl SessionManager {
	/// Create a fresh timestamped session under the sessions root.
	pub fn create(cwd: &Path) -> Result<Self, SessionError> {
		Self::create_with_parent(cwd, None)
	}

	/// [`SessionManager::create`] with an optional `parentSession` lineage in
	/// the header (upstream `SessionManager.create(cwd, dir, {parentSession})`
	/// for `/new`/fork-before-first-entry). The directory name gains a short
	/// random segment so several sessions created within the same second
	/// (new → fork) never collide.
	pub fn create_with_parent(
		cwd: &Path,
		parent_session: Option<String>,
	) -> Result<Self, SessionError> {
		Self::create_with_parent_in(&config::sessions_dir(), cwd, parent_session)
	}

	/// Root-parameterized [`SessionManager::create_with_parent`] — replacement
	/// flows place the new session next to the current one (upstream passes
	/// `getSessionDir()`); tests inject a temporary root.
	pub fn create_with_parent_in(
		root: &Path,
		cwd: &Path,
		parent_session: Option<String>,
	) -> Result<Self, SessionError> {
		// Random v4 segment: a v7's leading hex is timestamp-derived, so two
		// sessions created in the same millisecond would collide on the name.
		let id = format!(
			"{}-{}",
			chrono_like_timestamp(),
			&uuid::Uuid::new_v4().simple().to_string()[..8]
		);
		let dir = root.join(id);
		std::fs::create_dir_all(&dir)?;
		let header = SessionHeader {
			version: Some(CURRENT_SESSION_VERSION),
			id: uuid::Uuid::now_v7().to_string(),
			timestamp: iso_timestamp(pi_ai::types::now_ms()),
			cwd: cwd.to_string_lossy().into_owned(),
			parent_session,
		};
		let mut manager = Self::empty(dir, Some(header.clone()));
		manager.open_file()?;
		manager.write_header(&header)?;
		Ok(manager)
	}

	/// In-memory session (persistence disabled, `--no-session`).
	pub fn in_memory() -> Self {
		Self::empty(PathBuf::new(), None)
	}

	/// Open an existing session directory for append.
	///
	/// Empty logs self-heal with a fresh v3 header (upstream initializes
	/// empty files the same way). Legacy pi-rs or headerless non-empty logs
	/// are rejected without modification.
	pub fn open(dir: &Path) -> Result<Self, SessionError> {
		let path = dir.join("session.jsonl");
		// A missing/unreadable file behaves like an empty one (self-heal
		// below); a missing DIRECTORY still fails in open_file.
		let text = std::fs::read_to_string(&path).unwrap_or_default();
		let mut values = parse_file_lines(&text);
		if values.is_empty() {
			// Empty (or fully torn) log: start a fresh v3 session in place.
			let header = SessionHeader {
				version: Some(CURRENT_SESSION_VERSION),
				id: uuid::Uuid::now_v7().to_string(),
				timestamp: iso_timestamp(pi_ai::types::now_ms()),
				cwd: std::env::current_dir()
					.ok()
					.map(|cwd| cwd.to_string_lossy().into_owned())
					.unwrap_or_default(),
				parent_session: None,
			};
			let mut manager = Self::empty(dir.to_path_buf(), Some(header));
			manager.open_file()?;
			let header = manager.header.clone().expect("just set");
			manager.write_header(&header)?;
			return Ok(manager);
		}
		let first_type = values[0].get("type").and_then(Value::as_str);
		if first_type == Some("header") {
			return Err(SessionError::LegacyFormat(
				"session log uses the legacy pi-rs format (type \"header\") which is not the upstream v3 session format; the file was left untouched".into(),
			));
		}
		if first_type != Some("session") || values[0].get("id").and_then(Value::as_str).is_none() {
			return Err(SessionError::InvalidFormat(format!(
				"{} is not a valid session log (no session header)",
				path.display()
			)));
		}
		let migrated = migrate_to_current_version(&mut values);
		// The header is parsed after migration so the in-memory copy carries
		// the current version (upstream reads `header.version` post-migrate).
		let header = SessionHeader::from_value(&values[0]).expect("checked above");
		let entries: Vec<SessionEntry> = values
			.iter()
			.filter(|value| value.get("type").and_then(Value::as_str) != Some("session"))
			.filter_map(SessionEntry::from_value)
			.collect();
		let mut manager = Self::empty(dir.to_path_buf(), Some(header));
		manager.open_file()?;
		if migrated {
			// Upstream rewrites migrated files in place (`_rewriteFile`).
			manager.rewrite_file(&values)?;
		}
		for entry in entries {
			manager.index_entry(entry);
		}
		if !migrated {
			manager.entries_written = values.len() as u64;
		}
		Ok(manager)
	}

	/// Wrap an arbitrary file handle (tests inject failing writers).
	#[cfg(test)]
	pub(crate) fn with_file(dir: PathBuf, file: std::fs::File) -> Self {
		let mut manager = Self::empty(dir, None);
		manager.file = Some(file);
		manager
	}

	/// Open a session by path (upstream `SessionManager.open(sessionPath)`,
	/// used by `switch_session`). Layout deviation (accepted, pre-existing):
	/// pi-rs stores one `session.jsonl` per session *directory*, so `path`
	/// may name the directory or any file inside it — the directory's log is
	/// opened either way.
	pub fn open_session_path(path: &Path) -> Result<Self, SessionError> {
		let dir = if path.is_dir() {
			path
		} else {
			path.parent().unwrap_or(path)
		};
		Self::open(dir)
	}

	fn empty(session_dir: PathBuf, header: Option<SessionHeader>) -> Self {
		Self {
			session_dir,
			file: None,
			header,
			entries: Vec::new(),
			by_id: HashMap::new(),
			leaf_id: None,
			pending_sync: None,
			pending_metadata: false,
			pending_message_id: None,
			#[cfg(test)]
			fail_next_sync: false,
			write_broken: false,
			entries_written: 0,
		}
	}

	fn write_header(&mut self, header: &SessionHeader) -> Result<(), SessionError> {
		let mut line = serde_json::to_string(&header.to_value())?;
		line.push('\n');
		if let Some(file) = &mut self.file {
			file.write_all(line.as_bytes())?;
			file.sync_data()?;
		}
		self.entries_written += 1;
		Ok(())
	}

	/// Rewrite the whole log (migration path): truncate, write every line,
	/// fsync. Only called from `open` before anything else holds state.
	fn rewrite_file(&mut self, values: &[Value]) -> Result<(), SessionError> {
		let Some(file) = &mut self.file else {
			return Ok(()); // in-memory
		};
		file.set_len(0)?;
		file.seek(std::io::SeekFrom::Start(0))?;
		for value in values {
			let mut line = serde_json::to_string(value)?;
			line.push('\n');
			file.write_all(line.as_bytes())?;
		}
		file.sync_data()?;
		self.entries_written = values.len() as u64;
		Ok(())
	}

	fn open_file(&mut self) -> Result<(), SessionError> {
		if self.session_dir.as_os_str().is_empty() {
			return Ok(()); // in-memory
		}
		let path = self.session_dir.join("session.jsonl");
		let mut file = std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.read(true)
			.open(path)?;
		// Repair a torn final line (crash mid-write) so the next entry starts
		// on a fresh line instead of gluing onto it.
		let len = file.metadata()?.len();
		if len > 0 {
			let mut last = [0u8; 1];
			file.seek(std::io::SeekFrom::End(-1))?;
			file.read_exact(&mut last)?;
			if last[0] != b'\n' {
				file.write_all(b"\n")?;
			}
		}
		self.file = Some(file);
		Ok(())
	}

	pub fn session_dir(&self) -> Option<&Path> {
		if self.session_dir.as_os_str().is_empty() {
			None
		} else {
			Some(&self.session_dir)
		}
	}

	/// The session id used by this port's resume flow: the directory name
	/// (upstream keys sessions by header id; see the module docs for the
	/// accepted layout deviation).
	pub fn session_id(&self) -> Option<String> {
		self.session_dir()
			.and_then(|p| p.file_name())
			.map(|n| n.to_string_lossy().into_owned())
	}

	/// Header id (UUIDv7 for sessions created by this port).
	pub fn header_id(&self) -> Option<&str> {
		self.header.as_ref().map(|header| header.id.as_str())
	}

	pub fn header(&self) -> Option<&SessionHeader> {
		self.header.as_ref()
	}

	/// All entries in file order (excludes the header).
	pub fn entries(&self) -> &[SessionEntry] {
		&self.entries
	}

	pub fn leaf_id(&self) -> Option<&str> {
		self.leaf_id.as_deref()
	}

	/// `getEntry(id)` upstream: one entry by id.
	pub fn get_entry(&self, id: &str) -> Option<SessionEntry> {
		self.by_id.get(id).map(|index| self.entries[*index].clone())
	}

	/// `getChildren(parentId)` upstream: direct children in file order.
	pub fn get_children(&self, parent_id: &str) -> Vec<SessionEntry> {
		self.entries
			.iter()
			.filter(|entry| entry.parent_id() == Some(parent_id))
			.cloned()
			.collect()
	}

	/// `getTree()` upstream: the whole entry forest. Roots are entries whose
	/// `parentId` is null or self-referencing, plus orphans (broken parent
	/// chain); children are sorted by timestamp ascending. Builds bottom-up in
	/// reverse file order — every child is written after its parent, so all
	/// subtrees are assembled before their parents — which is iterative and
	/// cannot overflow the stack on deep trees.
	pub fn get_tree(&self) -> Vec<SessionTreeNode> {
		let mut pending_children: HashMap<String, Vec<SessionTreeNode>> = HashMap::new();
		let mut roots = Vec::new();
		for entry in self.entries.iter().rev() {
			let Some(id) = entry.id().map(str::to_owned) else {
				continue;
			};
			let mut children = pending_children.remove(&id).unwrap_or_default();
			children.sort_by(|a, b| a.entry.base.timestamp.cmp(&b.entry.base.timestamp));
			let node = SessionTreeNode {
				entry: entry.clone(),
				children,
			};
			match entry.parent_id() {
				None => roots.push(node),
				Some(parent) if parent == id => roots.push(node), // self-referencing root
				Some(parent) if self.by_id.contains_key(parent) => {
					match pending_children.get_mut(parent) {
						Some(siblings) => siblings.push(node),
						None => {
							pending_children.insert(parent.to_string(), vec![node]);
						}
					}
				}
				// Orphans (broken parent chain) become roots, like upstream's
				// failed nodeMap lookup.
				Some(_) => roots.push(node),
			}
		}
		// Reverse file order collected the roots latest-first; restore the
		// file order upstream iterates in.
		roots.reverse();
		roots
	}

	/// `branch(branchFromId)` upstream: move the leaf pointer to an existing
	/// entry. The next append becomes a child of it, forming a new branch.
	pub fn branch(&mut self, branch_from_id: &str) -> Result<(), SessionError> {
		if !self.by_id.contains_key(branch_from_id) {
			return Err(SessionError::EntryNotFound(branch_from_id.to_string()));
		}
		self.leaf_id = Some(branch_from_id.to_string());
		Ok(())
	}

	/// `resetLeaf()` upstream: move the leaf pointer before any entry, so the
	/// next append starts a new root.
	pub fn reset_leaf(&mut self) {
		self.leaf_id = None;
	}

	/// `appendSessionInfo(name)` upstream: sanitize `[\r\n]+ → space`, trim,
	/// and append a `session_info` entry.
	pub fn append_session_info(&mut self, name: &str) -> Result<Option<String>, SessionError> {
		let sanitized = sanitize_session_name(name);
		let entry = SessionEntry::new(EntryKind::SessionInfo {
			name: Some(sanitized),
		});
		self.append(entry)
	}

	/// `getSessionName()` upstream: the latest `session_info` name in reverse
	/// file order; an empty name (after trim) clears the title.
	pub fn session_name(&self) -> Option<String> {
		self.entries
			.iter()
			.rev()
			.find_map(|entry| match &entry.kind {
				EntryKind::SessionInfo { name } => {
					Some(name.as_deref().unwrap_or_default().trim().to_string())
				}
				_ => None,
			})
			.filter(|name| !name.is_empty())
	}

	/// Compaction-aware, edit-aware model context of the current log.
	pub fn projection(&self) -> SessionProjection {
		project_entries(&self.entries)
	}

	fn index_entry(&mut self, entry: SessionEntry) {
		if let Some(id) = entry.id().map(str::to_owned) {
			self.by_id.insert(id.clone(), self.entries.len());
			self.leaf_id = Some(id);
		}
		self.entries.push(entry);
	}

	/// Append one entry as a child of the current leaf; returns the assigned
	/// entry id. No-op write for in-memory sessions (state still advances).
	pub fn append(&mut self, mut entry: SessionEntry) -> Result<Option<String>, SessionError> {
		if entry.base.id.is_none() {
			let taken = |id: &str| self.by_id.contains_key(id);
			entry.base.id = Some(generate_entry_id(&taken));
		} else if self
			.by_id
			.contains_key(entry.base.id.as_deref().unwrap_or_default())
		{
			let taken = |id: &str| self.by_id.contains_key(id);
			entry.base.id = Some(generate_entry_id(&taken));
		}
		if entry.base.timestamp.is_none() {
			entry.base.timestamp = Some(iso_timestamp(pi_ai::types::now_ms()));
		}
		entry.base.parent_id = self.leaf_id.clone();
		let id = entry.base.id.clone();
		let is_message = matches!(entry.kind, EntryKind::Message { .. });
		let value = entry.to_value();
		let mut line = serde_json::to_string(&value)?;
		line.push('\n');
		self.append_line(&line, is_message)?;
		self.index_entry(entry);
		Ok(id)
	}

	/// Durability pipeline shared by every append: write, fsync, and the
	/// pending-sync rollback bookkeeping (unchanged from the flat-log port).
	fn append_line(&mut self, line: &str, is_message: bool) -> Result<(), SessionError> {
		#[cfg(test)]
		if std::mem::take(&mut self.fail_next_sync) {
			return self.append_line_with_io(
				line,
				is_message,
				|file, bytes| file.write_all(bytes),
				|_| Err(std::io::Error::other("injected sync failure")),
			);
		}
		self.append_line_with_io(
			line,
			is_message,
			|file, bytes| file.write_all(bytes),
			|file| file.sync_data(),
		)
	}

	fn append_line_with_io(
		&mut self,
		line: &str,
		is_message: bool,
		write: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
		sync: impl FnOnce(&std::fs::File) -> std::io::Result<()>,
	) -> Result<(), SessionError> {
		let Some(file) = &mut self.file else {
			return Ok(());
		};
		if self.write_broken {
			return Err(std::io::Error::other(
				"session log requires reopening after failed rollback",
			)
			.into());
		}
		if let Some(pending) = &self.pending_sync {
			if pending != line {
				return Err(
					std::io::Error::other("previous session entry still awaiting sync").into(),
				);
			}
		} else {
			let original_len = file.metadata()?.len();
			if let Err(error) = write(file, line.as_bytes()) {
				if file.set_len(original_len).is_err() {
					self.write_broken = true;
				}
				return Err(error.into());
			}
			self.pending_sync = Some(line.to_string());
			self.pending_metadata = !is_message;
		}
		sync(file)?;
		self.pending_sync = None;
		self.pending_metadata = false;
		self.entries_written += 1;
		Ok(())
	}

	pub fn append_message(&mut self, message: &Message) -> Result<Option<String>, SessionError> {
		// Retry-stable id: a failed append keeps its id so the retry writes
		// the identical line (pending-sync dedupe).
		let id = self
			.pending_message_id
			.get_or_insert_with(|| generate_entry_id(&|_| false))
			.clone();
		let mut entry = SessionEntry::new(EntryKind::Message {
			message: Box::new(message.clone()),
		});
		entry.base.id = Some(id.clone());
		let result = self.append(entry);
		if result.is_ok() {
			self.pending_message_id = None;
		}
		result
	}

	#[cfg(test)]
	pub(crate) fn fail_next_sync(&mut self) {
		self.fail_next_sync = true;
	}

	/// Finish a previously written metadata entry before appending messages.
	pub fn sync_pending_metadata(&mut self) -> Result<(), SessionError> {
		if self.pending_metadata {
			if let Some(file) = &self.file {
				file.sync_data()?;
			}
			self.pending_sync = None;
			self.pending_metadata = false;
			self.entries_written += 1;
		}
		Ok(())
	}

	/// List recent sessions (newest first) with their entry counts.
	pub fn list_recent(limit: usize) -> Vec<(PathBuf, u64)> {
		list_recent_in(&config::sessions_dir(), limit)
	}

	/// `createBranchedSession(leafId)` upstream (`session-manager.ts:1632`):
	/// write a new session file containing only the root-to-`leaf_id` path
	/// with `parentId` links re-chained along it, and return the new session
	/// path.
	///
	/// Layout deviation (see the module docs): the branched session becomes a
	/// new sibling *directory* next to the current one (upstream writes a
	/// `<timestamp>_<id>.jsonl` file into the same `getSessionDir()`); the
	/// header's `parentSession` points at the source `session.jsonl`,
	/// matching upstream's field semantics. The upstream label re-chaining
	/// (`replacementByLabelId`) is skipped: labels are kept as ordinary path
	/// entries, so no id on the path changes and `firstKeptEntryId` on
	/// compaction entries passes through unmodified (it can only reference
	/// entries on the retained path).
	pub fn create_branched_session(&self, leaf_id: &str) -> Result<PathBuf, SessionError> {
		// Sibling of the current session (upstream writes into
		// `getSessionDir()`); the sessions root when unpersisted.
		let root = self
			.session_dir()
			.and_then(Path::parent)
			.map(Path::to_path_buf)
			.unwrap_or_else(config::sessions_dir);
		self.create_branched_session_in(&root, leaf_id)
	}

	/// Root-parameterized [`SessionManager::create_branched_session`] (tests
	/// inject a temporary sessions root).
	pub fn create_branched_session_in(
		&self,
		root: &Path,
		leaf_id: &str,
	) -> Result<PathBuf, SessionError> {
		let path = build_session_path(&self.entries, Some(leaf_id));
		if path.is_empty() {
			return Err(SessionError::EntryNotFound(leaf_id.to_string()));
		}
		let cwd = self
			.header
			.as_ref()
			.map(|header| header.cwd.clone())
			.unwrap_or_default();
		let parent_file = self.session_dir().map(|dir| dir.join("session.jsonl"));

		let new_session_id = uuid::Uuid::now_v7().to_string();
		// Random v4 segment — see create_with_parent (v7's leading hex is
		// timestamp-derived and would collide within the same millisecond).
		let dir_name = format!(
			"{}-{}",
			chrono_like_timestamp(),
			&uuid::Uuid::new_v4().simple().to_string()[..8]
		);
		let dir = root.join(dir_name);
		std::fs::create_dir_all(&dir)?;

		let mut manager = {
			let header = SessionHeader {
				version: Some(CURRENT_SESSION_VERSION),
				id: new_session_id,
				timestamp: iso_timestamp(pi_ai::types::now_ms()),
				cwd,
				parent_session: parent_file.map(|path| path.to_string_lossy().into_owned()),
			};
			let mut manager = Self::empty(dir, Some(header));
			manager.open_file()?;
			let header = manager.header.clone().expect("just set");
			manager.write_header(&header)?;
			manager
		};
		// Re-chain the retained path: the first entry roots at null, every
		// later entry follows its path predecessor.
		let mut path_parent: Option<String> = None;
		for mut entry in path {
			entry.base.parent_id = path_parent.clone();
			let id = entry.base.id.clone();
			manager.append(entry)?;
			path_parent = id;
		}
		Ok(manager
			.session_dir()
			.expect("branched session is persisted")
			.to_path_buf())
	}
}

/// Which session a resume flag names — mirrors the `--continue`/`--resume`
/// selection in `pi/packages/coding-agent/src/main.ts` (`parsed.continue` /
/// `parsed.resume` + `resolveSessionPath`), reduced to this port's flat
/// sessions root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelector {
	/// `--continue`: the most recent session **for the current working
	/// directory** (upstream `findMostRecentSession(dir, cwd)`; the header's
	/// `cwd` must match). Sessions whose log is missing/unreadable/legacy are
	/// skipped.
	Recent,
	/// `--resume <id|path>`: a session id (directory name under the sessions
	/// root, exact or unique prefix) or a path to the session directory.
	ByIdOrPath(String),
}

/// Locate the session directory a resume selector names.
pub fn resolve_session(selector: &SessionSelector) -> Result<PathBuf, String> {
	let cwd = std::env::current_dir().map_err(|error| format!("cannot determine cwd: {error}"))?;
	resolve_session_in(&config::sessions_dir(), &cwd, selector)
}

/// Root- and cwd-parameterized [`resolve_session`] (tests inject temporaries).
pub fn resolve_session_in(
	root: &Path,
	cwd: &Path,
	selector: &SessionSelector,
) -> Result<PathBuf, String> {
	match selector {
		SessionSelector::Recent => list_recent_in(root, usize::MAX)
			.into_iter()
			.find(|(dir, _)| {
				peek_session_header(dir).is_some_and(|header| {
					!header.cwd.is_empty() && paths_equivalent(Path::new(&header.cwd), cwd)
				})
			})
			.map(|(dir, _)| dir)
			.ok_or_else(|| format!("no previous session to continue in {}", cwd.display())),
		SessionSelector::ByIdOrPath(arg) => {
			let candidate = PathBuf::from(arg);
			if candidate.is_dir() {
				return Ok(candidate);
			}
			// Exact id (directory name), then a unique prefix match.
			let exact = root.join(arg);
			if exact.is_dir() {
				return Ok(exact);
			}
			let matches = prefix_matches(root, arg);
			match matches.as_slice() {
				[only] => Ok(only.clone()),
				[] => Err(format!("no session found matching `{arg}`")),
				many => Err(format!(
					"session id `{arg}` is ambiguous, {} matches: {}",
					many.len(),
					many.iter()
						.filter_map(|p| p.file_name())
						.map(|n| n.to_string_lossy().into_owned())
						.collect::<Vec<_>>()
						.join(", ")
				)),
			}
		}
	}
}

/// Header-cwd matching: string equality first, then canonicalized comparison
/// for paths that differ by symlinks (macOS `/tmp` vs `/private/tmp`).
fn paths_equivalent(a: &Path, b: &Path) -> bool {
	if a == b {
		return true;
	}
	match (a.canonicalize(), b.canonicalize()) {
		(Ok(a), Ok(b)) => a == b,
		_ => false,
	}
}

/// Validate a session directory before reopening it, so a missing, corrupt,
/// legacy, or headerless session fails loudly instead of silently starting an
/// empty log.
pub fn validate_session_dir(dir: &Path) -> Result<(), String> {
	let path = dir.join("session.jsonl");
	let text = std::fs::read_to_string(&path)
		.map_err(|e| format!("cannot read {}: {e}", path.display()))?;
	if let Some(line) = parse_file_lines(&text).first() {
		match line.get("type").and_then(Value::as_str) {
			Some("session") if line.get("id").and_then(Value::as_str).is_some() => return Ok(()),
			Some("header") => {
				return Err(format!(
					"{} uses the legacy pi-rs session format (pre-v3), which is not supported; the file was left untouched",
					dir.display()
				));
			}
			_ => {} // torn header line: nothing recoverable before it
		}
	}
	Err(format!(
		"{} is not a valid session directory (no session header)",
		dir.display()
	))
}

/// `[\r\n]+ → space`, then trim — the `appendSessionInfo` sanitization
/// (`session-manager.ts:1304`).
fn sanitize_session_name(name: &str) -> String {
	let mut sanitized = String::with_capacity(name.len());
	let mut previous_newline = false;
	for character in name.chars() {
		if character == '\r' || character == '\n' {
			if !previous_newline {
				sanitized.push(' ');
			}
			previous_newline = true;
		} else {
			previous_newline = false;
			sanitized.push(character);
		}
	}
	sanitized.trim().to_string()
}

fn list_recent_in(root: &Path, limit: usize) -> Vec<(PathBuf, u64)> {
	let Ok(entries) = std::fs::read_dir(root) else {
		return Vec::new();
	};
	let mut sessions: Vec<(PathBuf, u64)> = entries
		.flatten()
		.filter(|e| e.path().is_dir())
		.map(|e| {
			let entries = count_entries(&e.path()).unwrap_or(0);
			(e.path(), entries)
		})
		.collect();
	sessions.sort_by(|a, b| b.0.cmp(&a.0));
	sessions.truncate(limit);
	sessions
}

fn prefix_matches(root: &Path, prefix: &str) -> Vec<PathBuf> {
	let Ok(entries) = std::fs::read_dir(root) else {
		return Vec::new();
	};
	let mut matches: Vec<PathBuf> = entries
		.flatten()
		.map(|e| e.path())
		.filter(|p| {
			p.is_dir()
				&& p.file_name()
					.is_some_and(|n| n.to_string_lossy().starts_with(prefix))
		})
		.collect();
	matches.sort();
	matches
}

fn count_entries(dir: &Path) -> Result<u64, SessionError> {
	let path = dir.join("session.jsonl");
	let Ok(text) = std::fs::read_to_string(path) else {
		return Ok(0);
	};
	Ok(text
		.lines()
		.filter(|l| !l.trim().is_empty())
		// A partial line from a crash mid-write is not a recovered entry.
		.filter(|l| serde_json::from_str::<Value>(l).is_ok())
		.count() as u64)
}

#[cfg(test)]
mod tests {
	use super::*;
	use pi_ai::types::UserMessage;

	fn temp_session_dir(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!(
			"pi-rs-session-{}-{}-{}",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&dir).unwrap();
		dir
	}

	fn user_message(text: &str) -> Message {
		Message::User(UserMessage::text(text))
	}

	fn create_in(dir: &Path, cwd: &Path) -> SessionManager {
		let header = SessionHeader {
			version: Some(CURRENT_SESSION_VERSION),
			id: uuid::Uuid::now_v7().to_string(),
			timestamp: iso_timestamp(pi_ai::types::now_ms()),
			cwd: cwd.to_string_lossy().into_owned(),
			parent_session: None,
		};
		let mut manager = SessionManager::empty(dir.to_path_buf(), Some(header));
		manager.open_file().unwrap();
		let header = manager.header.clone().unwrap();
		manager.write_header(&header).unwrap();
		manager
	}

	fn open_bare(dir: &Path) -> SessionManager {
		// Tests that only need an appendable log: self-healing open.
		SessionManager::open(dir).unwrap()
	}

	fn read_lines(dir: &Path) -> Vec<Value> {
		parse_file_lines(&std::fs::read_to_string(dir.join("session.jsonl")).unwrap())
	}

	// -- timestamps ----------------------------------------------------------

	#[test]
	fn timestamp_shapes() {
		let iso = iso_timestamp(1_733_234_400_123);
		assert_eq!(iso, "2024-12-03T14:00:00.123Z");
		let ts = chrono_like_timestamp();
		assert_eq!(ts.len(), 15);
		assert!(ts.contains('-'));
	}

	// -- wire format ---------------------------------------------------------

	#[test]
	fn header_serializes_upstream_v3_shape() {
		let header = SessionHeader {
			version: Some(3),
			id: "0190a1b2-c3d4-7e5f-8a9b-c0d1e2f3a4b5".into(),
			timestamp: "2024-12-03T14:00:00.000Z".into(),
			cwd: "/project".into(),
			parent_session: None,
		};
		let value = header.to_value();
		assert_eq!(value["type"], "session");
		assert_eq!(value["version"], 3);
		assert_eq!(value["id"], "0190a1b2-c3d4-7e5f-8a9b-c0d1e2f3a4b5");
		assert_eq!(value["cwd"], "/project");
		assert!(value.get("parentSession").is_none());
		let with_parent = SessionHeader {
			parent_session: Some("/old/session.jsonl".into()),
			..header
		};
		assert_eq!(
			with_parent.to_value()["parentSession"],
			"/old/session.jsonl"
		);
	}

	#[test]
	fn message_entry_serializes_upstream_shape() {
		let mut entry = SessionEntry::new(EntryKind::Message {
			message: Box::new(user_message("hi")),
		});
		entry.base = EntryBase {
			id: Some("a0b1c2d3".into()),
			parent_id: None,
			timestamp: Some("2024-12-03T14:00:00.000Z".into()),
		};
		let value = entry.to_value();
		assert_eq!(value["type"], "message");
		assert_eq!(value["id"], "a0b1c2d3");
		assert_eq!(value["parentId"], Value::Null);
		assert_eq!(value["message"]["role"], "user");
		assert_eq!(
			value["message"]["content"],
			serde_json::json!([{"type":"text","text":"hi"}])
		);
		// Round-trips through the typed parser.
		let parsed = SessionEntry::from_value(&value).unwrap();
		assert_eq!(parsed, entry);
	}

	#[test]
	fn model_change_uses_upstream_provider_model_id_fields() {
		let mut entry = SessionEntry::model_change("openai", "gpt-4o");
		entry.base.id = Some("d4e5f6g7".into());
		let value = entry.to_value();
		assert_eq!(value["type"], "model_change");
		assert_eq!(value["provider"], "openai");
		assert_eq!(value["modelId"], "gpt-4o");
		let parsed = SessionEntry::from_value(&value).unwrap();
		assert_eq!(parsed.kind, entry.kind);
	}

	#[test]
	fn compaction_entry_serializes_camel_case() {
		let entry = SessionEntry::compaction("the summary", 1234);
		let value = entry.to_value();
		assert_eq!(value["type"], "compaction");
		assert_eq!(value["summary"], "the summary");
		assert_eq!(value["tokensBefore"], 1234);
		// Keep-nothing form: the entry keeps itself (upstream `firstKeptEntryId ?? id`).
		assert_eq!(value["firstKeptEntryId"], value["id"]);
		assert!(!value["id"].as_str().unwrap().is_empty());
		// Optional upstream fields stay absent.
		assert!(value.get("details").is_none());
		assert!(value.get("fromHook").is_none());
		assert!(value.get("systemMessage").is_none());
		let parsed = SessionEntry::from_value(&value).unwrap();
		assert_eq!(parsed.kind, entry.kind);
	}

	#[test]
	fn compaction_entry_ids_are_unique() {
		let first = SessionEntry::compaction("a", 1);
		let second = SessionEntry::compaction("b", 2);
		assert_ne!(first.id(), second.id());
	}

	#[test]
	fn context_edit_replacement_is_null_or_content_object() {
		let mut omit = SessionEntry::context_edit("target");
		omit.base.id = Some("g6h7i8j9".into());
		let value = omit.to_value();
		assert_eq!(value["type"], "context_edit");
		assert_eq!(value["targetId"], "target");
		assert_eq!(value["replacement"], Value::Null);

		let replace = SessionEntry::new(EntryKind::ContextEdit {
			target_id: "target".into(),
			replacement: Some(ContextReplacement {
				content: Value::String("fixed".into()),
			}),
		});
		let value = replace.to_value();
		assert_eq!(value["replacement"]["content"], "fixed");
		assert_eq!(SessionEntry::from_value(&value).unwrap().kind, replace.kind);
	}

	#[test]
	fn unknown_entry_types_round_trip_verbatim() {
		let raw = json!({
			"type": "session_info",
			"id": "k1l2m3n4",
			"parentId": Value::Null,
			"timestamp": "2024-12-03T14:35:00.000Z",
			"name": "Refactor auth module"
		});
		let entry = SessionEntry::from_value(&raw).unwrap();
		assert!(matches!(entry.kind, EntryKind::SessionInfo { .. }));
		assert_eq!(entry.to_value(), raw);

		let futuristic = json!({
			"type": "hologram",
			"id": "x0y1z2a3",
			"parentId": "k1l2m3n4",
			"timestamp": "2024-12-03T14:36:00.000Z",
			"data": {"depth": 3}
		});
		let entry = SessionEntry::from_value(&futuristic).unwrap();
		assert!(matches!(entry.kind, EntryKind::Unknown(_)));
		assert_eq!(entry.to_value(), futuristic, "unknown entries pass through");
		assert_eq!(entry.kind.type_name(), "unknown");
	}

	#[test]
	fn custom_role_messages_are_preserved_but_contribute_no_context() {
		// Upstream v3 renamed hookMessage → custom; our Message union cannot
		// represent it, so the entry is kept verbatim and skipped on replay.
		let raw = json!({
			"type": "message",
			"id": "c0ffee00",
			"parentId": Value::Null,
			"timestamp": "2024-12-03T14:00:00.000Z",
			"message": {"role": "custom", "customType": "ext", "content": "injected", "display": true, "timestamp": 1}
		});
		let entry = SessionEntry::from_value(&raw).unwrap();
		assert!(matches!(entry.kind, EntryKind::Unknown(_)));
		let projection = project_entries(&[entry]);
		assert!(projection.messages.is_empty());
	}

	#[test]
	fn all_eleven_upstream_entry_types_parse() {
		let samples = [
			json!({"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"role":"user","content":"hi","timestamp":1}}),
			json!({"type":"model_change","id":"b","parentId":"a","timestamp":"t","provider":"openai","modelId":"gpt-4o"}),
			json!({"type":"thinking_level_change","id":"c","parentId":"b","timestamp":"t","thinkingLevel":"high"}),
			json!({"type":"usage","id":"d","parentId":"c","timestamp":"t","kind":"cache_warm","provider":"anthropic","model":"m","usage":{"input":0,"output":0,"cacheRead":5,"cacheWrite":0,"totalTokens":5}}),
			json!({"type":"compaction","id":"e","parentId":"d","timestamp":"t","summary":"s","firstKeptEntryId":"e","tokensBefore":9}),
			json!({"type":"branch_summary","id":"f","parentId":"e","timestamp":"t","fromId":"e","summary":"branch"}),
			json!({"type":"custom","id":"g","parentId":"f","timestamp":"t","customType":"ext","data":{"count":42}}),
			json!({"type":"custom_message","id":"h","parentId":"g","timestamp":"t","customType":"ext","content":"injected","display":true}),
			json!({"type":"context_edit","id":"i","parentId":"h","timestamp":"t","targetId":"a","replacement":null}),
			json!({"type":"label","id":"j","parentId":"i","timestamp":"t","targetId":"a","label":"checkpoint-1"}),
			json!({"type":"session_info","id":"k","parentId":"j","timestamp":"t","name":"name"}),
		];
		let expected = [
			"message",
			"model_change",
			"thinking_level_change",
			"usage",
			"compaction",
			"branch_summary",
			"custom",
			"custom_message",
			"context_edit",
			"label",
			"session_info",
		];
		for (sample, type_name) in samples.iter().zip(expected) {
			let entry = SessionEntry::from_value(sample).unwrap();
			assert_eq!(entry.kind.type_name(), type_name, "parsing {sample}");
			assert!(!matches!(entry.kind, EntryKind::Unknown(_)));
		}
	}

	// -- durability ----------------------------------------------------------

	#[test]
	fn open_missing_dir_fails() {
		let dir = std::env::temp_dir().join(format!("pi-rs-missing-{}", pi_ai::types::now_ms()));
		assert!(matches!(
			SessionManager::open(&dir),
			Err(SessionError::Io(_))
		));
	}

	#[test]
	fn resume_appends_after_reopen_preserves_order() {
		let dir = temp_session_dir("resume");
		{
			let mut session = open_bare(&dir);
			session.append_message(&user_message("one")).unwrap();
			session.append_message(&user_message("two")).unwrap();
		}
		let mut session = SessionManager::open(&dir).unwrap();
		assert_eq!(session.entries_written, 3); // self-healed header + two messages
		session.append_message(&user_message("three")).unwrap();
		assert_eq!(session.entries_written, 4);
		drop(session);

		let lines = read_lines(&dir);
		assert_eq!(lines[0]["type"], "session");
		assert_eq!(lines[0]["version"], 3);
		let texts: Vec<String> = lines[1..]
			.iter()
			.map(|v| {
				v["message"]["content"].as_array().unwrap()[0]["text"]
					.as_str()
					.unwrap()
					.to_string()
			})
			.collect();
		assert_eq!(texts, vec!["one", "two", "three"]);
		// Tree: each entry chains to its parent; leaf is the last one.
		let mut parent: Option<String> = None;
		for line in &lines[1..] {
			assert_eq!(
				line["parentId"],
				parent.clone().map(Value::String).unwrap_or(Value::Null)
			);
			parent = Some(line["id"].as_str().unwrap().to_string());
		}
		let reopened = SessionManager::open(&dir).unwrap();
		assert_eq!(reopened.leaf_id().unwrap(), parent.unwrap());
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn partial_trailing_line_is_not_counted() {
		let dir = temp_session_dir("partial");
		let path = dir.join("session.jsonl");
		let mut session = open_bare(&dir);
		session.append_message(&user_message("complete")).unwrap();
		drop(session);
		// Simulate a crash mid-write: append half of a JSON line.
		{
			let mut file = std::fs::OpenOptions::new()
				.append(true)
				.open(&path)
				.unwrap();
			use std::io::Write;
			file.write_all(b"{\"type\":\"mess").unwrap();
		}

		let reopened = SessionManager::open(&dir).unwrap();
		assert_eq!(reopened.entries_written, 2);
		// New appends must start on a fresh line, not glue onto the partial one.
		let mut session = reopened;
		session
			.append_message(&user_message("after crash"))
			.unwrap();
		drop(session);
		assert_eq!(
			crate::core::agent_session::read_session_messages(&dir).len(),
			2
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn write_failure_is_reported_and_does_not_advance() {
		let dir = temp_session_dir("iofail");
		let path = dir.join("readonly.jsonl");
		std::fs::write(&path, "").unwrap();
		// A read-only handle fails on the first write (EBADF), like ENOSPC would.
		let readonly = std::fs::File::open(&path).unwrap();
		let mut session = SessionManager::with_file(dir.clone(), readonly);
		assert!(matches!(
			session.append_message(&user_message("x")),
			Err(SessionError::Io(_))
		));
		assert_eq!(session.entries_written, 0);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn retry_after_sync_failure_does_not_append_twice() {
		let dir = temp_session_dir("sync-retry");
		let mut session = open_bare(&dir);
		let mut entry = SessionEntry::new(EntryKind::Message {
			message: Box::new(user_message("once")),
		});
		entry.base.id = Some("deadbeef".into());
		entry.base.parent_id = None;
		entry.base.timestamp = Some("2024-12-03T14:00:00.000Z".into());
		let line = serde_json::to_string(&entry.to_value()).unwrap() + "\n";
		assert!(
			session
				.append_line_with_io(
					&line,
					true,
					|f, bytes| f.write_all(bytes),
					|_| { Err(std::io::Error::other("injected sync failure")) }
				)
				.is_err()
		);
		// A different entry must not slip past the unsynced one.
		assert!(session.append_message(&user_message("different")).is_err());
		// Retrying the identical line succeeds exactly once.
		session
			.append_line_with_io(
				&line,
				true,
				|f, bytes| f.write_all(bytes),
				|f| f.sync_data(),
			)
			.unwrap();
		assert_eq!(session.entries_written, 2); // header + retried entry
		assert_eq!(
			crate::core::agent_session::read_session_messages(&dir).len(),
			1
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn partial_write_failure_rolls_back_before_retry() {
		let dir = temp_session_dir("partial-write");
		let mut session = open_bare(&dir);
		session.append_message(&user_message("before")).unwrap();
		let mut entry = SessionEntry::new(EntryKind::Message {
			message: Box::new(user_message("after")),
		});
		entry.base.id = Some("cafebabe".into());
		entry.base.parent_id = session.leaf_id.clone();
		entry.base.timestamp = Some("2024-12-03T14:00:00.000Z".into());
		let line = serde_json::to_string(&entry.to_value()).unwrap() + "\n";
		assert!(
			session
				.append_line_with_io(
					&line,
					true,
					|file, bytes| {
						file.write_all(&bytes[..bytes.len() / 2])?;
						Err(std::io::Error::other("injected partial write"))
					},
					|file| file.sync_data()
				)
				.is_err()
		);
		assert_eq!(session.entries_written, 2);
		session
			.append_line_with_io(
				&line,
				true,
				|file, bytes| file.write_all(bytes),
				|file| file.sync_data(),
			)
			.unwrap();
		let messages = crate::core::agent_session::read_session_messages(&dir);
		assert_eq!(messages.len(), 2);
		let text = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(text.lines().count(), 3);
		assert!(
			text.lines()
				.all(|line| serde_json::from_str::<Value>(line).is_ok())
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn failed_metadata_sync_can_be_finished_before_message() {
		let dir = temp_session_dir("metadata-sync");
		let mut session = open_bare(&dir);
		let change = SessionEntry::model_change("test", "model");
		let line = serde_json::to_string(&change.to_value()).unwrap() + "\n";
		assert!(
			session
				.append_line_with_io(
					&line,
					false,
					|file, bytes| file.write_all(bytes),
					|_| { Err(std::io::Error::other("injected sync failure")) }
				)
				.is_err()
		);
		session.sync_pending_metadata().unwrap();
		session.append_message(&user_message("after")).unwrap();
		let text = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(text.lines().count(), 3);
		assert_eq!(session.entries_written, 3);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn in_memory_append_never_fails_and_writes_nothing() {
		let mut session = SessionManager::in_memory();
		session.append_message(&user_message("ghost")).unwrap();
		assert_eq!(session.entries_written, 0);
		assert!(session.session_dir().is_none());
		// State still advances: entries chain and project.
		let id = session
			.append(SessionEntry::model_change("a", "b"))
			.unwrap();
		assert!(id.is_some());
		assert_eq!(session.entries().len(), 2);
	}

	// -- migrations ----------------------------------------------------------

	fn write_lines(dir: &Path, lines: &[Value]) {
		let text = lines
			.iter()
			.map(|value| serde_json::to_string(value).unwrap())
			.collect::<Vec<_>>()
			.join("\n")
			+ "\n";
		std::fs::write(dir.join("session.jsonl"), text).unwrap();
	}

	#[test]
	fn v1_file_migrates_to_v3_with_tree_ids() {
		let dir = temp_session_dir("v1");
		write_lines(
			&dir,
			&[
				// Extra v1-era header fields (provider/modelId/thinkingLevel, as
				// in upstream test/fixtures/large-session.jsonl) survive the
				// migration rewrite untouched.
				json!({"type":"session","id":"legacy-id","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/p","provider":"anthropic","modelId":"claude-sonnet-4-5","thinkingLevel":"off"}),
				json!({"type":"message","timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"hello","timestamp":1}}),
				json!({"type":"compaction","timestamp":"2024-12-03T14:10:00.000Z","summary":"s","firstKeptEntryIndex":1,"tokensBefore":5}),
				json!({"type":"message","timestamp":"2024-12-03T14:11:00.000Z","message":{"role":"hookMessage","customType":"ext","content":"note","display":true,"timestamp":3}}),
			],
		);
		let mut manager = SessionManager::open(&dir).unwrap();
		assert_eq!(manager.header().unwrap().effective_version(), 3);
		assert_eq!(manager.entries().len(), 3);

		// The file was rewritten migrated (upstream `_rewriteFile`).
		let lines = read_lines(&dir);
		assert_eq!(lines[0]["version"], 3);
		assert_eq!(
			lines[0]["provider"], "anthropic",
			"unknown header fields survive"
		);
		assert!(
			lines[1]["id"].as_str().unwrap().len() == 8,
			"v1 entries gain 8-hex ids"
		);
		// Tree linkage: each entry's parentId is the previous id (header is
		// not a tree node, so the first entry's parent is null).
		assert_eq!(lines[1]["parentId"], Value::Null);
		assert_eq!(lines[2]["parentId"], lines[1]["id"]);
		assert_eq!(lines[3]["parentId"], lines[2]["id"]);
		// firstKeptEntryIndex (header-inclusive, entry at index 1) → id.
		assert_eq!(lines[2]["firstKeptEntryId"], lines[1]["id"]);
		assert!(lines[2].get("firstKeptEntryIndex").is_none());
		// hookMessage → custom (kept verbatim as Unknown by our union).
		assert_eq!(lines[3]["message"]["role"], "custom");
		assert!(matches!(manager.entries()[2].kind, EntryKind::Unknown(_)));

		// Appending after migration chains onto the migrated leaf.
		manager
			.append_message(&user_message("after migration"))
			.unwrap();
		let lines = read_lines(&dir);
		assert_eq!(lines[4]["parentId"], lines[3]["id"]);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn v2_file_migrates_roles_but_keeps_ids() {
		let dir = temp_session_dir("v2");
		write_lines(
			&dir,
			&[
				json!({"type":"session","version":2,"id":"sid","timestamp":"t","cwd":"/p"}),
				json!({"type":"message","id":"m1","parentId":null,"timestamp":"t","message":{"role":"hookMessage","content":"x","customType":"e","display":false,"timestamp":1}}),
				json!({"type":"message","id":"m2","parentId":"m1","timestamp":"t","message":{"role":"user","content":"hi","timestamp":2}}),
			],
		);
		let manager = SessionManager::open(&dir).unwrap();
		assert_eq!(manager.header().unwrap().effective_version(), 3);
		let lines = read_lines(&dir);
		assert_eq!(lines[0]["version"], 3);
		assert_eq!(lines[1]["id"], "m1", "v2 ids are preserved");
		assert_eq!(lines[1]["message"]["role"], "custom");
		assert_eq!(lines[2]["message"]["role"], "user");
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn v3_file_opens_without_rewrite() {
		let dir = temp_session_dir("v3");
		let mut manager = create_in(&dir, Path::new("/p"));
		manager.append_message(&user_message("kept")).unwrap();
		drop(manager);
		let before = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		let manager = SessionManager::open(&dir).unwrap();
		assert_eq!(manager.entries().len(), 1);
		let after = std::fs::read_to_string(dir.join("session.jsonl")).unwrap();
		assert_eq!(before, after, "v3 files are not rewritten");
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn legacy_rust_format_is_rejected_with_clear_error() {
		let dir = temp_session_dir("legacy");
		std::fs::write(
			dir.join("session.jsonl"),
			concat!(
				"{\"type\":\"header\",\"version\":1,\"app\":\"pi-rs 0.1.0\",\"created\":42,\"cwd\":\"/p\"}\n",
				"{\"type\":\"message\",\"id\":null,\"message\":{\"role\":\"user\",\"content\":\"old\",\"timestamp\":1}}\n"
			),
		)
		.unwrap();
		let error = match SessionManager::open(&dir) {
			Err(SessionError::LegacyFormat(message)) => message,
			Err(error) => panic!("expected LegacyFormat, got {error}"),
			Ok(_) => panic!("legacy format must be rejected"),
		};
		assert!(message_contains_legacy(&error), "clear error: {error}");

		validate_rejects_legacy(&dir);
		// The file is untouched.
		assert!(
			std::fs::read_to_string(dir.join("session.jsonl"))
				.unwrap()
				.contains("\"type\":\"header\"")
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	fn message_contains_legacy(message: &str) -> bool {
		message.contains("legacy pi-rs format")
	}

	fn validate_rejects_legacy(dir: &Path) {
		let error = validate_session_dir(dir).unwrap_err();
		assert!(error.contains("legacy pi-rs session format"), "{error}");
	}

	// -- projection ----------------------------------------------------------

	fn assistant_message(text: &str) -> Message {
		Message::Assistant(pi_ai::AssistantMessage {
			content: vec![pi_ai::AssistantContent::Text(pi_ai::TextContent {
				text: text.to_string(),
				text_signature: None,
			})],
			api: "mock".into(),
			provider: "mock".into(),
			model: "mock".into(),
			response_model: None,
			response_id: None,
			thinking_level: None,
			usage: Default::default(),
			stop_reason: pi_ai::StopReason::Stop,
			error_message: None,
			raw_stop_reason: None,
			end_turn: None,
			timestamp: 0,
			duration_ms: None,
		})
	}

	fn linked(manager: &mut SessionManager, kind: EntryKind) -> SessionEntry {
		let mut entry = SessionEntry::new(kind);
		entry.base.parent_id = manager.leaf_id.clone();
		entry.base.id = Some(generate_entry_id(&|id| manager.by_id.contains_key(id)));
		entry.base.timestamp = Some("2024-12-03T14:00:00.000Z".into());
		manager.index_entry(entry.clone());
		entry
	}

	// -- tree navigation -----------------------------------------------------

	fn linked_with_stamp(
		manager: &mut SessionManager,
		kind: EntryKind,
		id: &str,
		timestamp: &str,
	) -> SessionEntry {
		let mut entry = SessionEntry::new(kind);
		entry.base.parent_id = manager.leaf_id.clone();
		entry.base.id = Some(id.to_string());
		entry.base.timestamp = Some(timestamp.to_string());
		manager.index_entry(entry.clone());
		entry
	}

	fn user_text_of(entry: &SessionEntry) -> String {
		match &entry.kind {
			EntryKind::Message { message } => match message.as_ref() {
				Message::User(user) => user.content.as_text(),
				_ => String::new(),
			},
			_ => String::new(),
		}
	}

	#[test]
	fn branch_moves_the_leaf_and_append_forks() {
		let mut manager = SessionManager::in_memory();
		let m1 = linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("root")),
			},
			"aaa10000",
			"2024-12-03T14:00:00.000Z",
		);
		let m2 = linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("first branch")),
			},
			"aaa20000",
			"2024-12-03T14:01:00.000Z",
		);
		// Fork from the root: the next append becomes its second child.
		manager.branch(m1.id().unwrap()).unwrap();
		assert_eq!(manager.leaf_id(), m1.id());
		let appended = manager
			.append(SessionEntry::new(EntryKind::Message {
				message: Box::new(user_message("second branch")),
			}))
			.unwrap()
			.expect("in-memory appends still assign ids");
		let child = manager.get_entry(&appended).unwrap();
		assert_eq!(child.parent_id(), m1.id(), "branches from the fork point");
		// Both children share the parent (the fork exists in one file).
		let children = manager.get_children(m1.id().unwrap());
		assert_eq!(children.len(), 2);
		assert_eq!(children[0].id(), m2.id());
		assert_eq!(children[1].id(), Some(appended.as_str()));
		// Unknown ids fail like upstream's thrown Error.
		assert!(matches!(
			manager.branch("nosuchid"),
			Err(SessionError::EntryNotFound(_))
		));
	}

	#[test]
	fn reset_leaf_starts_a_new_root() {
		let mut manager = SessionManager::in_memory();
		linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("old root")),
			},
			"bbb10000",
			"2024-12-03T14:00:00.000Z",
		);
		manager.reset_leaf();
		assert_eq!(manager.leaf_id(), None);
		let appended = manager
			.append(SessionEntry::new(EntryKind::Message {
				message: Box::new(user_message("new root")),
			}))
			.unwrap()
			.unwrap();
		assert_eq!(
			manager.get_entry(&appended).unwrap().parent_id(),
			None,
			"the next append is a root"
		);
	}

	#[test]
	fn get_tree_builds_the_forest_with_sorted_children() {
		let mut manager = SessionManager::in_memory();
		let root = linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("root")),
			},
			"ccc10000",
			"2024-12-03T14:00:00.000Z",
		);
		// Children appended out of timestamp order to exercise the sort.
		let late = linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("late")),
			},
			"ccc30000",
			"2024-12-03T14:03:00.000Z",
		);
		// Branch back to the root so `early` becomes a second child, appended
		// after `late` in file order with an earlier timestamp.
		manager.branch(root.id().unwrap()).unwrap();
		let early = linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("early")),
			},
			"ccc20000",
			"2024-12-03T14:01:00.000Z",
		);
		// A second root (parentId null) and an orphan (broken parent chain).
		manager.reset_leaf();
		let sibling_root = linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("sibling")),
			},
			"ccc40000",
			"2024-12-03T14:04:00.000Z",
		);
		manager.reset_leaf();
		linked_with_stamp(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("second root")),
			},
			"ccc50000",
			"2024-12-03T14:05:00.000Z",
		);
		let orphan = {
			let mut entry = SessionEntry::new(EntryKind::Message {
				message: Box::new(user_message("orphan")),
			});
			entry.base.parent_id = Some("missing".into());
			entry.base.id = Some("ccc60000".into());
			entry.base.timestamp = Some("2024-12-03T14:06:00.000Z".into());
			manager.index_entry(entry.clone());
			entry
		};
		let _ = &orphan;

		let tree = manager.get_tree();
		assert_eq!(tree.len(), 4, "three deliberate roots plus the orphan");
		let texts: Vec<String> = tree.iter().map(|node| user_text_of(&node.entry)).collect();
		assert_eq!(texts, vec!["root", "sibling", "second root", "orphan"]);
		// Children sort by timestamp ascending even when appended out of order.
		let root_node = &tree[0];
		let child_texts: Vec<String> = root_node
			.children
			.iter()
			.map(|node| user_text_of(&node.entry))
			.collect();
		assert_eq!(child_texts, vec!["early", "late"]);
		assert_eq!(root_node.children[0].entry.id(), early.id());
		assert_eq!(root_node.children[1].entry.id(), late.id());
		// The wire shape nests entries and children.
		let value = root_node.to_value();
		assert_eq!(value["entry"]["id"], root.id().unwrap());
		assert_eq!(value["children"].as_array().unwrap().len(), 2);
		assert_eq!(value["children"][0]["entry"]["id"], early.id().unwrap());
		// Tree entries round-trip through the wire parser.
		assert_eq!(
			SessionEntry::from_value(&value["children"][0]["entry"]).unwrap(),
			early.clone()
		);
		let _ = sibling_root;
	}

	#[test]
	fn get_tree_handles_self_referencing_roots() {
		let mut manager = SessionManager::in_memory();
		let mut entry = SessionEntry::new(EntryKind::Message {
			message: Box::new(user_message("self")),
		});
		entry.base.parent_id = Some("ddd10000".into());
		entry.base.id = Some("ddd10000".into());
		entry.base.timestamp = Some("2024-12-03T14:00:00.000Z".into());
		manager.index_entry(entry);
		let tree = manager.get_tree();
		assert_eq!(tree.len(), 1, "a self-referencing entry is a root");
	}

	// -- branched sessions ---------------------------------------------------

	#[test]
	fn create_branched_session_writes_the_relabeled_path() {
		let dir = temp_session_dir("branched");
		let mut manager = create_in(&dir, Path::new("/project"));
		let source_file = dir.join("session.jsonl");
		let m1 = manager
			.append_message(&user_message("summarized"))
			.unwrap()
			.unwrap();
		let m2 = manager
			.append_message(&user_message("kept"))
			.unwrap()
			.unwrap();
		let m3 = manager
			.append_message(&user_message("also kept"))
			.unwrap()
			.unwrap();
		// A compaction on the main branch: its firstKeptEntryId references m2.
		let mut compaction = SessionEntry::compaction("summary", 10);
		if let EntryKind::Compaction {
			first_kept_entry_id,
			..
		} = &mut compaction.kind
		{
			*first_kept_entry_id = m2.clone();
		}
		let compaction_id = manager.append(compaction).unwrap().unwrap();
		manager
			.append_message(&user_message("after compaction"))
			.unwrap();

		// Fork from m1 and add a branch turn: the branched file carries the
		// m1 → branch-turn path with re-chained parent links.
		manager.branch(&m1).unwrap();
		let branch_child = manager
			.append_message(&user_message("branch turn"))
			.unwrap()
			.unwrap();
		let root = temp_session_dir("branched-root");
		let new_dir = manager
			.create_branched_session_in(&root, &branch_child)
			.unwrap();
		assert_ne!(new_dir, dir, "a sibling session directory");

		let lines = read_lines(&new_dir);
		assert_eq!(lines[0]["type"], "session");
		assert_eq!(lines[0]["version"], 3);
		assert_eq!(
			lines[0]["parentSession"],
			source_file.display().to_string(),
			"the header names the source session file"
		);
		assert!(
			!lines[0]["id"].as_str().unwrap().is_empty(),
			"uuidv7 session id"
		);
		assert_eq!(lines.len(), 3, "header plus the two path entries");
		// The path re-chains: m1 roots at null, the branch child follows it.
		assert_eq!(lines[1]["id"], m1);
		assert_eq!(lines[1]["parentId"], Value::Null);
		assert_eq!(lines[2]["parentId"], m1);
		assert_eq!(lines[2]["id"], branch_child);
		let branch_text = lines[2]["message"]["content"][0]["text"].as_str().unwrap();
		assert_eq!(branch_text, "branch turn");

		// Entry ids survive the re-chain, so a compaction's firstKeptEntryId
		// still references the same (re-linked) id when it is on the path —
		// fork at the compaction leaf to exercise it.
		let compaction_leaf = manager
			.create_branched_session_in(&root, &compaction_id)
			.unwrap();
		let lines = read_lines(&compaction_leaf);
		let compaction_position = lines
			.iter()
			.position(|line| line["type"] == "compaction")
			.unwrap();
		let compaction_line = &lines[compaction_position];
		assert_eq!(
			compaction_line["firstKeptEntryId"], m2,
			"firstKeptEntryId passes through the re-chain unchanged"
		);
		assert_eq!(
			compaction_line["parentId"],
			lines[compaction_position - 1]["id"],
			"the compaction re-chains to its path predecessor"
		);
		// The branched log replays through the standard projection.
		let projection = project_file(&compaction_leaf);
		let texts: Vec<String> = projection
			.messages
			.iter()
			.map(|(_, message)| match message {
				Message::User(user) => user.content.as_text(),
				_ => String::new(),
			})
			.collect();
		assert_eq!(texts.len(), 3);
		assert!(texts[0].contains("summary"));
		assert_eq!(texts[1], "kept", "the firstKeptEntryId range replays");
		assert_eq!(
			texts[2], "also kept",
			"entries between the cut and the compaction replay"
		);
		let _ = m3;
		std::fs::remove_dir_all(dir).unwrap();
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn create_branched_session_rejects_unknown_leaves() {
		let manager = SessionManager::in_memory();
		assert!(matches!(
			manager.create_branched_session("nosuchid"),
			Err(SessionError::EntryNotFound(_))
		));
	}

	// -- session names -------------------------------------------------------

	#[test]
	fn session_info_sanitizes_and_the_name_clears() {
		let mut manager = SessionManager::in_memory();
		assert_eq!(manager.session_name(), None);
		manager
			.append_session_info("refactor\r\nthe\n\nauth module")
			.unwrap();
		assert_eq!(
			manager.session_name().as_deref(),
			Some("refactor the auth module")
		);
		// An empty (after-trim) name clears the title.
		manager.append_session_info("  ").unwrap();
		assert_eq!(manager.session_name(), None);
		// Wire shape: {type:"session_info", name}.
		let named = manager
			.entries()
			.iter()
			.find(|entry| matches!(entry.kind, EntryKind::SessionInfo { name: Some(_) }))
			.unwrap()
			.to_value();
		assert_eq!(named["type"], "session_info");
		assert_eq!(named["name"], "refactor the auth module");
	}

	// -- bash execution entries ----------------------------------------------

	fn bash_entry(exclude: bool) -> EntryKind {
		EntryKind::BashExecution {
			command: "echo hi".into(),
			output: "hi".into(),
			exit_code: Some(0),
			cancelled: false,
			truncated: false,
			full_output_path: None,
			exclude_from_context: exclude,
		}
	}

	#[test]
	fn bash_execution_entry_round_trips_the_upstream_message_shape() {
		let mut manager = SessionManager::in_memory();
		manager
			.append(SessionEntry::new(bash_entry(false)))
			.unwrap();
		let entry = manager
			.entries()
			.iter()
			.find(|entry| matches!(entry.kind, EntryKind::BashExecution { .. }))
			.unwrap()
			.clone();
		let value = entry.to_value();
		assert_eq!(
			value["type"], "message",
			"upstream persists it as a message"
		);
		assert_eq!(value["message"]["role"], "bashExecution");
		assert_eq!(value["message"]["command"], "echo hi");
		assert_eq!(value["message"]["exitCode"], 0);
		assert!(value["message"].get("fullOutputPath").is_none());
		assert!(value["message"].get("excludeFromContext").is_none());
		let parsed = SessionEntry::from_value(&value).unwrap();
		assert_eq!(parsed, entry, "typed round-trip");
		// The excluded form carries the flag; a null exitCode stays null.
		let excluded = SessionEntry::new(EntryKind::BashExecution {
			command: "c".into(),
			output: "o".into(),
			exit_code: None,
			cancelled: false,
			truncated: false,
			full_output_path: None,
			exclude_from_context: true,
		});
		let value = excluded.to_value();
		assert_eq!(value["message"]["excludeFromContext"], true);
		assert_eq!(value["message"]["exitCode"], Value::Null);
		assert!(matches!(
			SessionEntry::from_value(&value).unwrap().kind,
			EntryKind::BashExecution { .. }
		));
	}

	#[test]
	fn bash_execution_replays_as_user_text_unless_excluded() {
		let mut manager = SessionManager::in_memory();
		manager
			.append(SessionEntry::new(bash_entry(false)))
			.unwrap();
		let mut excluded = SessionEntry::new(EntryKind::BashExecution {
			command: "echo secret".into(),
			output: "secret".into(),
			exit_code: None,
			cancelled: true,
			truncated: false,
			full_output_path: None,
			exclude_from_context: true,
		});
		excluded.base.id = Some("bash0001".into());
		manager.append(excluded).unwrap();
		let projection = project_entries(manager.entries());
		assert_eq!(
			projection.messages.len(),
			1,
			"excluded entries skip context"
		);
		let Message::User(user) = &projection.messages[0].1 else {
			panic!("expected the user-form replay")
		};
		assert_eq!(user.content.as_text(), "Ran `echo hi`\n```\nhi\n```");
		// Upstream bashExecutionToText's non-zero-exit and truncated notes.
		assert_eq!(
			bash_execution_to_text("cmd", "out", Some(3), false, true, Some("/tmp/full")),
			"Ran `cmd`\n```\nout\n```\n\nCommand exited with code 3\n\n[Output truncated. Full output: /tmp/full]"
		);
		assert_eq!(
			bash_execution_to_text("cmd", "", None, true, false, None),
			"Ran `cmd`\n(no output)\n\n(command cancelled)"
		);
	}

	#[test]
	fn session_file_persistence_round_trips_bash_entries() {
		let dir = temp_session_dir("bash-persist");
		let mut manager = open_bare(&dir);
		manager
			.append(SessionEntry::new(bash_entry(false)))
			.unwrap();
		drop(manager);
		let (header, entries) = load_session_file(&dir.join("session.jsonl"));
		assert!(header.is_some());
		assert!(matches!(entries[0].kind, EntryKind::BashExecution { .. }));
		assert_eq!(
			super::super::agent_session::read_session_messages(&dir).len(),
			1
		);
		std::fs::remove_dir_all(dir).unwrap();
	}

	#[test]
	fn projection_walks_leaf_to_root() {
		let mut manager = SessionManager::in_memory();
		let m1 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("one")),
			},
		);
		let _m2 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(assistant_message("two")),
			},
		);
		let m3 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("three")),
			},
		);
		// A fork from m1: the second branch reads only its own lineage.
		let mut branch = SessionEntry::new(EntryKind::Message {
			message: Box::new(user_message("branch")),
		});
		branch.base.parent_id = m1.id().map(str::to_owned);
		branch.base.id = Some("b1anch00".into());
		branch.base.timestamp = Some("t".into());
		manager.index_entry(branch.clone());

		let all = manager.entries().to_vec();

		// Default leaf (file order): the newest entry — here the fork tip.
		let default_projection = project_entries(manager.entries());
		let default_texts: Vec<String> = default_projection
			.messages
			.iter()
			.map(|(_, message)| message_text(message))
			.collect();
		assert_eq!(default_texts, vec!["one", "branch"]);

		// Explicit leaf on the main branch → only its own lineage.
		let main_projection = project_entries_with_leaf(&all, m3.id());
		let main_texts: Vec<String> = main_projection
			.messages
			.iter()
			.map(|(_, message)| message_text(message))
			.collect();
		assert_eq!(main_texts, vec!["one", "two", "three"]);

		// Leaf before any entry → empty context.
		assert!(project_entries_with_leaf(&all, None).messages.is_empty());
	}

	fn message_text(message: &Message) -> String {
		match message {
			Message::User(user) => user.content.as_text(),
			Message::Assistant(assistant) => assistant.text(),
			_ => String::new(),
		}
	}

	#[test]
	fn projection_applies_compaction_and_context_edits() {
		let mut manager = SessionManager::in_memory();
		let m1 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("summarized")),
			},
		);
		let m2 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("kept")),
			},
		);
		let m3 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(assistant_message("kept reply")),
			},
		);
		let m4 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("after")),
			},
		);
		// Omit the assistant reply from context.
		let mut edit = SessionEntry::new(EntryKind::ContextEdit {
			target_id: m3.id().unwrap().to_string(),
			replacement: None,
		});
		edit.base.parent_id = m4.id().map(str::to_owned);
		edit.base.id = Some("ed1tf000".into());
		edit.base.timestamp = Some("t".into());
		manager.index_entry(edit.clone());

		// Compaction keeping from m2 onward (upstream firstKeptEntryId).
		let mut compaction = SessionEntry::compaction("summary", 10);
		if let EntryKind::Compaction {
			first_kept_entry_id,
			..
		} = &mut compaction.kind
		{
			*first_kept_entry_id = m2.id().unwrap().to_string();
		}
		compaction.base.parent_id = edit.id().map(str::to_owned);
		compaction.base.timestamp = Some("t".into());
		manager.index_entry(compaction.clone());
		let m5 = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("post-compaction")),
			},
		);

		let projection = project_entries(manager.entries());
		let texts: Vec<String> = projection
			.messages
			.iter()
			.map(|(_, message)| message_text(message))
			.collect();
		// The summary message, the kept "kept" user message (assistant omitted
		// by the edit), the "after" message, and the post-compaction one.
		assert_eq!(texts.len(), 4);
		assert!(texts[0].contains("summary"));
		assert_eq!(texts[1], "kept");
		assert_eq!(texts[2], "after");
		assert_eq!(texts[3], "post-compaction");
		// Entry ids survive projection.
		assert_eq!(projection.messages[3].0, m5.id().map(str::to_owned));
		// usage_valid_from sits past the kept range (the kept tail was
		// measured against a smaller context and is distrusted); the
		// post-compaction message is trusted again.
		assert_eq!(projection.usage_valid_from, 3);
		drop((m1, m2, m3, m4));
	}

	#[test]
	fn projection_records_thinking_and_model_settings() {
		let mut manager = SessionManager::in_memory();
		linked(
			&mut manager,
			EntryKind::ThinkingLevelChange {
				thinking_level: "high".into(),
			},
		);
		linked(
			&mut manager,
			EntryKind::ModelChange {
				provider: "openai".into(),
				model_id: "gpt-4o".into(),
			},
		);
		linked(
			&mut manager,
			EntryKind::Usage {
				kind: "cache_warm".into(),
				provider: "anthropic".into(),
				model: "m".into(),
				usage: Default::default(),
				note: None,
			},
		);
		linked(
			&mut manager,
			EntryKind::SessionInfo {
				name: Some("n".into()),
			},
		);
		let projection = project_entries(manager.entries());
		assert_eq!(projection.thinking_level.as_deref(), Some("high"));
		assert_eq!(projection.model, Some(("openai".into(), "gpt-4o".into())));
		assert!(
			projection.messages.is_empty(),
			"state entries contribute no context"
		);
	}

	#[test]
	fn branch_summary_and_custom_message_project_to_user_form() {
		let mut manager = SessionManager::in_memory();
		linked(
			&mut manager,
			EntryKind::BranchSummary {
				from_id: "elsewhere".into(),
				summary: "explored A".into(),
				details: None,
				usage: None,
				from_hook: None,
			},
		);
		linked(
			&mut manager,
			EntryKind::CustomMessage {
				custom_type: "ext".into(),
				content: Value::String("injected context".into()),
				display: true,
				details: None,
			},
		);
		let projection = project_entries(manager.entries());
		assert_eq!(projection.messages.len(), 2);
		assert!(
			projection.messages[0].1.role() == "user"
				&& message_text(&projection.messages[0].1).contains("explored A")
		);
		assert_eq!(message_text(&projection.messages[1].1), "injected context");
	}

	#[test]
	fn content_replacement_edits_patch_only_content() {
		let mut manager = SessionManager::in_memory();
		let target = linked(
			&mut manager,
			EntryKind::Message {
				message: Box::new(user_message("original")),
			},
		);
		linked(
			&mut manager,
			EntryKind::ContextEdit {
				target_id: target.id().unwrap().to_string(),
				replacement: Some(ContextReplacement {
					content: Value::String("replaced".into()),
				}),
			},
		);
		let projection = project_entries(manager.entries());
		assert_eq!(projection.messages.len(), 1);
		assert_eq!(message_text(&projection.messages[0].1), "replaced");
	}

	// -- resume selection ----------------------------------------------------

	fn resume_root(tag: &str) -> PathBuf {
		let root = std::env::temp_dir().join(format!(
			"pi-rs-resolve-{}-{}-{}",
			tag,
			std::process::id(),
			pi_ai::types::now_ms()
		));
		std::fs::create_dir_all(&root).unwrap();
		root
	}

	fn make_session(root: &Path, id: &str, cwd: &Path, text: &str) -> PathBuf {
		let dir = root.join(id);
		std::fs::create_dir_all(&dir).unwrap();
		let mut session = create_in(&dir, cwd);
		session.append_message(&user_message(text)).unwrap();
		dir
	}

	#[test]
	fn resolve_recent_picks_newest() {
		let root = resume_root("recent");
		let cwd = Path::new("/project");
		make_session(&root, "20260101-000000-1", cwd, "old");
		make_session(&root, "20260303-000000-3", cwd, "new");
		make_session(&root, "20260202-000000-2", cwd, "mid");
		let dir = resolve_session_in(&root, cwd, &SessionSelector::Recent).unwrap();
		assert_eq!(dir.file_name().unwrap(), "20260303-000000-3");
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_recent_filters_by_cwd() {
		let root = resume_root("cwd-filter");
		// Newest session belongs to a different working directory; --continue
		// must skip it (docs/sessions.md: "the most recent session for the
		// current working directory").
		make_session(
			&root,
			"20260303-000000-3",
			Path::new("/elsewhere"),
			"other cwd",
		);
		let mine = make_session(&root, "20260101-000000-1", Path::new("/project"), "mine");
		let dir =
			resolve_session_in(&root, Path::new("/project"), &SessionSelector::Recent).unwrap();
		assert_eq!(dir, mine);

		// An empty header cwd never matches (upstream sessionCwdMatches).
		let empty_cwd = make_session(
			&root,
			"20260404-000000-4",
			Path::new(""),
			"empty cwd header",
		);
		let dir = resolve_session_in(&root, Path::new(""), &SessionSelector::Recent).unwrap_err();
		assert!(dir.contains("no previous session to continue"));
		let _ = empty_cwd;
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_recent_skips_legacy_and_invalid_dirs() {
		let root = resume_root("cwd-skip");
		let cwd = Path::new("/project");
		// Newest dir is a legacy Rust session — skipped, not selected.
		let legacy = root.join("20260505-000000-5");
		std::fs::create_dir_all(&legacy).unwrap();
		std::fs::write(
			legacy.join("session.jsonl"),
			"{\"type\":\"header\",\"version\":1,\"app\":\"pi-rs\",\"created\":1,\"cwd\":\"/project\"}\n",
		)
		.unwrap();
		let mine = make_session(&root, "20260101-000000-1", cwd, "mine");
		let dir = resolve_session_in(&root, cwd, &SessionSelector::Recent).unwrap();
		assert_eq!(dir, mine);

		// Only legacy sessions → clear "none" error rather than a crash.
		let root2 = resume_root("cwd-skip2");
		let legacy2 = root2.join("20260505-000000-5");
		std::fs::create_dir_all(&legacy2).unwrap();
		std::fs::write(
			legacy2.join("session.jsonl"),
			"{\"type\":\"header\",\"version\":1,\"app\":\"pi-rs\",\"created\":1,\"cwd\":\"/project\"}\n",
		)
		.unwrap();
		assert!(
			resolve_session_in(&root2, cwd, &SessionSelector::Recent)
				.unwrap_err()
				.contains("no previous session to continue")
		);
		std::fs::remove_dir_all(root).unwrap();
		std::fs::remove_dir_all(root2).unwrap();
	}

	#[test]
	fn resolve_recent_without_sessions_errors() {
		let root = resume_root("empty");
		assert!(
			resolve_session_in(&root, Path::new("/p"), &SessionSelector::Recent)
				.unwrap_err()
				.contains("no previous session to continue")
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_by_exact_id_and_prefix() {
		let root = resume_root("byid");
		let cwd = Path::new("/p");
		make_session(&root, "20260101-000000-1", cwd, "one");
		let target = make_session(&root, "20260202-000000-2", cwd, "two");
		assert_eq!(
			resolve_session_in(
				&root,
				cwd,
				&SessionSelector::ByIdOrPath("20260202-000000-2".into())
			)
			.unwrap(),
			target
		);
		// Unique prefix match.
		assert_eq!(
			resolve_session_in(&root, cwd, &SessionSelector::ByIdOrPath("20260202".into()))
				.unwrap(),
			target
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_by_path_beats_id_lookup() {
		let root = resume_root("bypath");
		let target = make_session(&root, "20260101-000000-1", Path::new("/p"), "one");
		assert_eq!(
			resolve_session_in(
				&root,
				Path::new("/p"),
				&SessionSelector::ByIdOrPath(target.display().to_string())
			)
			.unwrap(),
			target
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn resolve_errors_are_specific() {
		let root = resume_root("errors");
		let cwd = Path::new("/p");
		make_session(&root, "20260101-000000-1", cwd, "one");
		make_session(&root, "20260101-000000-11", cwd, "eleven");
		assert!(
			resolve_session_in(&root, cwd, &SessionSelector::ByIdOrPath("nope".into()))
				.unwrap_err()
				.contains("no session found matching")
		);
		// Ambiguous prefix (not an exact id) across two sessions.
		assert!(
			resolve_session_in(&root, cwd, &SessionSelector::ByIdOrPath("20260101".into()))
				.unwrap_err()
				.contains("ambiguous")
		);
		// An exact id still wins over a multi-match prefix.
		assert!(
			resolve_session_in(
				&root,
				cwd,
				&SessionSelector::ByIdOrPath("20260101-000000-1".into())
			)
			.is_ok()
		);
		std::fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn validation_rejects_missing_or_headerless_dirs() {
		let root = resume_root("validate");
		let missing = root.join("missing");
		assert!(
			validate_session_dir(&missing)
				.unwrap_err()
				.contains("cannot read")
		);

		let corrupt = root.join("corrupt");
		std::fs::create_dir_all(&corrupt).unwrap();
		std::fs::write(corrupt.join("session.jsonl"), "{\"type\":\"message\"}\n").unwrap();
		assert!(
			validate_session_dir(&corrupt)
				.unwrap_err()
				.contains("no session header")
		);

		let good = make_session(&root, "20260101-000000-1", Path::new("/p"), "one");
		assert!(validate_session_dir(&good).is_ok());
		std::fs::remove_dir_all(root).unwrap();
	}
}
