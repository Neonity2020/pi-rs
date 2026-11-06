//! PKCE utilities — port of `pi/packages/ai/src/auth/oauth/pkce.ts`.

use sha2::{Digest, Sha256};

use super::types::AuthError;

/// base64url without padding (the JS `btoa(...)replace` shape).
fn base64url_encode(bytes: &[u8]) -> String {
	use base64::Engine;
	base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a PKCE code verifier and its S256 challenge.
pub fn generate_pkce() -> Result<(String, String), AuthError> {
	let mut verifier_bytes = [0u8; 32];
	getrandom_fill(&mut verifier_bytes)?;
	let verifier = base64url_encode(&verifier_bytes);
	let challenge = base64url_encode(&Sha256::digest(verifier.as_bytes()));
	Ok((verifier, challenge))
}

/// Random bytes via the OS CSPRNG (upstream `crypto.getRandomValues`).
pub fn getrandom_fill(buf: &mut [u8]) -> Result<(), AuthError> {
	// uuid's getrandom is not directly reachable; use /dev/urandom on unix and
	// theBCrypt-equivalent elsewhere through a tiny fallback. unix covers
	// every platform pi-rs targets today.
	getrandom_sys(buf)
}

#[cfg(unix)]
fn getrandom_sys(buf: &mut [u8]) -> Result<(), AuthError> {
	use std::io::Read;
	let mut file = std::fs::File::open("/dev/urandom")
		.map_err(|e| AuthError::Login(format!("random: {e}")))?;
	file.read_exact(buf)
		.map_err(|e| AuthError::Login(format!("random: {e}")))
}

#[cfg(not(unix))]
fn getrandom_sys(_buf: &mut [u8]) -> Result<(), AuthError> {
	Err(AuthError::Login(
		"random source unavailable on this platform".to_string(),
	))
}

#[cfg(test)]
mod tests {
	use super::*;
	use base64::Engine as _;

	#[test]
	fn challenge_matches_s256_of_verifier() {
		let (verifier, challenge) = generate_pkce().unwrap();
		// RFC 7636: verifier chars are unreserved/url-safe, no padding.
		assert!(!verifier.contains('+') && !verifier.contains('/') && !verifier.contains('='));
		assert_eq!(verifier.len(), 43); // 32 bytes base64url
		let derived = base64::engine::general_purpose::URL_SAFE_NO_PAD
			.encode(&Sha256::digest(verifier.as_bytes())[..]);
		assert_eq!(challenge, derived);
		// Two calls never share a verifier.
		let (other, _) = generate_pkce().unwrap();
		assert_ne!(verifier, other);
	}
}
