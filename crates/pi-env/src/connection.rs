//! Environment connection contract — port of `pi/packages/env/src/connection.ts`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileType {
	File,
	Dir,
	Symlink,
	Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntryInfo {
	pub path: String,
	pub name: String,
	pub file_type: FileType,
	pub size: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
	/// Working directory for the command.
	pub cwd: Option<String>,
	/// Kill the process after this many milliseconds. `None` = no timeout.
	pub timeout_ms: Option<u64>,
	/// Extra environment variables.
	pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecResult {
	pub stdout: String,
	pub stderr: String,
	/// Process exit code; non-zero is an error for the caller to interpret.
	pub exit_code: Option<i32>,
	/// True when the command was killed by the timeout.
	pub timed_out: bool,
}

/// Filesystem change notification (see `watch.ts`).
#[derive(Debug, Clone)]
pub struct WatchEvent {
	pub path: String,
	pub kind: WatchEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventKind {
	Created,
	Modified,
	Removed,
}

/// Where tools execute: the local machine or a remote host.
#[async_trait]
pub trait EnvConnection: Send + Sync {
	/// Run a shell command and collect its output.
	async fn exec(&self, command: &str, options: ExecOptions) -> Result<ExecResult>;

	/// Read a UTF-8 file into memory.
	async fn read_file(&self, path: &str) -> Result<String>;

	/// Write a UTF-8 file.
	async fn write_file(&self, path: &str, contents: &str) -> Result<()>;

	/// List one directory level.
	async fn list_files(&self, path: &str) -> Result<Vec<DirEntryInfo>>;

	/// Watch a directory tree for changes. Framework stage: returns an error
	/// until the notify-based watcher lands.
	async fn watch(&self, _path: &str) -> Result<()> {
		Err(crate::Error::RemoteUnimplemented)
	}

	/// Working directory of the connection.
	fn cwd(&self) -> String;

	/// Human label, e.g. `local` or `ssh://host`.
	fn describe(&self) -> String;
}
