//! Replace a regular file only after its complete replacement is on disk.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agent::Cancel;

pub(super) fn write(path: &Path, content: &[u8], cancel: &Cancel) -> io::Result<()> {
    replace(path, cancel, |file| file.write_all(content))
}

fn replace(
    path: &Path,
    cancel: &Cancel,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    // Follow an existing symlink, preserving the link itself. A dangling
    // symlink is an error; never silently replace it with a regular file.
    let path = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path)?,
        Ok(_) => path.to_path_buf(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(error) => return Err(error),
    };
    let permissions = match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Some(meta.permissions()),
        Ok(_) => return Err(io::Error::other("target is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    // Atomic replacement must not bypass an existing file's read-only flag.
    if permissions.as_ref().is_some_and(fs::Permissions::readonly) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target is read-only",
        ));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp_path = parent.join(format!(".pi-rs-{}.tmp", crate::util::short_id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if permissions.is_some() {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    let temp = Temp(temp_path);
    check_cancel(cancel)?;
    write(&mut file)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    file.sync_all()?;
    check_cancel(cancel)?;
    drop(file);
    fs::rename(&temp.0, &path)?;
    Ok(())
}

fn check_cancel(cancel: &Cancel) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled."))
    } else {
        Ok(())
    }
}

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_writes_preserve_the_original_and_remove_the_temp() {
        let dir = crate::testutil::temp_dir("atomic-failure");
        let path = dir.join("file");
        fs::write(&path, "original").unwrap();
        let error = replace(&path, &Cancel::new(), |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("simulated disk full"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("disk full"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let absent = dir.join("new-file");
        assert!(replace(&absent, &Cancel::new(), |_| Err(io::Error::other(
            "disk full"
        )))
        .is_err());
        assert!(!absent.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn cancellation_before_commit_preserves_the_original() {
        let dir = crate::testutil::temp_dir("atomic-cancel");
        let path = dir.join("file");
        fs::write(&path, "original").unwrap();
        let cancel = Cancel::new();
        assert!(replace(&path, &cancel, |file| {
            file.write_all(b"replacement")?;
            cancel.cancel();
            Ok(())
        })
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_and_preserves_executable_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = crate::testutil::temp_dir("atomic-links");
        let target = dir.join("script");
        let link = dir.join("link");
        fs::write(&target, "old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751)).unwrap();
        symlink("script", &link).unwrap();
        write(&link, b"new", &Cancel::new()).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o751
        );
        let dangling = dir.join("dangling");
        symlink("missing", &dangling).unwrap();
        assert!(write(&dangling, b"new", &Cancel::new()).is_err());
        assert!(fs::symlink_metadata(&dangling)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(write(&target, b"bad", &Cancel::new()).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }
}
