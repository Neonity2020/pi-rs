use thiserror::Error;

/// Every failure that can surface from a provider call.
/// Tool failures are *not* errors — they are tool results.
#[derive(Debug, Error)]
pub enum Error {
    #[error("cancelled")]
    Cancelled,
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error (status {status}): {body}")]
    Api { status: u16, body: String },
    #[error("stream error: {0}")]
    Stream(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
