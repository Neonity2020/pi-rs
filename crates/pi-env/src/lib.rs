//! pi-env — environment connections for pi-rs.
//!
//! Rust port of `pi/packages/env`: an [`EnvConnection`] abstracts where the
//! agent's tools execute — the local machine today, remote hosts over SSH
//! (see `remote-env.ts`) later. Tools and the bash executor program against
//! the trait, so a remote session changes no tool code.

pub mod connection;
pub mod errors;
pub mod local;
pub mod remote;

pub use connection::{
	DirEntryInfo, EnvConnection, ExecOptions, ExecResult, FileType, OutputChunkCallback, WatchEvent,
};
pub use errors::{Error, Result};
pub use local::LocalEnv;
pub use remote::RemoteEnv;
