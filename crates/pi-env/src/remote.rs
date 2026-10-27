//! Remote environment over SSH — port of `pi/packages/env/src/remote-env.ts`
//! and `ssh.ts`. Framework stage: contract only; the exec channel multiplexer
//! lands with the remote phase.

use async_trait::async_trait;

use crate::connection::{DirEntryInfo, EnvConnection, ExecOptions, ExecResult};
use crate::errors::Result;

#[derive(Clone)]
pub struct RemoteEnv {
	pub host: String,
	pub port: u16,
	pub user: Option<String>,
}

impl RemoteEnv {
	pub fn new(host: impl Into<String>) -> Self {
		Self {
			host: host.into(),
			port: 22,
			user: None,
		}
	}
}

#[async_trait]
impl EnvConnection for RemoteEnv {
	async fn exec(&self, _command: &str, _options: ExecOptions) -> Result<ExecResult> {
		Err(crate::Error::RemoteUnimplemented)
	}

	async fn read_file(&self, _path: &str) -> Result<String> {
		Err(crate::Error::RemoteUnimplemented)
	}

	async fn write_file(&self, _path: &str, _contents: &str) -> Result<()> {
		Err(crate::Error::RemoteUnimplemented)
	}

	async fn list_files(&self, _path: &str) -> Result<Vec<DirEntryInfo>> {
		Err(crate::Error::RemoteUnimplemented)
	}

	fn cwd(&self) -> String {
		String::new()
	}

	fn describe(&self) -> String {
		match &self.user {
			Some(user) => format!("ssh://{user}@{}:{}", self.host, self.port),
			None => format!("ssh://{}:{}", self.host, self.port),
		}
	}
}
