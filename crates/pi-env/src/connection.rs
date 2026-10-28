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

/// Receives raw output chunks from either stream as they arrive.
pub type OutputCallback = std::sync::Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Receives decoded stdout chunks as they arrive (see
/// [`EnvConnection::exec_streaming`]).
pub type OutputChunkCallback = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ExecOptions {
	/// Working directory for the command.
	pub cwd: Option<String>,
	/// Kill the process after this many milliseconds. `None` = no timeout.
	pub timeout_ms: Option<u64>,
	/// Extra environment variables.
	pub env: Vec<(String, String)>,
	/// Optional incremental output observer. Output is still captured normally.
	pub on_output: Option<OutputCallback>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecResult {
	pub stdout: String,
	pub stderr: String,
	/// Process exit code; non-zero is an error for the caller to interpret.
	pub exit_code: Option<i32>,
	/// Unix signal that terminated the process when there is no exit code
	/// (`ExitStatus::signal()`); `None` on other platforms or a normal exit.
	/// Callers conventionally report the failure as exit code `128 + signal`.
	pub signal: Option<i32>,
	/// True when the command was killed by the timeout.
	pub timed_out: bool,
	/// Complete stdout, spilled to this temp file when the in-memory capture
	/// hit its cap (`None` when the stream fit).
	pub full_stdout_path: Option<String>,
	/// Complete stderr, spilled to this temp file when the in-memory capture
	/// hit its cap (`None` when the stream fit).
	pub full_stderr_path: Option<String>,
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

	/// Run a shell command, streaming decoded stdout chunks to `on_chunk` as
	/// they arrive; stderr is captured without chunk callbacks. Connections
	/// without chunk-level streaming fall back to delivering the whole stdout
	/// as one chunk after the command finished.
	async fn exec_streaming(
		&self,
		command: &str,
		options: ExecOptions,
		on_chunk: OutputChunkCallback,
	) -> Result<ExecResult> {
		let result = self.exec(command, options).await?;
		if !result.stdout.is_empty() {
			on_chunk(&result.stdout);
		}
		Ok(result)
	}

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
