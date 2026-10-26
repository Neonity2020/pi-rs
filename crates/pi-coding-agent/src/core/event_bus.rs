//! Typed event bus — port of `pi/packages/coding-agent/src/core/event-bus.ts`:
//! a thin typed wrapper over broadcast fan-out with unsubscribe handles.

use std::sync::Arc;

use tokio::sync::broadcast;

/// Bus over any cloneable event payload.
#[derive(Clone)]
pub struct EventBus<T: Clone> {
	tx: broadcast::Sender<T>,
}

impl<T: Clone> Default for EventBus<T> {
	fn default() -> Self {
		let (tx, _) = broadcast::channel(1024);
		Self { tx }
	}
}

impl<T: Clone> EventBus<T> {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn subscribe(&self) -> broadcast::Receiver<T> {
		self.tx.subscribe()
	}

	pub fn emit(&self, event: T) {
		let _ = self.tx.send(event);
	}

	pub fn sender(&self) -> Arc<broadcast::Sender<T>> {
		Arc::new(self.tx.clone())
	}
}
