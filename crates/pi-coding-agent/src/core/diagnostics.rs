//! Resource diagnostics — port of
//! `pi/packages/coding-agent/src/core/diagnostics.ts` (the subset resource
//! loading produces today: warnings and name collisions; `error` rides in the
//! `message` once upstream produces it).

/// Upstream `ResourceCollision` — the two files competing for one name.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ResourceCollision {
	/// `"skill"` today (`"extension" | "prompt" | "theme"` upstream).
	pub resource_type: &'static str,
	pub name: String,
	pub winner_path: String,
	pub loser_path: String,
}

/// Upstream `ResourceDiagnostic`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ResourceDiagnostic {
	Warning {
		message: String,
		#[serde(skip_serializing_if = "Option::is_none")]
		path: Option<String>,
	},
	Collision {
		message: String,
		#[serde(skip_serializing_if = "Option::is_none")]
		path: Option<String>,
		collision: ResourceCollision,
	},
}

impl ResourceDiagnostic {
	pub fn warning(message: impl Into<String>, path: impl Into<String>) -> Self {
		ResourceDiagnostic::Warning {
			message: message.into(),
			path: Some(path.into()),
		}
	}

	pub fn message(&self) -> &str {
		match self {
			ResourceDiagnostic::Warning { message, .. } => message,
			ResourceDiagnostic::Collision { message, .. } => message,
		}
	}

	/// The file the diagnostic points at, when set.
	pub fn path(&self) -> Option<&str> {
		match self {
			ResourceDiagnostic::Warning { path, .. } => path.as_deref(),
			ResourceDiagnostic::Collision { path, .. } => path.as_deref(),
		}
	}
}
