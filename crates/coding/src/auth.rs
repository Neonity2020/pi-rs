//! Saved credentials: `$PI_RS_HOME/auth.json`.
//!
//! One JSON object keyed by provider, mirroring pi's auth file:
//!
//! ```json
//! {
//!   "deepseek": {
//!     "type": "api_key",
//!     "key": "sk-...",
//!     "updated_at": "2026-10-17T22:10:00Z"
//!   }
//! }
//! ```
//!
//! `pi login` / `/login` is the only writer. At runtime `--api-key` and the
//! environment still win, so CI can inject a key without touching the file.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::util;

pub const FILE_NAME: &str = "auth.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// Credential kind. Only `api_key` exists today.
    #[serde(rename = "type", default = "api_key_kind")]
    pub kind: String,
    pub key: String,
    /// Endpoint this key belongs to; absent means the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
}

fn api_key_kind() -> String {
    "api_key".to_string()
}

/// The whole file. Serialized as the provider map itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthFile {
    pub providers: BTreeMap<String, Credential>,
}

impl AuthFile {
    pub fn path() -> PathBuf {
        util::home_dir().join(FILE_NAME)
    }

    /// Missing or empty file = no credentials; a broken file is an error.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(Self::default()),
            Ok(text) => serde_json::from_str(&text).with_context(|| {
                format!("parse {} (fix or delete it, then retry)", path.display())
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn credential(provider: &str) -> Result<Option<Credential>> {
        Ok(Self::load()?.providers.get(provider).cloned())
    }

    /// Save (or replace) one provider entry. Returns the file path.
    pub fn save(provider: &str, key: &str, base_url: Option<&str>) -> Result<PathBuf> {
        Self::save_to(&Self::path(), provider, key, base_url)
    }

    pub fn save_to(
        path: &Path,
        provider: &str,
        key: &str,
        base_url: Option<&str>,
    ) -> Result<PathBuf> {
        let mut file = Self::load_from(path)?;
        file.providers.insert(
            provider.to_string(),
            Credential {
                kind: api_key_kind(),
                key: key.trim().to_string(),
                base_url: base_url.map(str::to_string),
                updated_at: util::iso_now(),
            },
        );
        write(path, &file)?;
        Ok(path.to_path_buf())
    }

    pub fn remove(provider: &str) -> Result<bool> {
        Self::remove_from(&Self::path(), provider)
    }

    pub fn remove_from(path: &Path, provider: &str) -> Result<bool> {
        let mut file = Self::load_from(path)?;
        let existed = file.providers.remove(provider).is_some();
        if existed {
            write(path, &file)?;
        }
        Ok(existed)
    }
}

/// Atomic write: temp file with 0600, then rename over the target.
fn write(path: &Path, file: &AuthFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        set_dir_mode(parent);
    }
    let text = serde_json::to_string_pretty(file)?;
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(FILE_NAME),
        std::process::id()
    ));

    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut handle = options
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    handle
        .write_all(text.as_bytes())
        .and_then(|_| handle.write_all(b"\n"))
        .and_then(|_| handle.sync_all())
        .with_context(|| format!("write {}", tmp.display()))?;
    drop(handle);
    set_file_mode(&tmp);

    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    set_file_mode(path);
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path) {}

#[cfg(unix)]
fn set_dir_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-rs-auth-{}-{name}", util::short_id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    #[test]
    fn roundtrip_and_replacement() {
        let path = temp_path("roundtrip");
        assert!(AuthFile::load_from(&path).unwrap().providers.is_empty());

        AuthFile::save_to(&path, "deepseek", "sk-one", Some("https://proxy")).unwrap();
        let file = AuthFile::load_from(&path).unwrap();
        let entry = &file.providers["deepseek"];
        assert_eq!(entry.kind, "api_key");
        assert_eq!(entry.key, "sk-one");
        assert_eq!(entry.base_url.as_deref(), Some("https://proxy"));
        assert!(!entry.updated_at.is_empty());

        // Saving another provider keeps the first, and replaces in place.
        AuthFile::save_to(&path, "openai", "sk-o", None).unwrap();
        AuthFile::save_to(&path, "deepseek", "sk-two", None).unwrap();
        let file = AuthFile::load_from(&path).unwrap();
        assert_eq!(file.providers.len(), 2);
        assert_eq!(file.providers["deepseek"].key, "sk-two");
        assert!(file.providers["deepseek"].base_url.is_none());

        // Keys are trimmed; blank base_url is not invented.
        AuthFile::save_to(&path, "x", "  sk-x \n", None).unwrap();
        assert_eq!(
            AuthFile::load_from(&path).unwrap().providers["x"].key,
            "sk-x"
        );
    }

    #[test]
    fn json_shape_is_the_pi_auth_shape() {
        let path = temp_path("shape");
        AuthFile::save_to(&path, "deepseek", "sk-abc", None).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["deepseek"]["type"], "api_key");
        assert_eq!(value["deepseek"]["key"], "sk-abc");
        assert!(value.get("providers").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("mode");
        AuthFile::save_to(&path, "deepseek", "sk-secret", None).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.json must not be world-readable");
    }

    #[test]
    fn remove_is_idempotent() {
        let path = temp_path("remove");
        AuthFile::save_to(&path, "deepseek", "sk", None).unwrap();
        assert!(AuthFile::remove_from(&path, "deepseek").unwrap());
        assert!(!AuthFile::remove_from(&path, "deepseek").unwrap());
        assert!(AuthFile::load_from(&path).unwrap().providers.is_empty());
    }

    #[test]
    fn corrupt_file_is_an_error_not_silent_data_loss() {
        let path = temp_path("corrupt");
        fs::write(&path, "{not json").unwrap();
        let err = AuthFile::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("parse"), "{err}");

        // Empty file counts as "no credentials".
        fs::write(&path, "  \n").unwrap();
        assert!(AuthFile::load_from(&path).unwrap().providers.is_empty());
    }
}
