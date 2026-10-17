use std::sync::Arc;

use tokio::sync::watch;

/// A cheap, clonable cancellation token threaded through everything
/// (provider request, stream consumption, tool execution).
#[derive(Clone, Debug)]
pub struct Cancel {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Cancel {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolves as soon as the token is cancelled (immediately if it already is).
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut rx = self.rx.clone();
        let _ = rx.wait_for(|v| *v).await;
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}
