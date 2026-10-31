//! Wheel-scroll acceleration — port of `pi/packages/tui/src/wheel-scroll.ts`.
//!
//! Local macOS terminals receive wheel and trackpad deltas that the OS has
//! already accelerated, and they emit one event per line: there the count is
//! always 1. Other platforms, and SSH sessions where the client platform is
//! unknown, usually send one event per wheel notch — there the count follows
//! event velocity so a fast spin scrolls faster than repeated notches.

/// Lines moved per mouse-wheel event, or [`WheelScrollLines::Auto`] to
/// accelerate fast wheel spins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelScrollLines {
	Auto,
	Fixed(usize),
}

// Several events closer than this belong to one physical notch (Ghostty emits
// them ~4 ms apart) or come from a high-resolution source. They move one line
// each and do not accelerate.
const BURST_GAP_MS: u64 = 5;
// A pause longer than this ends a scroll gesture.
const GESTURE_GAP_MS: i64 = 200;
// Average event gap that maps to one line per event. Faster events scale up
// proportionally.
const REFERENCE_GAP_MS: f64 = 100.0;
const MAX_AUTO_LINES: f64 = 6.0;

/// Whether this terminal already accelerates wheel input: local macOS only.
pub fn terminal_accelerates_wheel() -> bool {
	std::env::var_os("SSH_CONNECTION").is_none()
		&& std::env::var_os("SSH_CLIENT").is_none()
		&& std::env::var_os("SSH_TTY").is_none()
		&& cfg!(target_os = "macos")
}

fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0)
}

/// Converts wheel events into line counts (upstream `WheelScrollAccelerator`).
pub struct WheelScrollAccelerator {
	lines: WheelScrollLines,
	accelerate: bool,
	last_time: Option<u64>,
	last_direction: i8,
	average_gap: Option<f64>,
	carry: f64,
}

impl Default for WheelScrollAccelerator {
	fn default() -> Self {
		Self::new(WheelScrollLines::Auto)
	}
}

impl WheelScrollAccelerator {
	pub fn new(lines: WheelScrollLines) -> Self {
		Self::with_acceleration(lines, !terminal_accelerates_wheel())
	}

	pub fn with_acceleration(lines: WheelScrollLines, accelerate: bool) -> Self {
		Self {
			lines,
			accelerate,
			last_time: None,
			last_direction: 0,
			average_gap: None,
			carry: 0.0,
		}
	}

	pub fn set_lines(&mut self, lines: WheelScrollLines) {
		self.lines = lines;
		self.reset();
	}

	/// Positive line count for a wheel event in `direction` (+1 down, -1 up).
	/// `now` is milliseconds (tests inject a clock; callers use [`now_ms`]).
	pub fn next(&mut self, direction: i8, now: u64) -> usize {
		if let WheelScrollLines::Fixed(count) = self.lines {
			return count.max(1);
		}
		if !self.accelerate {
			return 1;
		}
		let gap = match self.last_time {
			Some(last) => now as i64 - last as i64,
			None => i64::MAX,
		};
		let same_gesture = direction == self.last_direction && gap <= GESTURE_GAP_MS;
		self.last_time = Some(now);
		self.last_direction = direction;
		if !same_gesture {
			self.average_gap = None;
			self.carry = 0.0;
			return 1;
		}
		if gap < BURST_GAP_MS as i64 {
			return 1;
		}
		let gap = gap as f64;
		self.average_gap = Some(match self.average_gap {
			Some(average) => (average + gap) / 2.0,
			None => gap,
		});
		let lines = MAX_AUTO_LINES
			.min(REFERENCE_GAP_MS / self.average_gap.unwrap_or(REFERENCE_GAP_MS))
			+ self.carry;
		let whole = lines.floor();
		self.carry = lines - whole;
		whole.max(1.0) as usize
	}

	fn reset(&mut self) {
		self.last_time = None;
		self.last_direction = 0;
		self.average_gap = None;
		self.carry = 0.0;
	}
}

/// Millisecond clock for [`WheelScrollAccelerator::next`].
pub fn wheel_now_ms() -> u64 {
	now_ms()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fixed_lines_are_always_at_least_one() {
		let mut wheel = WheelScrollAccelerator::new(WheelScrollLines::Fixed(4));
		assert_eq!(wheel.next(1, 0), 4);
		assert_eq!(wheel.next(-1, 10_000), 4);
		let mut wheel = WheelScrollAccelerator::new(WheelScrollLines::Fixed(0));
		assert_eq!(wheel.next(1, 0), 1);
	}

	#[test]
	fn no_acceleration_moves_one_line_per_event() {
		// macOS local terminals: the OS already accelerates; every event —
		// however fast — moves exactly one line.
		let mut wheel = WheelScrollAccelerator::with_acceleration(WheelScrollLines::Auto, false);
		for now in [0, 3, 6, 9, 12, 100, 5_000] {
			assert_eq!(wheel.next(1, now), 1, "now={now}");
		}
	}

	#[test]
	fn isolated_notch_moves_one_line() {
		let mut wheel = WheelScrollAccelerator::with_acceleration(WheelScrollLines::Auto, true);
		assert_eq!(wheel.next(1, 0), 1, "first event of a gesture");
		assert_eq!(
			wheel.next(1, 10_000),
			1,
			"gap beyond the gesture window restarts"
		);
		assert_eq!(wheel.next(-1, 10_050), 1, "direction change restarts");
	}

	#[test]
	fn burst_events_move_one_line_each() {
		let mut wheel = WheelScrollAccelerator::with_acceleration(WheelScrollLines::Auto, true);
		assert_eq!(wheel.next(1, 0), 1);
		// Ghostty-style ~4 ms apart: one physical notch, one line each.
		assert_eq!(wheel.next(1, 4), 1);
		assert_eq!(wheel.next(1, 8), 1);
	}

	#[test]
	fn fast_spin_accelerates_up_to_six_lines() {
		let mut wheel = WheelScrollAccelerator::with_acceleration(WheelScrollLines::Auto, true);
		assert_eq!(wheel.next(1, 0), 1);
		// 20 ms gaps average: 100/20 = 5 lines per event.
		assert_eq!(wheel.next(1, 20), 5);
		assert_eq!(wheel.next(1, 40), 5);
		// A long pause ends the gesture; the next event is a fresh notch.
		assert_eq!(wheel.next(1, 40 + 500), 1);
	}

	#[test]
	fn slow_spin_stays_at_one_line() {
		let mut wheel = WheelScrollAccelerator::with_acceleration(WheelScrollLines::Auto, true);
		assert_eq!(wheel.next(1, 0), 1);
		// 100 ms gaps: reference velocity, 1 line per event.
		assert_eq!(wheel.next(1, 100), 1);
		assert_eq!(wheel.next(1, 200), 1);
	}
}
