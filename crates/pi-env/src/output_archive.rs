//! Private temporary output archives, cleaned after 24 hours of inactivity.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
static ACTIVE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

#[cfg(unix)]
fn directory() -> io::Result<PathBuf> {
	let path = std::env::temp_dir().join(format!("pi-rs-output-{}", unsafe { libc::geteuid() }));
	let mut builder = std::fs::DirBuilder::new();
	#[cfg(unix)]
	{
		use std::os::unix::fs::DirBuilderExt;
		builder.mode(0o700);
	}
	match builder.create(&path) {
		Ok(()) => {}
		Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
		Err(e) => return Err(e),
	}
	let metadata = std::fs::symlink_metadata(&path)?;
	if !metadata.is_dir() {
		return Err(io::Error::new(
			io::ErrorKind::PermissionDenied,
			"output archive directory is not a directory",
		));
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				"output archive directory is not private",
			));
		}
	}
	Ok(path)
}

#[cfg(not(unix))]
fn directory() -> io::Result<PathBuf> {
	Err(io::Error::new(
		io::ErrorKind::Unsupported,
		"private output archives are not supported on this platform",
	))
}

#[cfg(unix)]
fn process_is_alive(path: &Path) -> bool {
	let Some(pid) = path
		.file_name()
		.and_then(|name| name.to_str())
		.and_then(|name| name.split('-').next())
		.and_then(|pid| pid.parse::<libc::pid_t>().ok())
		.filter(|pid| *pid > 0)
	else {
		return false;
	};
	if unsafe { libc::kill(pid, 0) } == 0 {
		return true;
	}
	std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_is_alive(_path: &Path) -> bool {
	false
}

fn cleanup(dir: &Path, active: &HashSet<PathBuf>) {
	let Ok(entries) = std::fs::read_dir(dir) else {
		return;
	};
	for entry in entries.flatten() {
		let path = entry.path();
		if active.contains(&path) || process_is_alive(&path) {
			continue;
		}
		let Ok(meta) = std::fs::symlink_metadata(&path) else {
			continue;
		};
		if meta.is_file()
			&& meta
				.modified()
				.ok()
				.and_then(|time| time.elapsed().ok())
				.is_some_and(|age| age >= RETENTION)
		{
			let _ = std::fs::remove_file(path);
		}
	}
}

/// Create a private output archive. Files last modified at least 24 hours ago
/// are removed; on Unix, archives of running processes are retained as well.
pub fn create_output_archive(tag: &str) -> io::Result<(File, PathBuf)> {
	static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let dir = directory()?;
	let mut active = ACTIVE.get_or_init(Default::default).lock().unwrap();
	cleanup(&dir, &active);
	let tag: String = tag
		.chars()
		.filter(|c| c.is_ascii_alphanumeric() || *c == '-')
		.collect();
	loop {
		let path = dir.join(format!(
			"{}-{}-{tag}.log",
			std::process::id(),
			COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
		));
		let mut options = OpenOptions::new();
		options.write(true).create_new(true);
		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(0o600);
		}
		match options.open(&path) {
			Ok(file) => {
				active.insert(path.clone());
				return Ok((file, path));
			}
			Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
			Err(e) => return Err(e),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[cfg(unix)]
	#[test]
	fn archive_is_private_and_active_files_are_not_cleaned() {
		let (file, path) = create_output_archive("test").unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::MetadataExt;
			assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
			assert_eq!(
				std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
				0o700
			);
		}
		let mut active = HashSet::new();
		active.insert(path.clone());
		cleanup(path.parent().unwrap(), &active);
		assert!(path.exists());
		drop(file);
		std::fs::remove_file(path).unwrap();
	}
	#[test]
	fn cleanup_removes_only_expired_nonactive_files() {
		let dir = std::env::temp_dir().join(format!("pi-rs-cleanup-test-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let old = dir.join("old");
		let active = dir.join("active");
		let recent = dir.join("recent");
		let live_process = dir.join(format!("{}-other.log", std::process::id()));
		for path in [&old, &active, &recent, &live_process] {
			std::fs::write(path, "test").unwrap();
		}
		let old_time = std::time::SystemTime::now() - RETENTION - Duration::from_secs(60);
		for path in [&old, &active, &live_process] {
			std::fs::File::options()
				.write(true)
				.open(path)
				.unwrap()
				.set_times(std::fs::FileTimes::new().set_modified(old_time))
				.unwrap();
		}
		cleanup(&dir, &HashSet::from([active.clone()]));
		assert!(!old.exists());
		assert!(active.exists());
		assert!(recent.exists());
		#[cfg(unix)]
		assert!(live_process.exists());
		std::fs::remove_dir_all(dir).unwrap();
	}
}
