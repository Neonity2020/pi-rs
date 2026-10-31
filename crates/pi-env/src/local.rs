//! Local environment — the default connection, executing on this machine.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use walkdir::WalkDir;

use crate::connection::{DirEntryInfo, EnvConnection, ExecOptions, ExecResult, FileType};
use crate::errors::{Error, Result};

#[derive(Default, Clone)]
pub struct LocalEnv {
	cwd: String,
}

impl LocalEnv {
	pub fn new() -> Self {
		Self {
			cwd: std::env::current_dir()
				.map(|p| p.to_string_lossy().into_owned())
				.unwrap_or_default(),
		}
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

// Continue draining after the limit so a noisy child cannot block on a full pipe.
const MAX_CAPTURE_BYTES: usize = 60_000;

// After the shell exits, a descendant that inherited stdout/stderr keeps the
// pipes open. Finalize once the streams have been silent for this long after
// exit (re-armed on every chunk), instead of waiting for EOF forever. Mirrors
// pi's `waitForChildProcess` (utils/child-process.ts, EXIT_STDIO_GRACE_MS).
const EXIT_STDIO_GRACE_MS: u64 = 100;

// Shared, per-stream output buffer so output already read survives finalizing
// while a lingering descendant still holds the pipe. Output past the cap is
// spilled to a temp file so the bash tool can still hand over the full stream.
struct Capture {
	buf: Arc<std::sync::Mutex<Vec<u8>>>,
	truncated: Arc<std::sync::atomic::AtomicBool>,
	total: Arc<std::sync::atomic::AtomicU64>,
	spill: Arc<std::sync::Mutex<Option<SpillFile>>>,
	error: Arc<std::sync::Mutex<Option<std::io::Error>>>,
	task: tokio::task::JoinHandle<()>,
}

struct SpillFile {
	file: std::fs::File,
	path: std::path::PathBuf,
	keep: bool,
}

impl SpillFile {
	fn create(tag: &str) -> std::io::Result<Self> {
		let (file, path) = crate::create_output_archive(tag)?;
		Ok(Self {
			file,
			path,
			keep: false,
		})
	}

	fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
		std::io::Write::write_all(&mut self.file, bytes)
	}
}

impl Drop for SpillFile {
	fn drop(&mut self) {
		if !self.keep {
			let _ = std::fs::remove_file(&self.path);
		}
	}
}

impl Drop for Capture {
	fn drop(&mut self) {
		self.task.abort();
	}
}

impl Capture {
	fn start<R: AsyncRead + Unpin + Send + 'static>(
		reader: R,
		on_output: Option<crate::connection::OutputCallback>,
		on_chunk: Option<crate::connection::OutputChunkCallback>,
		tag: &'static str,
	) -> Self {
		Self::start_with(reader, on_output, on_chunk, tag, SpillFile::create)
	}

	fn start_with<R: AsyncRead + Unpin + Send + 'static>(
		mut reader: R,
		on_output: Option<crate::connection::OutputCallback>,
		on_chunk: Option<crate::connection::OutputChunkCallback>,
		tag: &'static str,
		create: fn(&str) -> std::io::Result<SpillFile>,
	) -> Self {
		let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
		let truncated = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let total = Arc::new(std::sync::atomic::AtomicU64::new(0));
		let spill = Arc::new(std::sync::Mutex::new(None::<SpillFile>));
		let error = Arc::new(std::sync::Mutex::new(None));
		let task = {
			let buf = buf.clone();
			let truncated = truncated.clone();
			let total = total.clone();
			let spill = spill.clone();
			let error = error.clone();
			tokio::spawn(async move {
				let mut chunk = [0u8; 8192];
				loop {
					match reader.read(&mut chunk).await {
						Ok(0) => break,
						Err(e) => {
							*error.lock().unwrap() = Some(e);
							break;
						}
						Ok(n) => {
							total.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
							if let Some(callback) = &on_output {
								callback(&chunk[..n]);
							}
							if let Some(on_chunk) = &on_chunk {
								on_chunk(&String::from_utf8_lossy(&chunk[..n]));
							}
							let mut guard = buf.lock().unwrap();
							let keep = n.min(MAX_CAPTURE_BYTES.saturating_sub(guard.len()));
							guard.extend_from_slice(&chunk[..keep]);
							if keep < n {
								truncated.store(true, std::sync::atomic::Ordering::Relaxed);
								let mut error = error.lock().unwrap();
								if error.is_some() {
									continue;
								}
								let mut spill = spill.lock().unwrap();
								let result = match spill.as_mut() {
									Some(file) => file.append(&chunk[keep..n]),
									None => create(tag).and_then(|mut file| {
										file.append(&guard)?;
										file.append(&chunk[keep..n])?;
										*spill = Some(file);
										Ok(())
									}),
								};
								if let Err(e) = result {
									*error = Some(std::io::Error::new(
										e.kind(),
										format!("could not archive {tag}: {e}"),
									));
									spill.take();
								}
							}
						}
					}
				}
			})
		};
		Self {
			buf,
			truncated,
			total,
			spill,
			error,
			task,
		}
	}

	fn len(&self) -> u64 {
		self.total.load(std::sync::atomic::Ordering::Relaxed)
	}

	fn is_finished(&self) -> bool {
		self.task.is_finished()
	}

	async fn stop(&mut self) {
		self.task.abort();
		let _ = (&mut self.task).await;
	}

	fn take_error(&self) -> Option<std::io::Error> {
		self.error.lock().unwrap().take()
	}

	fn finish(self) -> (String, Option<String>) {
		let bytes = std::mem::take(&mut *self.buf.lock().unwrap());
		let mut text = String::from_utf8_lossy(&bytes).into_owned();
		if self.truncated.load(std::sync::atomic::Ordering::Relaxed) {
			text.push_str("\n… (output truncated at 60000 bytes)");
		}
		let path = self.spill.lock().unwrap().as_mut().map(|file| {
			file.keep = true;
			file.path.display().to_string()
		});
		(text, path)
	}
}

// Dropping the exec future (for example on cancellation) must also kill the
// shell's descendants, not only the shell held by tokio's Child.
//
// Scope of the guarantee (pi-rs framework stage):
// - Unix: the shell runs as the leader of a new process group; dropping the
//   guard SIGKILLs every group member, so children and grandchildren that
//   stay in the group are cleaned up. A descendant that deliberately escapes
//   (calls setsid()/setpgid into a different group) is NOT covered.
// - Other platforms: tokio's kill_on_drop kills only the direct shell; a
//   detached descendant that survives it is not reaped.
#[cfg(unix)]
struct ProcessGroup(Option<u32>);

#[cfg(unix)]
impl Drop for ProcessGroup {
	fn drop(&mut self) {
		if let Some(pid) = self.0 {
			// The child was started as the leader of a new process group.
			unsafe {
				libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
			}
		}
	}
}

impl LocalEnv {
	/// Shared body of [`EnvConnection::exec`] and
	/// [`EnvConnection::exec_streaming`]; `on_chunk` receives decoded stdout
	/// chunks (never stderr) as they arrive.
	async fn exec_internal(
		&self,
		command: &str,
		options: ExecOptions,
		on_chunk: Option<crate::connection::OutputChunkCallback>,
	) -> Result<ExecResult> {
		let mut cmd = tokio::process::Command::new("sh");
		cmd.arg("-c").arg(command).arg("--");
		if let Some(cwd) = &options.cwd {
			cmd.current_dir(cwd);
		}
		for (key, value) in &options.env {
			cmd.env(key, value);
		}
		cmd.stdin(std::process::Stdio::null());
		cmd.stdout(std::process::Stdio::piped());
		cmd.stderr(std::process::Stdio::piped());
		cmd.kill_on_drop(true);
		#[cfg(unix)]
		{
			use std::os::unix::process::CommandExt;
			cmd.as_std_mut().process_group(0);
		}
		let mut child = cmd.spawn()?;
		#[cfg(unix)]
		let mut group = ProcessGroup(child.id());
		let stdout = child.stdout.take().expect("stdout is piped");
		let stderr = child.stderr.take().expect("stderr is piped");
		let mut out = Capture::start(stdout, options.on_output.clone(), on_chunk, "stdout");
		let mut err = Capture::start(stderr, options.on_output.clone(), None, "stderr");

		let status = match options.timeout_ms {
			Some(timeout_ms) => {
				match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
					Ok(status) => status?,
					// Dropping `group` kills the whole process group (Unix).
					Err(_elapsed) => return Err(Error::Timeout(timeout_ms)),
				}
			}
			None => child.wait().await?,
		};

		// The shell has exited. Normally the pipes close with it; a descendant
		// that inherited stdout/stderr keeps them open. Require one silent
		// grace interval after exit (re-armed on each chunk) before finalizing
		// with the output gathered so far, so an actively writing descendant
		// is drained but an idle inherited handle cannot hang exec forever.
		let grace = Duration::from_millis(EXIT_STDIO_GRACE_MS);
		let mut deadline = tokio::time::Instant::now() + grace;
		let mut last_len = out.len() + err.len();
		while !(out.is_finished() && err.is_finished()) {
			tokio::time::sleep_until(deadline).await;
			let now = out.len() + err.len();
			if now == last_len {
				break;
			}
			last_len = now;
			deadline = tokio::time::Instant::now() + grace;
		}
		out.stop().await;
		err.stop().await;
		if let Some(error) = out.take_error().or_else(|| err.take_error()) {
			return Err(error.into());
		}
		let (stdout, full_stdout_path) = out.finish();
		let (stderr, full_stderr_path) = err.finish();
		#[cfg(unix)]
		{
			group.0 = None;
		}

		#[cfg(unix)]
		let signal = {
			use std::os::unix::process::ExitStatusExt;
			status.signal()
		};
		#[cfg(not(unix))]
		let signal = None;

		Ok(ExecResult {
			stdout,
			stderr,
			exit_code: status.code(),
			signal,
			timed_out: false,
			full_stdout_path,
			full_stderr_path,
		})
	}
}

#[async_trait]
impl EnvConnection for LocalEnv {
	async fn exec(&self, command: &str, options: ExecOptions) -> Result<ExecResult> {
		self.exec_internal(command, options, None).await
	}

	async fn exec_streaming(
		&self,
		command: &str,
		options: ExecOptions,
		on_chunk: crate::connection::OutputChunkCallback,
	) -> Result<ExecResult> {
		self.exec_internal(command, options, Some(on_chunk)).await
	}

	async fn read_file(&self, path: &str) -> Result<String> {
		tokio::fs::read_to_string(path)
			.await
			.map_err(|e| match e.kind() {
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

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn capture_reports_archive_creation_failure_and_drains() {
		use tokio::io::AsyncWriteExt;
		let (mut writer, reader) = tokio::io::duplex(8192);
		let mut capture = Capture::start_with(reader, None, None, "test", |_| {
			Err(std::io::Error::other("injected create failure"))
		});
		writer.write_all(&vec![b'x'; 100_000]).await.unwrap();
		drop(writer);
		while !capture.is_finished() {
			tokio::task::yield_now().await;
		}
		capture.stop().await;
		assert!(
			capture
				.take_error()
				.unwrap()
				.to_string()
				.contains("injected create failure")
		);
		assert!(capture.spill.lock().unwrap().is_none());
	}

	#[tokio::test]
	async fn capture_reports_archive_write_failure() {
		use tokio::io::AsyncWriteExt;
		let (mut writer, reader) = tokio::io::duplex(8192);
		let mut capture = Capture::start_with(reader, None, None, "test", |_| {
			let (file, path) = crate::create_output_archive("readonly-test")?;
			drop(file);
			Ok(SpillFile {
				file: std::fs::File::open(&path)?,
				path,
				keep: false,
			})
		});
		writer.write_all(&vec![b'x'; 100_000]).await.unwrap();
		drop(writer);
		while !capture.is_finished() {
			tokio::task::yield_now().await;
		}
		capture.stop().await;
		assert!(
			capture
				.take_error()
				.unwrap()
				.to_string()
				.contains("could not archive test")
		);
		assert!(capture.spill.lock().unwrap().is_none());
	}

	#[tokio::test]
	async fn abandoned_capture_removes_unclaimed_archive() {
		use tokio::io::AsyncWriteExt;
		let (mut writer, reader) = tokio::io::duplex(8192);
		let capture = Capture::start(reader, None, None, "cancel-test");
		writer.write_all(&vec![b'x'; 100_000]).await.unwrap();
		let path = loop {
			if let Some(path) = capture
				.spill
				.lock()
				.unwrap()
				.as_ref()
				.map(|f| f.path.clone())
			{
				break path;
			}
			tokio::task::yield_now().await;
		};
		assert!(path.exists());
		drop(capture);
		tokio::time::timeout(Duration::from_secs(1), async {
			while path.exists() {
				tokio::task::yield_now().await;
			}
		})
		.await
		.expect("abandoned archive must be removed");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn post_exit_spilled_output_keeps_draining_while_active() {
		let result = LocalEnv::new().exec(
			"(head -c 70000 /dev/zero; sleep 0.06; head -c 1000 /dev/zero; sleep 0.06; head -c 1000 /dev/zero; sleep 0.06; head -c 1000 /dev/zero) & exit",
			ExecOptions::default(),
		).await.unwrap();
		let path = result.full_stdout_path.unwrap();
		assert_eq!(std::fs::metadata(&path).unwrap().len(), 73_000);
		std::fs::remove_file(path).unwrap();
	}

	#[tokio::test]
	async fn limits_and_drains_both_output_streams() {
		let result = LocalEnv::new()
			.exec(
				"yes x | head -c 200000; yes y | head -c 200000 >&2",
				ExecOptions {
					timeout_ms: Some(5000),
					..Default::default()
				},
			)
			.await
			.unwrap();
		assert_eq!(result.exit_code, Some(0));
		assert!(result.stdout.len() < 61_000 && result.stdout.contains("output truncated"));
		assert!(result.stderr.len() < 61_000 && result.stderr.contains("output truncated"));
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn timeout_kills_descendants() {
		let path = std::env::temp_dir().join(format!("pi-env-timeout-{}", std::process::id()));
		let _ = std::fs::remove_file(&path);
		let command = format!("(sleep 1; touch '{}') & wait", path.display());
		let result = LocalEnv::new()
			.exec(
				&command,
				ExecOptions {
					timeout_ms: Some(100),
					..Default::default()
				},
			)
			.await;
		assert!(matches!(result, Err(Error::Timeout(100))));
		tokio::time::sleep(Duration::from_millis(1200)).await;
		assert!(!path.exists(), "descendant survived timeout");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn dropping_exec_kills_descendants() {
		let path = std::env::temp_dir().join(format!("pi-env-cancel-{}", std::process::id()));
		let _ = std::fs::remove_file(&path);
		let command = format!("(sleep 1; touch '{}') & wait", path.display());
		{
			let env = LocalEnv::new();
			let exec = env.exec(&command, ExecOptions::default());
			tokio::pin!(exec);
			tokio::select! {
				_ = &mut exec => panic!("command unexpectedly finished"),
				_ = tokio::time::sleep(Duration::from_millis(100)) => {}
			}
		}
		tokio::time::sleep(Duration::from_millis(1200)).await;
		assert!(!path.exists(), "descendant survived cancellation");
	}

	#[tokio::test]
	async fn exec_returns_while_descendant_holds_pipes() {
		// A backgrounded descendant inherits stdout/stderr; exec must still
		// return promptly after the shell exits (post-exit silence grace).
		let start = std::time::Instant::now();
		let result = LocalEnv::new()
			.exec("sleep 5 & echo started", ExecOptions::default())
			.await
			.unwrap();
		assert!(
			start.elapsed() < Duration::from_secs(3),
			"exec hung on inherited pipes"
		);
		assert_eq!(result.exit_code, Some(0));
		assert!(result.stdout.contains("started"));
	}

	#[tokio::test]
	async fn post_exit_output_is_drained() {
		// A descendant already writing when the shell exits keeps being read;
		// the silence grace re-arms on every chunk instead of a fixed deadline.
		let result = LocalEnv::new()
			.exec(
				"for i in 1 2 3 4 5 6; do echo $i; sleep 0.05; done & exit",
				ExecOptions::default(),
			)
			.await
			.unwrap();
		let ticks = result
			.stdout
			.lines()
			.filter(|l| l.parse::<u32>().is_ok())
			.count();
		assert!(ticks >= 3, "post-exit output truncated: {result:?}");
	}

	#[tokio::test]
	async fn normal_command_skips_grace_when_pipes_close() {
		// Streams end with the shell: no grace delay may be observable.
		let start = std::time::Instant::now();
		let result = LocalEnv::new()
			.exec("echo done", ExecOptions::default())
			.await
			.unwrap();
		assert_eq!(result.stdout.trim(), "done");
		assert!(
			start.elapsed() < Duration::from_millis(500),
			"grace leaked into fast path"
		);
	}

	#[tokio::test]
	async fn overflow_capture_spills_the_full_stream() {
		let result = LocalEnv::new()
			.exec("yes x | head -c 80000", ExecOptions::default())
			.await
			.unwrap();
		assert_eq!(result.exit_code, Some(0));
		assert!(result.stdout.contains("output truncated"));
		let path = result.full_stdout_path.expect("stdout spilled to a file");
		let full = std::fs::read_to_string(&path).unwrap();
		assert_eq!(full.len(), 80_000);
		assert!(result.full_stderr_path.is_none());
		std::fs::remove_file(path).unwrap();
	}

	#[tokio::test]
	async fn exec_streaming_delivers_stdout_chunks_as_they_arrive() {
		let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
		let callback = {
			let seen = seen.clone();
			Arc::new(move |text: &str| seen.lock().unwrap().push(text.to_string()))
				as crate::connection::OutputChunkCallback
		};
		let result = LocalEnv::new()
			.exec_streaming(
				"printf first; echo oops >&2; sleep 0.3; printf second",
				ExecOptions::default(),
				callback,
			)
			.await
			.unwrap();
		let chunks = seen.lock().unwrap();
		let joined = chunks.join("");
		assert!(
			chunks.iter().any(|t| t.contains("first")),
			"first chunk observed: {chunks:?}"
		);
		assert!(
			chunks.len() >= 2,
			"chunks were coalesced into one delivery: {chunks:?}"
		);
		assert_eq!(joined.replace('\n', ""), "firstsecond");
		assert!(result.stdout.contains("first"));
		// stderr is captured but never reaches the chunk callback.
		assert!(!joined.contains("oops"));
		assert!(result.stderr.contains("oops"));
	}
}
