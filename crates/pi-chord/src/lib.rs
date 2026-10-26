//! pi-chord — typed host runtime for plugins, contracts, replicated state,
//! and RPC over JSON links.
//!
//! Rust port of `pi/packages/chord`: cells, deltas, clusters, websocket links,
//! and the plugin host runtime pi uses for distributed/multi-agent setups.
//!
//! Framework stage: intentionally a placeholder. Cells, delta tracking
//! (`delta/diff.rs`, `delta/tracker.rs`), the cluster manager, and the
//! websocket wire protocol are ported when the durable/remote phases land.

/// Cell address inside a chord cluster (placeholder contract).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CellAddress {
	pub cluster: String,
	pub node: String,
	pub cell: String,
}

/// Marker error for unimplemented chord subsystems.
#[derive(Debug, thiserror::Error)]
#[error("chord subsystem not implemented yet (pi-rs framework stage)")]
pub struct Unimplemented;
