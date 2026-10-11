// SPDX-License-Identifier: GPL-3.0-or-later

use std::{fs, io, io::Write, path::Path};
use uuid::Uuid;

/// Exercise the same filesystem access SQLite and encrypted credentials need,
/// without opening or changing any existing state files.
pub(super) fn prepare_data_directory(data: &Path) -> Result<(), String> {
    check_writable_directory(data).map_err(|error| {
        #[cfg(unix)]
        // SAFETY: These libc calls take no arguments and only read process identity.
        let identity = unsafe { format!(" (UID {}, GID {})", libc::geteuid(), libc::getegid()) };
        #[cfg(not(unix))]
        let identity = "";
        format!(
            "Cannot write to Sync data directory {}{identity}: {error}. \
             Grant this container user write access to the mounted host directory, \
             or set Compose user: \"UID:GID\" to a user with access. \
             Check that the /data mount is read/write and the filesystem has free space.",
            data.display()
        )
    })
}

fn check_writable_directory(data: &Path) -> io::Result<()> {
    fs::create_dir_all(data)?;
    let probe = data.join(format!(".write-check-{}", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&probe)?;
    let written = file
        .write_all(b"Sync storage check\n")
        .and_then(|()| file.sync_all());
    drop(file);
    // Try cleanup even when writing or syncing failed; preserve the original error.
    let removed = fs::remove_file(probe);
    written.and(removed)
}

#[cfg(test)]
mod tests {
    use super::prepare_data_directory;
    use std::fs;

    #[test]
    fn creates_missing_directory_and_preserves_existing_state() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("new/state");
        prepare_data_directory(&data).unwrap();
        assert_eq!(fs::read_dir(&data).unwrap().count(), 0);
        for name in ["sync.sqlite", "credentials.enc", "state.key"] {
            fs::write(data.join(name), name.as_bytes()).unwrap();
        }
        #[cfg(unix)]
        let original_permissions = {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&data, fs::Permissions::from_mode(0o750)).unwrap();
            fs::set_permissions(data.join("state.key"), fs::Permissions::from_mode(0o600)).unwrap();
            (
                fs::metadata(&data).unwrap().permissions().mode(),
                fs::metadata(data.join("state.key"))
                    .unwrap()
                    .permissions()
                    .mode(),
            )
        };
        prepare_data_directory(&data).unwrap();
        assert_eq!(fs::read_dir(&data).unwrap().count(), 3);
        for name in ["sync.sqlite", "credentials.enc", "state.key"] {
            assert_eq!(fs::read(data.join(name)).unwrap(), name.as_bytes());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&data).unwrap().permissions().mode(),
                original_permissions.0
            );
            assert_eq!(
                fs::metadata(data.join("state.key"))
                    .unwrap()
                    .permissions()
                    .mode(),
                original_permissions.1
            );
        }
    }

    #[test]
    fn reports_invalid_path_with_recovery_instructions() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("file");
        fs::write(&data, b"existing file").unwrap();
        let error = prepare_data_directory(&data).unwrap_err();
        assert!(error.contains(&data.display().to_string()));
        assert!(error.contains("user: \"UID:GID\""));
        assert!(error.contains("read/write"));
        #[cfg(unix)]
        {
            assert!(error.contains(&format!("UID {}", unsafe { libc::geteuid() })));
            assert!(error.contains(&format!("GID {}", unsafe { libc::getegid() })));
        }
        assert_eq!(fs::read(data).unwrap(), b"existing file");
    }
}
