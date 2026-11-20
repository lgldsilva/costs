//! Atomic report exports and the private per-user dashboard destination.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Resolve a per-user path without shell expansion or a shared temporary directory.
pub fn default_report_path() -> io::Result<PathBuf> {
    let project = directories::ProjectDirs::from("com", "lgldsilva", "costs")
        .ok_or_else(|| io::Error::other("cannot resolve per-user report directory"))?;
    let directory = project.data_local_dir().join("reports");
    ensure_private_directory(&directory)?;
    Ok(directory.join("dashboard.html"))
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(io::Error::other("report directory must be a real directory"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    return Err(io::Error::other("report directory must be private (0700)"));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => ensure_private_directory(path),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Write a new sibling, then rename it over the destination; never truncate a link target.
/// Explicit outputs retain regular-file replacement behavior and require an existing parent.
pub fn write_report(path: &Path, contents: &[u8]) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(io::Error::other("report output must be a regular file, not a symlink"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (temporary, mut file) = loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".costs-report-{}-{sequence}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        // A concurrently introduced leaf symlink is replaced by rename, never followed.
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_regular_report_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.html");
        std::fs::write(&path, "old").unwrap();
        write_report(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_without_modifying_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("report.html");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_report(&link, b"new").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        assert!(std::fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_directory_and_rejects_directory_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("reports");
        ensure_private_directory(&directory).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let link = root.path().join("linked");
        std::os::unix::fs::symlink(&directory, &link).unwrap();
        assert!(ensure_private_directory(&link).is_err());
    }
}
