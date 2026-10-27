//! Theme — counterpart of `pi/packages/tui/src/colors.ts`,
//! `pi/packages/tui/src/oklab.ts`, and the coding-agent's
//! `src/modes/interactive/theme/` directory (`theme.ts`, `theme-tokens.ts`,
//! `schemas/theme.schema.json`).
//!
//! [`Theme::dark`]/[`Theme::light`] stay the built-in ANSI baselines;
//! [`Theme::from_value`] / [`Theme::from_json_str`] load user theme files
//! following the upstream JSON schema (camelCase fields):
//!
//! Covered:
//! - `name` (required, must not contain `/`), `vars` (with recursive
//!   reference resolution and cycle detection), and `colors`.
//! - Color values, matching upstream `parseColor`: `#rgb`/`#rrggbb`,
//!   `oklch(...)`/`okhsl(...)` (converted to sRGB by the ported Ottosson
//!   math below), 0-255 palette indexes, and `""` (terminal default →
//!   [`Color::Reset`]).
//! - The `colors` tokens pi-tui renders with: `userMessageText` → `user`,
//!   `text` → `assistant`, `thinkingText` → `thinking`, `toolTitle` →
//!   `tool`, `toolOutput` → `tool_output`, plus `error`, `success`,
//!   `border`, `accent`, `muted`, the ten `md*` tokens, and
//!   `userMessageBg` → `user_bg`.
//!
//! Leniently ignored (pi-tui does not render them yet): `$schema`,
//! `appearance`, `export`, unknown top-level fields, and `colors` entries
//! outside the covered list (`borderAccent`, `borderMuted`, `warning`,
//! `dim`, `scrollbar*`, `selectedBg`, `searchMatch*`, `customMessage*`,
//! `toolPendingBg`/`toolSuccessBg`/`toolErrorBg`, `toolDiff*`, `syntax*`,
//! `thinking*`, `bashMode`).
//!
//! Missing covered tokens fall back to [`Theme::dark`]; a malformed value
//! fails the whole load (`Err`), so callers can fall back to the built-in
//! baseline with the error message in hand (upstream `setTheme`).

use std::collections::HashMap;

use ratatui::style::Color;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
	pub name: String,
	/// User/echo text.
	pub user: Color,
	/// Assistant text.
	pub assistant: Color,
	/// Thinking/reasoning text.
	pub thinking: Color,
	/// Tool names and args.
	pub tool: Color,
	/// Tool result output.
	pub tool_output: Color,
	/// Errors and aborted runs.
	pub error: Color,
	/// Success indicators.
	pub success: Color,
	/// Borders and separators.
	pub border: Color,
	/// Prompts, hints, footer.
	pub accent: Color,
	/// Dim metadata (timestamps, token counts).
	pub muted: Color,
	// Markdown element colors — counterpart of pi's `md*` theme tokens
	// (system-theme.ts): mdHeading yellow, mdLink blue, mdCode/mdListBullet
	// violet, mdCodeBlock green, quote/hr/borders dim.
	/// Markdown headings (`mdHeading`).
	pub md_heading: Color,
	/// Markdown link text (`mdLink`).
	pub md_link: Color,
	/// Markdown link URL fallback (`mdLinkUrl`).
	pub md_link_url: Color,
	/// Markdown inline code (`mdCode`).
	pub md_code: Color,
	/// Markdown fenced code content (`mdCodeBlock`).
	pub md_code_block: Color,
	/// Markdown code fence lines (`mdCodeBlockBorder`).
	pub md_code_block_border: Color,
	/// Markdown blockquote text (`mdQuote`).
	pub md_quote: Color,
	/// Markdown blockquote border (`mdQuoteBorder`).
	pub md_quote_border: Color,
	/// Markdown horizontal rules (`mdHr`).
	pub md_hr: Color,
	/// Markdown list bullets/numbers (`mdListBullet`).
	pub md_list_bullet: Color,
	/// Background behind user messages (`userMessageBg`).
	pub user_bg: Color,
}

impl Default for Theme {
	fn default() -> Self {
		Self::dark()
	}
}

impl Theme {
	pub fn dark() -> Self {
		Self {
			name: "pi-dark".into(),
			user: Color::Blue,
			assistant: Color::White,
			thinking: Color::DarkGray,
			tool: Color::Yellow,
			tool_output: Color::Gray,
			error: Color::Red,
			success: Color::Green,
			border: Color::DarkGray,
			accent: Color::Cyan,
			muted: Color::DarkGray,
			md_heading: Color::Yellow,
			md_link: Color::Blue,
			md_link_url: Color::DarkGray,
			md_code: Color::Magenta,
			md_code_block: Color::Green,
			md_code_block_border: Color::DarkGray,
			md_quote: Color::Gray,
			md_quote_border: Color::DarkGray,
			md_hr: Color::DarkGray,
			md_list_bullet: Color::Magenta,
			user_bg: Color::Indexed(236),
		}
	}

	pub fn light() -> Self {
		Self {
			name: "pi-light".into(),
			user: Color::Blue,
			assistant: Color::Black,
			thinking: Color::Indexed(244),
			tool: Color::Indexed(130),
			tool_output: Color::Indexed(238),
			error: Color::Red,
			success: Color::Green,
			border: Color::Indexed(250),
			accent: Color::Cyan,
			muted: Color::Indexed(249),
			md_heading: Color::Indexed(130),
			md_link: Color::Blue,
			md_link_url: Color::Indexed(244),
			md_code: Color::Indexed(127),
			md_code_block: Color::Indexed(28),
			md_code_block_border: Color::Indexed(249),
			md_quote: Color::Indexed(238),
			md_quote_border: Color::Indexed(249),
			md_hr: Color::Indexed(249),
			md_list_bullet: Color::Indexed(127),
			user_bg: Color::Indexed(254),
		}
	}

	/// Load a theme from parsed JSON following the upstream schema. See the
	/// module comment for coverage and fallback rules.
	pub fn from_value(value: &Value) -> Result<Theme, String> {
		let object = value
			.as_object()
			.ok_or_else(|| "Invalid theme: expected a JSON object".to_string())?;

		let name = match object.get("name") {
			Some(Value::String(name)) if !name.is_empty() && !name.contains('/') => name.clone(),
			Some(Value::String(name)) => {
				return Err(format!(
					"Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
				));
			}
			_ => return Err("Invalid theme: missing required string field \"name\"".to_string()),
		};

		let mut vars = HashMap::new();
		if let Some(vars_json) = object.get("vars") {
			let entries = vars_json
				.as_object()
				.ok_or_else(|| "Invalid theme: \"vars\" must be an object".to_string())?;
			for (key, value) in entries {
				vars.insert(key.clone(), parse_color_value(key, value)?);
			}
		}

		let colors = object
			.get("colors")
			.and_then(|colors| colors.as_object())
			.ok_or_else(|| {
				"Invalid theme: expected an object with a \"colors\" map.".to_string()
			})?;

		// Resolve one covered token; absent (or null) tokens fall back to the
		// built-in dark baseline.
		let token = |token: &str, base: Color| -> Result<Color, String> {
			match colors.get(token) {
				None | Some(Value::Null) => Ok(base),
				Some(value) => {
					let parsed = parse_color_value(token, value)?;
					let resolved = resolve_var_refs(parsed, &vars, &mut Vec::new())?;
					Ok(to_color(token, &resolved)?)
				}
			}
		};

		let dark = Theme::dark();
		Ok(Theme {
			name,
			user: token("userMessageText", dark.user)?,
			assistant: token("text", dark.assistant)?,
			thinking: token("thinkingText", dark.thinking)?,
			tool: token("toolTitle", dark.tool)?,
			tool_output: token("toolOutput", dark.tool_output)?,
			error: token("error", dark.error)?,
			success: token("success", dark.success)?,
			border: token("border", dark.border)?,
			accent: token("accent", dark.accent)?,
			muted: token("muted", dark.muted)?,
			md_heading: token("mdHeading", dark.md_heading)?,
			md_link: token("mdLink", dark.md_link)?,
			md_link_url: token("mdLinkUrl", dark.md_link_url)?,
			md_code: token("mdCode", dark.md_code)?,
			md_code_block: token("mdCodeBlock", dark.md_code_block)?,
			md_code_block_border: token("mdCodeBlockBorder", dark.md_code_block_border)?,
			md_quote: token("mdQuote", dark.md_quote)?,
			md_quote_border: token("mdQuoteBorder", dark.md_quote_border)?,
			md_hr: token("mdHr", dark.md_hr)?,
			md_list_bullet: token("mdListBullet", dark.md_list_bullet)?,
			user_bg: token("userMessageBg", dark.user_bg)?,
		})
	}

	/// Load a theme from JSON text (leading BOM stripped, like upstream
	/// `stripBom`).
	pub fn from_json_str(content: &str) -> Result<Theme, String> {
		let content = content.strip_prefix('\u{feff}').unwrap_or(content);
		let value = serde_json::from_str::<Value>(content)
			.map_err(|error| format!("Failed to parse theme: {error}"))?;
		Theme::from_value(&value)
	}
}

// ============================================================================
// Color values — port of `pi/packages/tui/src/colors.ts` (`parseColor`,
// `resolveVarRefs` in the coding-agent's `theme.ts`) and `oklab.ts`.
// ============================================================================

/// A theme color as written in a theme file: a string (hex, `oklch()` /
/// `okhsl()`, `""` terminal default, or a `vars` reference) or a palette
/// index (upstream `ColorValue`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ColorValue {
	Str(String),
	Indexed(u8),
}

fn parse_color_value(token: &str, value: &Value) -> Result<ColorValue, String> {
	match value {
		Value::String(text) => Ok(ColorValue::Str(text.clone())),
		Value::Number(number) => match number.as_u64() {
			Some(index) if index <= 255 => Ok(ColorValue::Indexed(index as u8)),
			_ => Err(format!(
				"Invalid theme color for \"{token}\": palette index must be an integer from 0 to 255"
			)),
		},
		_ => Err(format!(
			"Invalid theme color for \"{token}\": expected a color string or a palette index"
		)),
	}
}

/// Resolve `vars` references (upstream `resolveVarRefs`): hex, `oklch()` /
/// `okhsl()`, and `""` are terminal values; anything else names a variable,
/// resolved recursively with cycle detection.
fn resolve_var_refs(
	value: ColorValue,
	vars: &HashMap<String, ColorValue>,
	visited: &mut Vec<String>,
) -> Result<ColorValue, String> {
	let ColorValue::Str(text) = &value else {
		return Ok(value);
	};
	let lower = text.to_ascii_lowercase();
	if text.is_empty()
		|| text.starts_with('#')
		|| lower.starts_with("oklch(")
		|| lower.starts_with("okhsl(")
	{
		return Ok(value);
	}
	if visited.contains(text) {
		return Err(format!("Circular variable reference detected: {text}"));
	}
	let Some(target) = vars.get(text) else {
		return Err(format!("Variable reference not found: {text}"));
	};
	visited.push(text.clone());
	resolve_var_refs(target.clone(), vars, visited)
}

/// Convert a resolved value to a ratatui color (upstream `parseColor` plus
/// the split into ratatui's color kinds).
fn to_color(token: &str, value: &ColorValue) -> Result<Color, String> {
	match value {
		ColorValue::Indexed(index) => Ok(Color::Indexed(*index)),
		ColorValue::Str(text) => {
			if text.is_empty() {
				return Ok(Color::Reset);
			}
			if let Some(hex) = text.strip_prefix('#') {
				return parse_hex(hex)
					.map(|(r, g, b)| Color::Rgb(r, g, b))
					.ok_or_else(|| {
						format!("Invalid theme color for \"{token}\": invalid hex color \"{text}\"")
					});
			}
			let lower = text.to_ascii_lowercase();
			if let Some(inner) = lower
				.strip_prefix("oklch(")
				.and_then(|rest| rest.strip_suffix(')'))
			{
				return parse_oklch(inner)
					.map(|(l, c, h)| oklch_to_rgb(l, c, h))
					.map(|(r, g, b)| Color::Rgb(r, g, b))
					.ok_or_else(|| {
						format!(
							"Invalid theme color for \"{token}\": invalid oklch color \"{text}\""
						)
					});
			}
			if let Some(inner) = lower
				.strip_prefix("okhsl(")
				.and_then(|rest| rest.strip_suffix(')'))
			{
				return parse_okhsl(inner)
					.map(|(h, s, l)| okhsl_to_rgb(h, s, l))
					.map(|(r, g, b)| Color::Rgb(r, g, b))
					.ok_or_else(|| {
						format!(
							"Invalid theme color for \"{token}\": invalid okhsl color \"{text}\""
						)
					});
			}
			Err(format!(
				"Invalid theme color for \"{token}\": invalid color value \"{text}\""
			))
		}
	}
}

/// `#rgb`/`#rrggbb` digits (without the `#`) → `(r, g, b)`.
fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
	let digits = match hex.len() {
		3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
		6 if hex.chars().all(|c| c.is_ascii_hexdigit()) => hex.to_string(),
		_ => return None,
	};
	if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
		return None;
	}
	Some((
		u8::from_str_radix(&digits[0..2], 16).ok()?,
		u8::from_str_radix(&digits[2..4], 16).ok()?,
		u8::from_str_radix(&digits[4..6], 16).ok()?,
	))
}

/// Parse a numeric token, optionally `deg`-suffixed (hue).
fn parse_number(token: &str) -> Option<f64> {
	token
		.strip_suffix("deg")
		.unwrap_or(token)
		.parse::<f64>()
		.ok()
}

/// Parse a numeric token with an optional `%` suffix, returning it scaled to
/// 0-1 when percent-encoded.
fn parse_percent(token: &str) -> Option<f64> {
	match token.strip_suffix('%') {
		Some(value) => parse_number(value).map(|v| v / 100.0),
		None => parse_number(token),
	}
}

fn parse_oklch(inner: &str) -> Option<(f64, f64, f64)> {
	let parts: Vec<&str> = inner.split_whitespace().collect();
	if parts.len() != 3 {
		return None;
	}
	// Upstream: lightness may be percent-encoded, hue may carry `deg`.
	let l = parse_percent(parts[0])?;
	let c = parse_number(parts[1])?;
	let h = parse_number(parts[2])?;
	Some((l, c, h))
}

fn parse_okhsl(inner: &str) -> Option<(f64, f64, f64)> {
	let parts: Vec<&str> = inner.split_whitespace().collect();
	if parts.len() != 3 {
		return None;
	}
	// Upstream: hue may carry `deg`, saturation and lightness may be
	// percent-encoded.
	let h = parse_number(parts[0])?;
	let s = parse_percent(parts[1])?;
	let l = parse_percent(parts[2])?;
	Some((h, s, l))
}

// ----------------------------------------------------------------------------
// OKHSL/OKLCH → sRGB. Port of `pi/packages/tui/src/oklab.ts`, which is a port
// of Björn Ottosson's reference implementation
// (https://bottosson.github.io/posts/colorpicker/), Copyright (c) 2021,
// used under the MIT license.
// ----------------------------------------------------------------------------

type Vec3 = [f64; 3];
type Mat3 = [Vec3; 3];

fn multiply(m: &Mat3, v: Vec3) -> Vec3 {
	[
		m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
		m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
		m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
	]
}

const LAB_TO_LMS: Mat3 = [
	[1.0, 0.396_337_777_376_174_9, 0.215_803_757_309_913_6],
	[1.0, -0.105_561_345_815_658_6, -0.063_854_172_825_813_3],
	[1.0, -0.089_484_177_529_811_9, -1.291_485_548_019_409_2],
];
const LMS_TO_LINEAR_SRGB: Mat3 = [
	[
		4.076_741_636_075_958,
		-3.307_711_539_258_063,
		0.230_969_903_182_104_3,
	],
	[
		-1.268_437_973_285_031_5,
		2.609_757_349_287_688,
		-0.341_319_376_002_657,
	],
	[
		-0.004_196_076_138_675_6,
		-0.703_418_617_935_936_2,
		1.707_614_694_074_611_7,
	],
];

/// Per sRGB channel (red, green, blue): the (a, b) half-plane where that
/// channel clips first, and the polynomial approximating the maximum
/// saturation there.
const SATURATION_FIT: [([f64; 2], [f64; 5]); 3] = [
	(
		[-1.881_703_1, -0.809_365_01],
		[
			1.190_862_77,
			1.765_767_28,
			0.596_626_41,
			0.755_151_97,
			0.567_712_45,
		],
	),
	(
		[1.814_440_8, -1.194_452_67],
		[
			0.739_565_15,
			-0.459_544_04,
			0.082_854_27,
			0.125_410_73,
			-0.145_032_04,
		],
	),
	(
		[0.131_107_58, 1.813_339_71],
		[
			1.357_336_52,
			-0.009_157_99,
			-1.151_302_1,
			-0.505_596_06,
			0.006_921_67,
		],
	),
];

const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = 1.206 / 1.03; // (1 + K1) / (1 + K2)

/// OKHSL lightness to Oklab lightness.
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
	(x * x + K1 * x) / (K3 * (x + K2))
}

/// sRGB transfer function: linear to encoded channel, both 0-1.
fn linear_to_srgb(value: f64) -> f64 {
	if value > 0.003_130_8 {
		1.055 * value.powf(1.0 / 2.4) - 0.055
	} else {
		12.92 * value
	}
}

/// Oklab [L, a, b] to linear sRGB [r, g, b] (0-1, may leave the gamut).
fn oklab_to_linear_srgb(lab: Vec3) -> Vec3 {
	let lms = multiply(&LAB_TO_LMS, lab);
	multiply(
		&LMS_TO_LINEAR_SRGB,
		[lms[0].powi(3), lms[1].powi(3), lms[2].powi(3)],
	)
}

/// Linear sRGB [r, g, b] to sRGB channels (0-255, rounded), clipping
/// out-of-gamut channels.
fn linear_srgb_to_rgb8(linear: Vec3) -> (u8, u8, u8) {
	let channel = |value: f64| (linear_to_srgb(value).clamp(0.0, 1.0) * 255.0).round() as u8;
	(channel(linear[0]), channel(linear[1]), channel(linear[2]))
}

/// Rate of change of each cube-root LMS component along a chroma direction
/// (a, b).
fn lms_slopes(a: f64, b: f64) -> Vec3 {
	[
		LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
		LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
		LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
	]
}

/// Largest saturation (C/L) inside sRGB for hue (a, b): polynomial fit plus
/// one Halley step.
fn max_saturation(a: f64, b: f64) -> f64 {
	let channel = SATURATION_FIT
		.iter()
		.position(|([x, y], _)| x * a + y * b > 1.0)
		.unwrap_or(2);
	let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
	let weights = &LMS_TO_LINEAR_SRGB[channel];
	let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;

	let slopes = lms_slopes(a, b);
	let base = [
		1.0 + saturation * slopes[0],
		1.0 + saturation * slopes[1],
		1.0 + saturation * slopes[2],
	];
	let dot =
		|values: Vec3| weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2];
	let f = dot([
		base[0] * base[0] * base[0],
		base[1] * base[1] * base[1],
		base[2] * base[2] * base[2],
	]);
	let f1 = dot([
		3.0 * slopes[0] * base[0] * base[0],
		3.0 * slopes[1] * base[1] * base[1],
		3.0 * slopes[2] * base[2] * base[2],
	]);
	let f2 = dot([
		6.0 * slopes[0] * slopes[0] * base[0],
		6.0 * slopes[1] * slopes[1] * base[1],
		6.0 * slopes[2] * slopes[2] * base[2],
	]);
	saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// Oklab lightness and chroma of the most saturated sRGB color of hue (a, b).
fn cusp(a: f64, b: f64) -> (f64, f64) {
	let saturation = max_saturation(a, b);
	let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
	let lightness = (1.0 / linear[0].max(linear[1]).max(linear[2])).cbrt();
	(lightness, lightness * saturation)
}

/// Chroma where the constant-lightness line at `lightness` leaves the sRGB
/// gamut.
fn max_chroma(a: f64, b: f64, lightness: f64, cusp: (f64, f64)) -> f64 {
	let (cusp_l, cusp_c) = cusp;
	if lightness <= cusp_l {
		return (cusp_c * lightness) / cusp_l;
	}
	// Upper half: triangle edge, then one Halley step against each channel
	// reaching 1.
	let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
	let slopes = lms_slopes(a, b);
	let lms = [
		lightness + t * slopes[0],
		lightness + t * slopes[1],
		lightness + t * slopes[2],
	];
	let cubes = [lms[0].powi(3), lms[1].powi(3), lms[2].powi(3)];
	let first = [
		3.0 * slopes[0] * lms[0] * lms[0],
		3.0 * slopes[1] * lms[1] * lms[1],
		3.0 * slopes[2] * lms[2] * lms[2],
	];
	let second = [
		6.0 * slopes[0] * slopes[0] * lms[0],
		6.0 * slopes[1] * slopes[1] * lms[1],
		6.0 * slopes[2] * slopes[2] * lms[2],
	];
	let mut min_step = f64::MAX;
	for row in &LMS_TO_LINEAR_SRGB {
		let dot = |values: Vec3| row[0] * values[0] + row[1] * values[1] + row[2] * values[2];
		let f = dot(cubes) - 1.0;
		let f1 = dot(first);
		let f2 = dot(second);
		let u = f1 / (f1 * f1 - 0.5 * f * f2);
		let step = if u >= 0.0 { -f * u } else { f64::MAX };
		min_step = min_step.min(step);
	}
	t + min_step
}

/// OKHSL's chroma reference points at lightness L and hue (a, b):
/// `(c0, cMid, cMax)`.
fn chroma_stops(lightness: f64, a: f64, b: f64) -> (f64, f64, f64) {
	let peak = cusp(a, b);
	let c_max = max_chroma(a, b, lightness, peak);
	let k =
		c_max / (lightness * (peak.1 / peak.0)).min((1.0 - lightness) * (peak.1 / (1.0 - peak.0)));
	let mid_s = 0.115_169_93
		+ 1.0
			/ (7.447_789_7
				+ 4.159_012_4 * b
				+ a * (-2.195_573_47
					+ 1.751_984_01 * b
					+ a * (-2.137_049_48 - 10.023_010_43 * b
						+ a * (-4.248_945_61 + 5.387_708_19 * b + 4.698_910_13 * a))));
	let mid_t = 0.112_396_42
		+ 1.0
			/ (1.613_203_2 - 0.681_243_79 * b
				+ a * (0.403_706_12
					+ 0.901_481_23 * b
					+ a * (-0.270_879_43
						+ 0.612_239_9 * b + a
						* (0.002_992_15 - 0.453_995_68 * b - 0.146_618_72 * a))));
	let c_mid = 0.9
		* k * ((1.0
		/ (1.0 / (lightness * mid_s).powi(4) + 1.0 / ((1.0 - lightness) * mid_t).powi(4)))
	.sqrt()
	.sqrt());
	let c0 =
		(1.0 / (1.0 / (lightness * 0.4).powi(2) + 1.0 / ((1.0 - lightness) * 0.8).powi(2))).sqrt();
	(c0, c_mid, c_max)
}

/// Convert OKHSL to sRGB channels (0-255, rounded), clipping out-of-gamut
/// channels. Hue in degrees; saturation and lightness 0-1.
fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> (u8, u8, u8) {
	let l = okhsl_to_oklab_lightness(lightness);
	let mut lab: Vec3 = [l, 0.0, 0.0];
	if l > 0.0 && l < 1.0 && saturation > 0.0 {
		let angle = 2.0 * std::f64::consts::PI * hue.rem_euclid(360.0) / 360.0;
		let (a, b) = (angle.cos(), angle.sin());
		let (c0, c_mid, c_max) = chroma_stops(l, a, b);
		// Chroma rises from 0 through cMid at s = 0.8 to cMax at s = 1.
		let chroma = if saturation < 0.8 {
			let t = 1.25 * saturation;
			let k1 = 0.8 * c0;
			(t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
		} else {
			let t = 5.0 * (saturation - 0.8);
			let k1 = 0.2 * c_mid * c_mid * 1.25 * 1.25 / c0;
			c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
		};
		lab = [l, chroma * a, chroma * b];
	}
	linear_srgb_to_rgb8(oklab_to_linear_srgb(lab))
}

/// Convert OKLCH to sRGB channels (upstream `oklchToRgb`): the direct
/// conversion when in gamut, otherwise chroma is reduced (bisection) until
/// the color fits — e.g. `oklch(100% 0.3 150)` maps to white.
fn oklch_to_rgb(l: f64, c: f64, hue: f64) -> (u8, u8, u8) {
	let radians = hue * std::f64::consts::PI / 180.0;
	let (cos, sin) = (radians.cos(), radians.sin());
	let at_chroma = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);

	let direct = at_chroma(c);
	let epsilon = 1e-7;
	let in_gamut = |linear: Vec3| {
		linear[0] >= -epsilon
			&& linear[0] <= 1.0 + epsilon
			&& linear[1] >= -epsilon
			&& linear[1] <= 1.0 + epsilon
			&& linear[2] >= -epsilon
			&& linear[2] <= 1.0 + epsilon
	};
	if in_gamut(direct) {
		return linear_srgb_to_rgb8(direct);
	}

	// The achromatic color is always in gamut, so it is the fallback when no
	// bisection step fits.
	let mut linear = at_chroma(0.0);
	let mut low = 0.0;
	let mut high = c;
	for _ in 0..20 {
		let chroma = (low + high) / 2.0;
		let candidate = at_chroma(chroma);
		if in_gamut(candidate) {
			low = chroma;
			linear = candidate;
		} else {
			high = chroma;
		}
	}
	linear_srgb_to_rgb8(linear)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn from_value_parses_complete_theme() {
		// Fixture outputs generated with the upstream implementation
		// (pi/packages/tui/src/colors.ts parseColor + oklab.ts).
		let json = r##"{
			"$schema": "https://pi.dev/schemas/theme.schema.json",
			"name": "sample",
			"appearance": "dark",
			"vars": {
				"violet": "#c8a6ff",
				"muted": "okhsl(229 6% 67%)",
				"chained": "violet"
			},
			"colors": {
				"accent": "chained",
				"border": "#5555aa",
				"success": "#68b78d",
				"error": 196,
				"muted": "muted",
				"text": "",
				"thinkingText": "oklch(67% 0.1 232deg)",
				"toolTitle": "okhsl(83 88% 67%)",
				"toolOutput": "okhsl(229 6% 67%)",
				"userMessageText": "#ffffff",
				"userMessageBg": 236,
				"mdHeading": "#ffcc00",
				"mdLink": "#69add0",
				"mdLinkUrl": "#9da5a9",
				"mdCode": "#a798d7",
				"mdCodeBlock": "#68b78d",
				"mdCodeBlockBorder": "#9da5a9",
				"mdQuote": "#9da5a9",
				"mdQuoteBorder": "#9da5a9",
				"mdHr": "#9da5a9",
				"mdListBullet": "#a798d7",
				"warning": "#123456",
				"someFutureToken": "#abcdef"
			},
			"export": { "pageBg": "#101010" }
		}"##;
		let value = serde_json::from_str::<Value>(json).unwrap();
		let theme = Theme::from_value(&value).unwrap();

		assert_eq!(theme.name, "sample");
		assert_eq!(theme.accent, Color::Rgb(200, 166, 255)); // var → var → hex
		assert_eq!(theme.border, Color::Rgb(85, 85, 170));
		assert_eq!(theme.success, Color::Rgb(104, 183, 141));
		assert_eq!(theme.error, Color::Indexed(196));
		assert_eq!(theme.muted, Color::Rgb(157, 165, 169)); // okhsl var
		assert_eq!(theme.assistant, Color::Reset); // "" = terminal default
		assert_eq!(theme.thinking, Color::Rgb(77, 160, 200)); // oklch
		assert_eq!(theme.tool, Color::Rgb(205, 154, 34)); // okhsl
		assert_eq!(theme.tool_output, Color::Rgb(157, 165, 169));
		assert_eq!(theme.user, Color::Rgb(255, 255, 255));
		assert_eq!(theme.user_bg, Color::Indexed(236));
		assert_eq!(theme.md_heading, Color::Rgb(255, 204, 0));
		assert_eq!(theme.md_link, Color::Rgb(105, 173, 208));
		assert_eq!(theme.md_code, Color::Rgb(167, 152, 215));
	}

	#[test]
	fn missing_tokens_fall_back_to_dark() {
		let json = r##"{
			"name": "minimal",
			"colors": { "accent": "#ff0000", "mdHeading": "#00ff00" }
		}"##;
		let value = serde_json::from_str::<Value>(json).unwrap();
		let theme = Theme::from_value(&value).unwrap();
		let dark = Theme::dark();

		assert_eq!(theme.name, "minimal");
		assert_eq!(theme.accent, Color::Rgb(255, 0, 0));
		assert_eq!(theme.md_heading, Color::Rgb(0, 255, 0));
		assert_eq!(theme.user, dark.user);
		assert_eq!(theme.assistant, dark.assistant);
		assert_eq!(theme.thinking, dark.thinking);
		assert_eq!(theme.tool, dark.tool);
		assert_eq!(theme.tool_output, dark.tool_output);
		assert_eq!(theme.error, dark.error);
		assert_eq!(theme.success, dark.success);
		assert_eq!(theme.border, dark.border);
		assert_eq!(theme.muted, dark.muted);
		assert_eq!(theme.md_link, dark.md_link);
		assert_eq!(theme.md_code_block, dark.md_code_block);
		assert_eq!(theme.md_quote_border, dark.md_quote_border);
		assert_eq!(theme.md_hr, dark.md_hr);
		assert_eq!(theme.md_list_bullet, dark.md_list_bullet);
		assert_eq!(theme.user_bg, dark.user_bg);
	}

	#[test]
	fn from_value_rejects_bad_themes() {
		let parse = |json: &str| Theme::from_value(&serde_json::from_str::<Value>(json).unwrap());

		let not_object = serde_json::from_str::<Value>("\"nope\"").unwrap();
		let error = Theme::from_value(&not_object).unwrap_err();
		assert!(error.contains("expected a JSON object"), "{error}");

		let error = parse(r#"{"colors": {}}"#).unwrap_err();
		assert!(error.contains("\"name\""), "{error}");

		let error = parse(r#"{"name": "a/b", "colors": {}}"#).unwrap_err();
		assert!(error.contains("cannot contain \"/\""), "{error}");

		let error = parse(r#"{"name": "x"}"#).unwrap_err();
		assert!(error.contains("\"colors\""), "{error}");

		let error = parse(r#"{"name": "x", "colors": {"error": "bogus"}}"#).unwrap_err();
		assert!(error.contains("bogus"), "{error}");

		let error = parse(r#"{"name": "x", "colors": {"error": 300}}"#).unwrap_err();
		assert!(error.contains("0 to 255"), "{error}");

		let error = parse(r#"{"name": "x", "colors": {"error": 1.5}}"#).unwrap_err();
		assert!(error.contains("0 to 255"), "{error}");

		let error = parse(r#"{"name": "x", "colors": {"accent": "nope"}}"#).unwrap_err();
		assert!(
			error.contains("Variable reference not found: nope"),
			"{error}"
		);

		let error =
			parse(r#"{"name": "x", "vars": {"a": "b", "b": "a"}, "colors": {"accent": "a"}}"#)
				.unwrap_err();
		assert!(error.contains("Circular variable reference"), "{error}");

		let error = parse(r#"{"name": "x", "colors": {"accent": "okhsl(83 0.5 0.67 extra)"}}"#)
			.unwrap_err();
		assert!(error.contains("okhsl"), "{error}");

		// On failure the caller falls back to the built-in baseline:
		let theme = parse(r#"{"name": "x", "colors": {"error": "bogus"}}"#)
			.unwrap_or_else(|_| Theme::dark());
		assert_eq!(theme, Theme::dark());
	}

	#[test]
	fn from_json_str_strips_bom_and_reports_invalid_json() {
		let theme = Theme::from_json_str("\u{feff}{\"name\": \"bom\", \"colors\": {}}\n").unwrap();
		assert_eq!(theme.name, "bom");

		let error = Theme::from_json_str("{not json").unwrap_err();
		assert!(error.contains("Failed to parse theme"), "{error}");
	}

	#[test]
	fn okhsl_and_oklch_convert_like_upstream() {
		// Expected values produced by pi/packages/tui/src/{colors,oklab}.ts.
		type OkhslCase<'a> = (&'a str, (f64, f64, f64), (u8, u8, u8));
		let cases: &[OkhslCase] = &[
			("okhsl(83 88% 67%)", (83.0, 0.88, 0.67), (205, 154, 34)),
			("okhsl(232 0.54 0.67)", (232.0, 0.54, 0.67), (105, 173, 208)),
			("okhsl(295 50% 67%)", (295.0, 0.50, 0.67), (167, 152, 215)),
			("okhsl(159 59% 67%)", (159.0, 0.59, 0.67), (104, 183, 141)),
			("okhsl(20 72% 67%)", (20.0, 0.72, 0.67), (234, 127, 129)),
			("okhsl(229 6% 67%)", (229.0, 0.06, 0.67), (157, 165, 169)),
			("okhsl(234 3% 89%)", (234.0, 0.03, 0.89), (222, 224, 225)),
			("okhsl(233 41% 24%)", (233.0, 0.41, 0.24), (33, 59, 73)),
			("okhsl(0 0% 50%)", (0.0, 0.0, 0.5), (119, 119, 119)),
		];
		for (source, (h, s, l), expected) in cases {
			assert_eq!(okhsl_to_rgb(*h, *s, *l), *expected, "{source}");
		}

		assert_eq!(oklch_to_rgb(0.67, 0.1, 232.0), (77, 160, 200));
		// Out-of-gamut oklch must gamut-map to white, not clip channels.
		assert_eq!(oklch_to_rgb(1.0, 0.3, 150.0), (255, 255, 255));
	}

	#[test]
	fn hex_and_indexed_parsing() {
		assert_eq!(parse_hex("c8a6ff"), Some((200, 166, 255)));
		assert_eq!(parse_hex("abc"), Some((170, 187, 204)));
		assert_eq!(parse_hex("12345"), None);
		assert_eq!(parse_hex("zzzzzz"), None);

		assert_eq!(
			parse_color_value("x", &serde_json::json!(236)).unwrap(),
			ColorValue::Indexed(236)
		);
		assert_eq!(
			parse_color_value("x", &serde_json::json!("#c8a6ff")).unwrap(),
			ColorValue::Str("#c8a6ff".into())
		);
		assert!(parse_color_value("x", &serde_json::json!(-1)).is_err());
		assert!(parse_color_value("x", &serde_json::json!(256)).is_err());
		assert!(parse_color_value("x", &serde_json::json!(true)).is_err());
	}
}
