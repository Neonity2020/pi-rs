//! Device-code flow polling — port of
//! `pi/packages/ai/src/auth/oauth/device-code.ts`.

use std::time::{Duration, Instant};

use super::types::AuthError;

const CANCEL_MESSAGE: &str = "Login cancelled";
const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL_MS: u64 = 1000;
/// RFC 8628 section 3.2: an omitted `interval` means 5 seconds.
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
/// RFC 8628 section 3.5: `slow_down` adds 5 seconds to the interval.
const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// One poll result (`OAuthDeviceCodePollResult`).
pub enum DevicePollResult<T> {
	Pending,
	SlowDown {
		/// Server-provided new interval (GitHub reports it in `interval`).
		interval_seconds: Option<u64>,
	},
	Failed {
		message: String,
	},
	Complete(T),
}

/// Poll a device-code flow until it completes, times out, or is cancelled.
/// `interval_seconds` seeds the first poll gap; `expires_in_seconds` bounds
/// the whole flow; `wait_before_first_poll` matches upstream's option.
pub async fn poll_oauth_device_code_flow<T, F, Fut>(
	interval_seconds: Option<u64>,
	expires_in_seconds: Option<u64>,
	wait_before_first_poll: bool,
	signal: tokio_util::sync::CancellationToken,
	mut poll: F,
) -> Result<T, AuthError>
where
	F: FnMut() -> Fut,
	Fut: std::future::Future<Output = Result<DevicePollResult<T>, AuthError>>,
{
	let deadline = expires_in_seconds.map(|s| Instant::now() + Duration::from_secs(s));
	let mut interval_ms = interval_seconds
		.unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
		.max(1)
		.saturating_mul(1000)
		.max(MINIMUM_INTERVAL_MS);

	let mut slow_down_responses = 0u32;
	if wait_before_first_poll {
		sleep_until(deadline, interval_ms, &signal).await?;
	}

	loop {
		if let Some(deadline) = deadline
			&& Instant::now() >= deadline
		{
			break;
		}
		if signal.is_cancelled() {
			return Err(AuthError::Login(CANCEL_MESSAGE.to_string()));
		}

		match poll().await? {
			DevicePollResult::Complete(value) => return Ok(value),
			DevicePollResult::Failed { message } => return Err(AuthError::Login(message)),
			DevicePollResult::SlowDown { interval_seconds } => {
				slow_down_responses += 1;
				// Trust a server-provided interval when given (GitHub reports
				// the new minimum in `interval`); otherwise RFC 8628 §3.5.
				interval_ms = interval_seconds
					.filter(|s| *s > 0)
					.map(|s| s.max(1).saturating_mul(1000).max(MINIMUM_INTERVAL_MS))
					.unwrap_or_else(|| {
						(interval_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS).max(MINIMUM_INTERVAL_MS)
					});
			}
			DevicePollResult::Pending => {}
		}

		if let Some(deadline) = deadline
			&& Instant::now() >= deadline
		{
			break;
		}
		sleep_until(deadline, interval_ms, &signal).await?;
	}

	Err(AuthError::Login(
		if slow_down_responses > 0 {
			SLOW_DOWN_TIMEOUT_MESSAGE
		} else {
			TIMEOUT_MESSAGE
		}
		.to_string(),
	))
}

/// Abortable sleep clamped to the deadline (upstream `abortableSleep`).
async fn sleep_until(
	deadline: Option<Instant>,
	interval_ms: u64,
	signal: &tokio_util::sync::CancellationToken,
) -> Result<(), AuthError> {
	if signal.is_cancelled() {
		return Err(AuthError::Login(CANCEL_MESSAGE.to_string()));
	}
	let mut delay = Duration::from_millis(interval_ms);
	if let Some(deadline) = deadline {
		let remaining = deadline.saturating_duration_since(Instant::now());
		if remaining.is_zero() {
			return Ok(());
		}
		delay = delay.min(remaining);
	}
	tokio::select! {
		() = tokio::time::sleep(delay) => Ok(()),
		() = signal.cancelled() => Err(AuthError::Login(CANCEL_MESSAGE.to_string())),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn token() -> tokio_util::sync::CancellationToken {
		tokio_util::sync::CancellationToken::new()
	}

	#[tokio::test]
	async fn completes_and_respects_slow_down() {
		// Complete on the second poll; the first slow_down widens the gap.
		let mut polls = 0u32;
		let result =
			poll_oauth_device_code_flow::<_, _, _>(Some(1), Some(5), false, token(), || {
				polls += 1;
				let round = polls;
				async move {
					if round == 1 {
						Ok(DevicePollResult::SlowDown {
							interval_seconds: Some(1),
						})
					} else {
						Ok(DevicePollResult::Complete(round))
					}
				}
			})
			.await
			.unwrap();
		assert_eq!(result, 2);
	}

	#[tokio::test]
	async fn times_out_with_slow_down_message() {
		let error =
			poll_oauth_device_code_flow::<_, _, _>(Some(1), Some(1), false, token(), || async {
				Ok::<_, AuthError>(DevicePollResult::<u8>::SlowDown {
					interval_seconds: Some(1),
				})
			})
			.await
			.unwrap_err();
		assert!(error.to_string().contains("slow_down responses"));
	}

	#[tokio::test]
	async fn cancellation_during_sleep() {
		let signal = token();
		signal.cancel();
		let error = poll_oauth_device_code_flow::<_, _, _>(None, None, false, signal, || async {
			Ok::<_, AuthError>(DevicePollResult::<u8>::Pending)
		})
		.await
		.unwrap_err();
		assert_eq!(error.to_string(), "Login cancelled");
	}
}
