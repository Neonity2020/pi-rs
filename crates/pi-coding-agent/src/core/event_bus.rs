//! Typed event bus — port of `pi/packages/coding-agent/src/core/event-bus.ts`:
//! a thin typed wrapper over broadcast fan-out with unsubscribe handles.

use std::sync::{Arc, Weak};

use tokio::sync::broadcast;

/// Bus over any cloneable event payload.
#[derive(Clone)]
pub struct EventBus<T: Clone> {
	tx: Arc<broadcast::Sender<T>>,
}

/// Non-owning handle: a subscriber that must not keep the channel alive
/// (long-lived closures such as agent hooks hold this and upgrade per emit).
#[derive(Clone)]
pub struct EventBusWeak<T: Clone> {
	tx: Weak<broadcast::Sender<T>>,
}

impl<T: Clone> Default for EventBus<T> {
	fn default() -> Self {
		let (tx, _) = broadcast::channel(1024);
		Self { tx: Arc::new(tx) }
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
		self.tx.clone()
	}

	/// Drop the ownership this handle carries (see [`EventBusWeak`]).
	pub fn downgrade(&self) -> EventBusWeak<T> {
		EventBusWeak {
			tx: Arc::downgrade(&self.tx),
		}
	}
}

impl<T: Clone> EventBusWeak<T> {
	/// The bus, while any owner is alive.
	pub fn upgrade(&self) -> Option<EventBus<T>> {
		self.tx.upgrade().map(|tx| EventBus { tx })
	}
}
