//! `@file` image pipeline — port of `pi/packages/coding-agent/src/utils/`
//! `mime.ts` (`detectSupportedImageMimeType`), `image-process.ts`
//! (`processImage`), `image-resize-core.ts` (`resizeImageInProcess`), and
//! `exif-orientation.ts`. Upstream runs Photon (Rust/WASM) in a worker; the
//! port uses the native `image` crate with the same strategy: keep
//! png/jpeg/gif/webp as-is, convert everything else to PNG, resize with
//! Lanczos3 when dimensions or the 4.5 MB base64 budget are exceeded, try
//! PNG then decreasing-quality JPEG candidates, and shrink by 75% until the
//! budget fits or the image is 1×1.

use base64::Engine as _;

/// First bytes inspected for the magic-number sniff (upstream
/// `IMAGE_TYPE_SNIFF_BYTES`).
const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// 4.5 MB of base64 payload — headroom below Anthropic's 5 MB limit
/// (upstream `DEFAULT_MAX_BYTES`).
const DEFAULT_MAX_BYTES: usize = 4_500_000;

const DEFAULT_MAX_WIDTH: u32 = 2000;
const DEFAULT_MAX_HEIGHT: u32 = 2000;
const DEFAULT_JPEG_QUALITY: u8 = 80;

// ---------------------------------------------------------------------------
// Magic-number sniffing (mime.ts)
// ---------------------------------------------------------------------------

/// The inline-supported image type of the buffer, `None` for anything else
/// (animated PNGs and malformed BMPs included, like upstream).
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<&'static str> {
	if buffer.starts_with(&[0xff, 0xd8, 0xff]) {
		return if buffer.get(3) == Some(&0xf7) {
			None
		} else {
			Some("image/jpeg")
		};
	}
	if buffer.starts_with(&PNG_SIGNATURE) {
		return if is_png(buffer) && !is_animated_png(buffer) {
			Some("image/png")
		} else {
			None
		};
	}
	if starts_with_ascii(buffer, 0, b"GIF87a") || starts_with_ascii(buffer, 0, b"GIF89a") {
		return Some("image/gif");
	}
	if starts_with_ascii(buffer, 0, b"RIFF") && starts_with_ascii(buffer, 8, b"WEBP") {
		return Some("image/webp");
	}
	if starts_with_ascii(buffer, 0, b"BM") && is_bmp(buffer) {
		return Some("image/bmp");
	}
	None
}

/// Sniff the type from the file's first 4100 bytes.
pub fn detect_supported_image_mime_type_from_file(path: &std::path::Path) -> Option<&'static str> {
	let Ok(file) = std::fs::File::open(path) else {
		return None;
	};
	use std::io::Read;
	let mut handle = file.take(IMAGE_TYPE_SNIFF_BYTES as u64);
	let mut buffer = Vec::new();
	if handle.read_to_end(&mut buffer).is_err() {
		return None;
	}
	detect_supported_image_mime_type(&buffer)
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &[u8]) -> bool {
	buffer.len() >= offset + text.len() && &buffer[offset..offset + text.len()] == text
}

fn read_u32_be(buffer: &[u8], offset: usize) -> u32 {
	u32::from_be_bytes([
		*buffer.get(offset).unwrap_or(&0),
		*buffer.get(offset + 1).unwrap_or(&0),
		*buffer.get(offset + 2).unwrap_or(&0),
		*buffer.get(offset + 3).unwrap_or(&0),
	])
}

fn read_u32_le(buffer: &[u8], offset: usize) -> u32 {
	u32::from_le_bytes([
		*buffer.get(offset).unwrap_or(&0),
		*buffer.get(offset + 1).unwrap_or(&0),
		*buffer.get(offset + 2).unwrap_or(&0),
		*buffer.get(offset + 3).unwrap_or(&0),
	])
}

fn read_u16_le(buffer: &[u8], offset: usize) -> u16 {
	u16::from_le_bytes([
		*buffer.get(offset).unwrap_or(&0),
		*buffer.get(offset + 1).unwrap_or(&0),
	])
}

/// The first chunk after the signature must be a 13-byte IHDR.
fn is_png(buffer: &[u8]) -> bool {
	buffer.len() >= 16
		&& read_u32_be(buffer, PNG_SIGNATURE.len()) == 13
		&& starts_with_ascii(buffer, 12, b"IHDR")
}

/// APNG: an `acTL` chunk before the first `IDAT`.
fn is_animated_png(buffer: &[u8]) -> bool {
	let mut offset = PNG_SIGNATURE.len();
	while offset + 8 <= buffer.len() {
		let chunk_length = read_u32_be(buffer, offset) as usize;
		let chunk_type_offset = offset + 4;
		if starts_with_ascii(buffer, chunk_type_offset, b"acTL") {
			return true;
		}
		if starts_with_ascii(buffer, chunk_type_offset, b"IDAT") {
			return false;
		}
		let next_offset = offset + 8 + chunk_length + 4;
		if next_offset <= offset || next_offset > buffer.len() {
			return false;
		}
		offset = next_offset;
	}
	false
}

fn is_bmp(buffer: &[u8]) -> bool {
	if buffer.len() < 26 {
		return false;
	}
	let declared_file_size = read_u32_le(buffer, 2);
	let pixel_data_offset = read_u32_le(buffer, 10);
	let dib_header_size = read_u32_le(buffer, 14);
	if declared_file_size != 0 && declared_file_size < 26 {
		return false;
	}
	if pixel_data_offset < 14 + dib_header_size {
		return false;
	}
	if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
		return false;
	}
	let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
		(read_u16_le(buffer, 22), read_u16_le(buffer, 24))
	} else if (40..=124).contains(&dib_header_size) {
		if buffer.len() < 30 {
			return false;
		}
		(read_u16_le(buffer, 26), read_u16_le(buffer, 28))
	} else {
		return false;
	};
	color_planes == 1 && matches!(bits_per_pixel, 1 | 4 | 8 | 16 | 24 | 32)
}

// ---------------------------------------------------------------------------
// EXIF orientation (exif-orientation.ts)
// ---------------------------------------------------------------------------

fn read_orientation_from_tiff(bytes: &[u8], tiff_start: usize) -> u16 {
	if tiff_start + 8 > bytes.len() {
		return 1;
	}
	let le = matches!(bytes[tiff_start..tiff_start + 2], [0x49, 0x49]);
	let read16 = |pos: usize| -> u16 {
		let raw = [
			bytes.get(pos).copied().unwrap_or(0),
			bytes.get(pos + 1).copied().unwrap_or(0),
		];
		if le {
			u16::from_le_bytes(raw)
		} else {
			u16::from_be_bytes(raw)
		}
	};
	let read32 = |pos: usize| -> u32 {
		let raw: [u8; 4] = [
			bytes.get(pos).copied().unwrap_or(0),
			bytes.get(pos + 1).copied().unwrap_or(0),
			bytes.get(pos + 2).copied().unwrap_or(0),
			bytes.get(pos + 3).copied().unwrap_or(0),
		];
		if le {
			u32::from_le_bytes(raw)
		} else {
			u32::from_be_bytes(raw)
		}
	};
	let ifd_start = tiff_start + read32(tiff_start + 4) as usize;
	if ifd_start + 2 > bytes.len() {
		return 1;
	}
	let entry_count = read16(ifd_start);
	for entry in 0..entry_count as usize {
		let entry_pos = ifd_start + 2 + entry * 12;
		if entry_pos + 12 > bytes.len() {
			return 1;
		}
		if read16(entry_pos) == 0x0112 {
			let value = read16(entry_pos + 8);
			return if (1..=8).contains(&value) { value } else { 1 };
		}
	}
	1
}

fn find_jpeg_tiff_offset(bytes: &[u8]) -> Option<usize> {
	let mut offset = 2;
	while offset < bytes.len().saturating_sub(1) {
		if bytes[offset] != 0xff {
			return None;
		}
		let marker = bytes[offset + 1];
		if marker == 0xff {
			offset += 1;
			continue;
		}
		if marker == 0xe1 {
			let segment_start = offset + 4;
			if segment_start + 6 > bytes.len() {
				return None;
			}
			if has_exif_header(bytes, segment_start) {
				return Some(segment_start + 6);
			}
		}
		if offset + 4 > bytes.len() {
			return None;
		}
		let length = ((bytes[offset + 2] as usize) << 8) | bytes[offset + 3] as usize;
		offset += 2 + length;
	}
	None
}

fn find_webp_tiff_offset(bytes: &[u8]) -> Option<usize> {
	let mut offset = 12;
	while offset + 8 <= bytes.len() {
		let chunk_id = &bytes[offset..offset + 4];
		let chunk_size = bytes[offset + 4] as u32
			| ((bytes[offset + 5] as u32) << 8)
			| ((bytes[offset + 6] as u32) << 16)
			| ((bytes[offset + 7] as u32) << 24);
		let data_start = offset + 8;
		if chunk_id == b"EXIF" {
			if data_start + chunk_size as usize > bytes.len() {
				return None;
			}
			// Some WebP files have an "Exif\0\0" prefix before the TIFF header.
			let tiff_start = if chunk_size >= 6 && has_exif_header(bytes, data_start) {
				data_start + 6
			} else {
				data_start
			};
			return Some(tiff_start);
		}
		// RIFF chunks are padded to even size.
		offset = data_start + chunk_size as usize + (chunk_size as usize % 2);
	}
	None
}

fn has_exif_header(bytes: &[u8], offset: usize) -> bool {
	bytes.get(offset..offset + 6) == Some(b"Exif\0\0")
}

fn get_exif_orientation(bytes: &[u8]) -> u16 {
	let tiff_offset = if bytes.starts_with(&[0xff, 0xd8]) {
		find_jpeg_tiff_offset(bytes)
	} else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12].starts_with(b"WEBP") {
		find_webp_tiff_offset(bytes)
	} else {
		None
	};
	tiff_offset
		.map(|start| read_orientation_from_tiff(bytes, start))
		.unwrap_or(1)
}

/// Re-orient a decoded image per its EXIF orientation tag (upstream
/// `applyExifOrientation`).
pub fn apply_exif_orientation(
	image: image::DynamicImage,
	original_bytes: &[u8],
) -> image::DynamicImage {
	match get_exif_orientation(original_bytes) {
		2 => image.fliph(),
		3 => image.rotate180(),
		4 => image.flipv(),
		// The image crate has no transpose ops; upstream composes them the
		// same way (rotate + flip).
		5 => image.rotate90().fliph(),
		6 => image.rotate90(),
		7 => image.rotate270().fliph(),
		8 => image.rotate270(),
		_ => image,
	}
}

// ---------------------------------------------------------------------------
// processImage (image-process.ts) + resize (image-resize-core.ts)
// ---------------------------------------------------------------------------

/// A processed inline attachment: base64 payload, resolved mime type, and
/// processing hints appended to the `<file>` block.
pub struct ProcessedImage {
	pub data: String,
	pub mime_type: String,
	pub hints: Vec<String>,
}

/// `processImage` failure messages (upstream text shapes).
#[derive(Debug)]
pub enum ProcessImageError {
	NotConvertible,
	NotResizable,
}

impl ProcessImageError {
	pub fn message(&self) -> &'static str {
		match self {
			Self::NotConvertible => {
				"[Image omitted: could not be converted to a supported inline image format.]"
			}
			Self::NotResizable => {
				"[Image omitted: could not be resized below the inline image size limit.]"
			}
		}
	}
}

/// Mime type without parameters, lowercased.
fn base_mime_type(mime_type: &str) -> &str {
	let trimmed = mime_type.split(';').next().unwrap_or(mime_type).trim();
	// The sniffed/declared set is ASCII; an owned lowercase copy is avoided
	// by matching case-insensitively below.
	trimmed
}

fn is_inline_supported(mime_type: &str) -> bool {
	mime_type.eq_ignore_ascii_case("image/png")
		|| mime_type.eq_ignore_ascii_case("image/jpeg")
		|| mime_type.eq_ignore_ascii_case("image/gif")
		|| mime_type.eq_ignore_ascii_case("image/webp")
}

/// The `processImage` pipeline. Within the dimension/size limits the
/// ORIGINAL bytes pass through unchanged for png/jpeg/gif/webp (upstream
/// returns `inputBytes` verbatim — EXIF orientation is only baked in when a
/// resize or conversion happens); everything else is decoded, oriented, and
/// re-encoded as PNG. The resize path mirrors `resizeImageInProcess`:
/// Lanczos3 to fit 2000×2000, PNG + decreasing-quality JPEG candidates, 75%
/// shrink loop down to 1×1 under the 4.5 MB base64 budget.
pub fn process_image(
	bytes: &[u8],
	mime_type: &str,
	auto_resize: bool,
) -> Result<ProcessedImage, ProcessImageError> {
	let base = base_mime_type(mime_type);
	let passthrough_mime = is_inline_supported(base).then(|| base.to_ascii_lowercase());
	let converted_from = (!passthrough_mime.is_some()).then(|| base.to_string());

	// Decode for dimensions and the resize pipeline; failure -> not
	// convertible (upstream's Photon decode failing returns null).
	let image = image::load_from_memory(bytes).map_err(|_| ProcessImageError::NotConvertible)?;
	let image = apply_exif_orientation(image, bytes);
	let original_width = image.width();
	let original_height = image.height();
	let input_base64_size = base64::engine::general_purpose::STANDARD
		.encode(bytes)
		.len();

	let within_limits = original_width <= DEFAULT_MAX_WIDTH
		&& original_height <= DEFAULT_MAX_HEIGHT
		&& input_base64_size < DEFAULT_MAX_BYTES;
	if !auto_resize || within_limits {
		let (data, mime_type, hints) = match &passthrough_mime {
			Some(mime) => (
				base64::engine::general_purpose::STANDARD.encode(bytes),
				mime.clone(),
				Vec::new(),
			),
			None => {
				let data = encode_png_base64(&image);
				let hints = converted_from
					.iter()
					.map(|from| format!("[Image converted from {from} to image/png.]"))
					.collect();
				(data, "image/png".to_string(), hints)
			}
		};
		return Ok(ProcessedImage {
			data,
			mime_type,
			hints,
		});
	}

	// Initial target dimensions respecting the max limits.
	let mut target_width = original_width;
	let mut target_height = original_height;
	if target_width > DEFAULT_MAX_WIDTH {
		target_height =
			((target_height as u64 * DEFAULT_MAX_WIDTH as u64) / target_width as u64) as u32;
		target_width = DEFAULT_MAX_WIDTH;
	}
	if target_height > DEFAULT_MAX_HEIGHT {
		target_width =
			((target_width as u64 * DEFAULT_MAX_HEIGHT as u64) / target_height as u64) as u32;
		target_height = DEFAULT_MAX_HEIGHT;
	}

	let quality_steps = [DEFAULT_JPEG_QUALITY, 85, 70, 55, 40];
	let mut current_width = target_width.max(1);
	let mut current_height = target_height.max(1);
	loop {
		let resized = image.resize_exact(
			current_width,
			current_height,
			image::imageops::FilterType::Lanczos3,
		);
		// PNG first, then JPEG at decreasing qualities; the first candidate
		// under budget wins (upstream `tryEncodings`).
		let mut candidates: Vec<(String, &'static str)> =
			vec![(encode_png_base64(&resized), "image/png")];
		for quality in quality_steps {
			candidates.push((encode_jpeg_base64(&resized, quality), "image/jpeg"));
		}
		if let Some((data, candidate_mime)) = candidates
			.iter()
			.find(|(data, _)| data.len() < DEFAULT_MAX_BYTES)
		{
			let mut hints = Vec::new();
			if let Some(from) = &converted_from {
				hints.push(format!(
					"[Image converted from {from} to {candidate_mime}.]"
				));
			}
			hints.push(dimension_note(
				original_width,
				original_height,
				current_width,
				current_height,
			));
			return Ok(ProcessedImage {
				data: data.clone(),
				mime_type: candidate_mime.to_string(),
				hints,
			});
		}
		if current_width == 1 && current_height == 1 {
			return Err(ProcessImageError::NotResizable);
		}
		let next_width = (current_width * 3 / 4).max(1);
		let next_height = (current_height * 3 / 4).max(1);
		if next_width == current_width && next_height == current_height {
			return Err(ProcessImageError::NotResizable);
		}
		current_width = next_width;
		current_height = next_height;
	}
}

fn dimension_note(original_width: u32, original_height: u32, width: u32, height: u32) -> String {
	let scale = original_width as f64 / width.max(1) as f64;
	format!(
		"[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {scale:.2} to map to original image.]"
	)
}

fn encode_png_base64(image: &image::DynamicImage) -> String {
	let mut buffer = std::io::Cursor::new(Vec::new());
	let _ = image.write_to(&mut buffer, image::ImageFormat::Png);
	base64::engine::general_purpose::STANDARD.encode(buffer.get_ref())
}

fn encode_jpeg_base64(image: &image::DynamicImage, quality: u8) -> String {
	let rgb = image.to_rgb8();
	let mut buffer = std::io::Cursor::new(Vec::new());
	let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
	let _ = rgb.write_with_encoder(encoder);
	base64::engine::general_purpose::STANDARD.encode(buffer.get_ref())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn png_bytes(width: u32, height: u32) -> Vec<u8> {
		let image = image::DynamicImage::new_rgb8(width, height);
		let mut buffer = std::io::Cursor::new(Vec::new());
		image
			.write_to(&mut buffer, image::ImageFormat::Png)
			.unwrap();
		buffer.into_inner()
	}

	#[test]
	fn sniffs_magic_numbers() {
		assert_eq!(
			detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xe0]),
			Some("image/jpeg")
		);
		assert_eq!(
			detect_supported_image_mime_type(&[0xff, 0xd8, 0xff, 0xf7]),
			None,
			"JPEG extension skipped"
		);
		let png = png_bytes(4, 4);
		assert_eq!(detect_supported_image_mime_type(&png), Some("image/png"));
		assert_eq!(
			detect_supported_image_mime_type(b"GIF89a\0\0\0\0"),
			Some("image/gif")
		);
		assert_eq!(
			detect_supported_image_mime_type(b"RIFF\0\0\0\0WEBPVP8 "),
			Some("image/webp")
		);
		assert_eq!(
			detect_supported_image_mime_type(b"BM\x00\x00"),
			None,
			"malformed BMP rejected"
		);
		assert_eq!(detect_supported_image_mime_type(b"not an image"), None);
	}

	#[test]
	fn apng_is_rejected() {
		// PNG signature + IHDR + an acTL chunk before IDAT.
		let mut bytes = PNG_SIGNATURE.to_vec();
		bytes.extend_from_slice(&13u32.to_be_bytes());
		bytes.extend_from_slice(b"IHDR");
		bytes.extend_from_slice(&[0u8; 13]);
		bytes.extend_from_slice(&[0u8; 4]); // IHDR CRC
		// acTL chunk (len 8) before any IDAT.
		bytes.extend_from_slice(&8u32.to_be_bytes());
		bytes.extend_from_slice(b"acTL");
		bytes.extend_from_slice(&[0u8; 8]);
		assert_eq!(
			detect_supported_image_mime_type(&bytes),
			None,
			"animated PNG skipped"
		);
	}

	#[test]
	fn orientation_lookup_reads_tiff_tag() {
		// Minimal TIFF: II (LE) header, IFD with one entry 0x0112 = 6.
		let mut tiff = Vec::new();
		tiff.extend_from_slice(b"II\x2a\x00");
		tiff.extend_from_slice(&8u32.to_le_bytes());
		tiff.extend_from_slice(&1u16.to_le_bytes()); // entry count
		tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation tag
		tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
		tiff.extend_from_slice(&1u32.to_le_bytes()); // count
		tiff.extend_from_slice(&6u16.to_le_bytes()); // value
		tiff.extend_from_slice(&[0u8; 2]); // value padding
		assert_eq!(read_orientation_from_tiff(&tiff, 0), 6);
	}

	#[test]
	fn process_image_passes_small_images_through_verbatim() {
		let bytes = png_bytes(10, 10);
		let processed = process_image(&bytes, "image/png", true).unwrap();
		assert_eq!(processed.mime_type, "image/png");
		assert!(processed.hints.is_empty(), "no note without a resize");
		// Within limits the original bytes pass through verbatim (upstream
		// returns inputBytes unchanged).
		assert_eq!(
			base64::engine::general_purpose::STANDARD.encode(&bytes),
			processed.data
		);
	}

	#[test]
	fn oversized_images_resize_and_report_dimensions() {
		let bytes = png_bytes(3000, 10);
		let processed = process_image(&bytes, "image/png", true).unwrap();
		assert_eq!(processed.hints.len(), 1, "dimension note");
		assert!(processed.hints[0].contains("original 3000x10"));
		assert!(processed.hints[0].contains("2000x"));
		let decoded = image::load_from_memory(
			&base64::engine::general_purpose::STANDARD
				.decode(&processed.data)
				.unwrap(),
		)
		.unwrap();
		assert_eq!(decoded.width(), 2000);
	}

	#[test]
	fn auto_resize_off_keeps_original_dimensions() {
		let bytes = png_bytes(3000, 10);
		let processed = process_image(&bytes, "image/png", false).unwrap();
		let decoded = image::load_from_memory(
			&base64::engine::general_purpose::STANDARD
				.decode(&processed.data)
				.unwrap(),
		)
		.unwrap();
		assert_eq!(decoded.width(), 3000);
		assert!(processed.hints.is_empty());
	}

	#[test]
	fn garbage_bytes_are_not_convertible() {
		assert!(matches!(
			process_image(b"garbage", "image/png", true),
			Err(ProcessImageError::NotConvertible)
		));
	}

	#[test]
	fn exif_orientation_transforms_decode() {
		// Orientation 6 (90° CW): a 2x1 image displays as 1x2.
		let image = image::DynamicImage::new_rgb8(2, 1);
		let oriented = apply_exif_orientation(image, &[]);
		assert_eq!(oriented.width(), 2, "no EXIF → unchanged");
		let mut jpeg_with_exif = vec![0xff, 0xd8];
		// APP1 segment: marker, length, Exif\0\0, TIFF with orientation 6.
		let mut tiff = Vec::new();
		tiff.extend_from_slice(b"II\x2a\x00");
		tiff.extend_from_slice(&8u32.to_le_bytes());
		tiff.extend_from_slice(&1u16.to_le_bytes()); // entry count
		tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation tag
		tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
		tiff.extend_from_slice(&1u32.to_le_bytes()); // count
		tiff.extend_from_slice(&6u16.to_le_bytes()); // value
		tiff.extend_from_slice(&[0u8; 2]); // value padding
		let mut payload = b"Exif\0\0".to_vec();
		payload.extend_from_slice(&tiff);
		jpeg_with_exif.extend_from_slice(&[0xff, 0xe1]); // APP1 marker
		jpeg_with_exif.extend_from_slice(&(payload.len() as u16 + 2).to_be_bytes());
		jpeg_with_exif.extend_from_slice(&payload);
		let image = image::DynamicImage::new_rgb8(2, 1);
		let oriented = apply_exif_orientation(image, &jpeg_with_exif);
		assert_eq!(
			(oriented.width(), oriented.height()),
			(1, 2),
			"orientation 6 applied"
		);
	}
}
