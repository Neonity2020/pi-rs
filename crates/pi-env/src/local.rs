//! Local environment — the default connection, executing on this machine.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use walkdir::WalkDir;

use crate::connection::{
	DirEntryInfo, EnvConnection, ExecOptions, ExecResult, FileType,
};
use crate::errors::{Error, Result};

#[derive(Default, Clone)]
pub struct LocalEnv {
	cwd: String,
}

impl LocalEnv {
	pub fn new() -> Self {
		Self { cwd: std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default() }
	}

	pub fn with_cwd(cwd: impl Into<String>) -> Self {
		Self { cwd: cwd.into() }
	}
}

fn file_type(path: &Path) -> FileType {
	let meta = std::fs::symlink_metadata(path);
	match meta {
		Ok(m) if m.is_dir() => FileType::Dir,
		Ok(m) if m.is_file() => FileType::File,
		Ok(m) if m.file_type().is_symlink() => FileType::Symlink,
		_ => FileType::Other,
	}
}

#[async_trait]
impl EnvConnection for LocalEnv {
	async fn exec(&self, command: &str, options: ExecOptions) -> Result<ExecResult> {
		let mut cmd = tokio::process::Command::new("sh");
		cmd.arg("-c").arg(command).arg("--");
		if let Some(cwd) = &options.cwd {
			cmd.current_dir(cwd);
		}
		for (key, value) in &options.env {
			cmd.env(key, value);
		}
		cmd.stdin(std::process::Stdio::null());
		cmd.kill_on_drop(true);

		let child = cmd.output();
		let output = match options.timeout_ms {
			Some(timeout_ms) => tokio::time::timeout(Duration::from_millis(timeout_ms), child)
				.await
				.map_err(|_| Error::Timeout(timeout_ms))?,
			None => child.await,
		}?;

		Ok(ExecResult {
			stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
			stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
			exit_code: output.status.code(),
			timed_out: false,
		})
	}

	async fn read_file(&self, path: &str) -> Result<String> {
		tokio::fs::read_to_string(path).await.map_err(|e| match e.kind() {
			std::io::ErrorKind::NotFound => Error::NotFound(path.to_string()),
			_ => Error::Io(e),
		})
	}

	async fn write_file(&self, path: &str, contents: &str) -> Result<()> {
		if let Some(parent) = Path::new(path).parent() {
			tokio::fs::create_dir_all(parent).await?;
		}
		tokio::fs::write(path, contents).await?;
		Ok(())
	}

	async fn list_files(&self, path: &str) -> Result<Vec<DirEntryInfo>> {
		let entries: Vec<DirEntryInfo> = WalkDir::new(path)
			.max_depth(1)
			.sort_by_file_name()
			.into_iter()
			.filter_map(|e| e.ok())
			.filter(|e| e.depth() == 1)
			.map(|e| {
				let meta = e.metadata().ok();
				DirEntryInfo {
					path: e.path().to_string_lossy().into_owned(),
					name: e.file_name().to_string_lossy().into_owned(),
					file_type: file_type(e.path()),
					size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
				}
			})
			.collect();
		Ok(entries)
	}

	fn cwd(&self) -> String {
		self.cwd.clone()
	}

	fn describe(&self) -> String {
		"local".to_string()
	}
}
