//! Error type — port of `pi/packages/env/src/errors.ts`.

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("io error: {0}")]
	Io(#[from] std::io::Error),
	#[error("process exited with code {code}: {stderr}")]
	ExitStatus { code: i32, stderr: String },
	#[error("timed out after {0}ms")]
	Timeout(u64),
	#[error("path not found: {0}")]
	NotFound(String),
	#[error("remote environment not implemented yet (pi-rs framework stage)")]
	RemoteUnimplemented,
}

pub type Result<T> = std::result::Result<T, Error>;
