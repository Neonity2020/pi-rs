//! Read an image from the system clipboard.
//!
//! The terminal cannot hand us clipboard image bytes (OSC 52 is text-only),
//! so this shells out to the platform's own tools. No image codec, no new
//! dependencies. Images are capped at 10 MB and kept out of the session
//! unless the user actually sends them.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// Providers accept large uploads, but a runaway clipboard should not silently
/// become a 20 MB session entry.
const MAX_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub media_type: &'static str,
    /// Base64, no data-URL prefix.
    pub data: String,
    pub bytes: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Read the clipboard image, if there is one.
///
/// `Ok(None)` means "the clipboard has no image"; `Err` means the platform
/// tool failed or the image is unusable.
pub fn read_image() -> Result<Option<ClipboardImage>> {
    let path = temp_path();
    let result = read_from(&path);
    let _ = fs::remove_file(&path);
    result
}

fn read_from(path: &Path) -> Result<Option<ClipboardImage>> {
    if !capture(path)? {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }

    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_BYTES {
        bail!(
            "image is larger than 10 MB ({} MB) — resize it first",
            bytes.len() / (1024 * 1024)
        );
    }
    let Some(media_type) = sniff(&bytes) else {
        bail!("clipboard image format is not supported (PNG, JPEG, GIF, or WebP)");
    };
    let (width, height) = match dimensions(&bytes) {
        Some((width, height)) => (Some(width), Some(height)),
        None => (None, None),
    };

    Ok(Some(ClipboardImage {
        media_type,
        data: base64(&bytes),
        bytes: bytes.len(),
        width,
        height,
    }))
}

fn temp_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("pi-rs-clip-{}-{nanos}.png", std::process::id()))
}

// ---------------------------------------------------------------------------
// Platform capture
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn capture(path: &Path) -> Result<bool> {
    let target = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    // Request the PNG flavor: AppKit converts TIFF/GIF/etc. on the way out.
    // A text-only clipboard raises inside the try block, which is "no image".
    let script = format!(
        r#"
try
    set imageData to the clipboard as «class PNGf»
on error
    return "none"
end try
set targetFile to open for access POSIX file "{target}" with write permission
set eof targetFile to 0
write imageData to targetFile
close access targetFile
return "ok"
"#
    );

    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("run osascript (clipboard access)")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("osascript could not read the clipboard: {error}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "ok")
}

#[cfg(target_os = "linux")]
fn capture(path: &Path) -> Result<bool> {
    use std::io::ErrorKind;

    let attempts: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--type", "image/png", "--no-newline"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    let mut ran = false;

    for (program, args) in attempts {
        match std::process::Command::new(program).args(args).output() {
            Ok(output) => {
                ran = true;
                if output.status.success() && !output.stdout.is_empty() {
                    fs::write(path, &output.stdout).context("write clipboard image")?;
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("run {program}"));
            }
        }
    }

    if !ran {
        bail!("no clipboard tool found — install wl-clipboard or xclip");
    }
    Ok(false)
}

#[cfg(target_os = "windows")]
fn capture(path: &Path) -> Result<bool> {
    let target = path.to_string_lossy().into_owned();
    let script = r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$image = [System.Windows.Forms.Clipboard]::GetImage()
if ($null -eq $image) { exit 2 }
$image.Save($env:PI_RS_CLIP_PATH, [System.Drawing.Imaging.ImageFormat]::Png)
"#;

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", script])
        .env("PI_RS_CLIP_PATH", &target)
        .output()
        .context("run powershell (clipboard access)")?;
    match output.status.code() {
        Some(0) => Ok(true),
        // Exit 2 is the script's "clipboard has no image".
        Some(2) => Ok(false),
        _ => {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("powershell could not read the clipboard: {error}");
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn capture(_path: &Path) -> Result<bool> {
    bail!("image paste is not supported on this platform")
}

// ---------------------------------------------------------------------------
// Format helpers
// ---------------------------------------------------------------------------

fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        && bytes.len() >= 24
        && &bytes[12..16] == b"IHDR"
    {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((width, height));
    }

    if (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) && bytes.len() >= 10 {
        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return Some((width, height));
    }

    if bytes.starts_with(&[0xff, 0xd8]) {
        return jpeg_dimensions(bytes);
    }

    None
}

/// Walk JPEG segments until a start-of-frame marker. SOF0/1/2/3, 5/6/7,
/// 9/10/11, 13/14/15 all carry the dimensions; C4/C8/CC do not.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        let mut marker = offset + 1;
        while marker < bytes.len() && bytes[marker] == 0xff {
            marker += 1;
        }
        if marker >= bytes.len() {
            return None;
        }
        let kind = bytes[marker];
        // Standalone markers have no length field.
        if kind == 0x01 || (0xd0..=0xd7).contains(&kind) || kind == 0xd8 {
            offset = marker + 1;
            continue;
        }
        if marker + 2 >= bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes([bytes[marker + 1], bytes[marker + 2]]) as usize;
        if length < 2 {
            return None;
        }
        if (0xc0..=0xcf).contains(&kind) && !matches!(kind, 0xc4 | 0xc8 | 0xcc) {
            let payload = marker + 3;
            if payload + 5 > bytes.len() {
                return None;
            }
            let height = u16::from_be_bytes([bytes[payload + 1], bytes[payload + 2]]) as u32;
            let width = u16::from_be_bytes([bytes[payload + 3], bytes[payload + 4]]) as u32;
            return Some((width, height));
        }
        offset = marker + 1 + length;
    }
    None
}

/// Standard base64 with padding. A dependency is not worth 25 lines.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let bits = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(bits >> 18) as usize & 63] as char);
        out.push(ALPHABET[(bits >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(bits >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[bits as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0, 0, 0]), "AAAA");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 33];
        bytes[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn sniffs_supported_formats() {
        assert_eq!(sniff(&png(1, 1)), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a\x01\x00\x01\x00"), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(sniff(&webp), Some("image/webp"));
        assert_eq!(sniff(b"not an image"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn reads_png_dimensions() {
        assert_eq!(dimensions(&png(2940, 1672)), Some((2940, 1672)));
        assert_eq!(dimensions(&png(1, 1)), Some((1, 1)));
    }

    #[test]
    fn reads_gif_dimensions() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&640u16.to_le_bytes());
        gif.extend_from_slice(&480u16.to_le_bytes());
        gif.extend_from_slice(&[0; 4]);
        assert_eq!(dimensions(&gif), Some((640, 480)));
    }

    #[test]
    fn reads_jpeg_dimensions() {
        // SOI, APP0 (empty), SOF0 with 32x16, EOI.
        let jpeg = [
            0xff, 0xd8, // SOI
            0xff, 0xe0, 0x00, 0x02, // APP0, empty payload
            0xff, 0xc0, 0x00, 0x11, // SOF0, length 17
            0x08, // precision
            0x00, 0x10, // height 16
            0x00, 0x20, // width 32
            0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, // components
        ];
        assert_eq!(dimensions(&jpeg), Some((32, 16)));
        assert_eq!(dimensions(&[0xff, 0xd8, 0xff, 0xd9]), None);
    }

    #[test]
    fn unknown_bytes_have_no_dimensions() {
        assert_eq!(dimensions(b"GIF"), None);
        assert_eq!(dimensions(b"nope"), None);
    }
}
