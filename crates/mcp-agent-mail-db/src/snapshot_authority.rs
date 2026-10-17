//! Stable per-primary admission for snapshot metadata and verified selection.
//!
//! This file is never unlinked on release. Its inode is the cooperating
//! processes' authority; a replaced or aliased lock is an error, not stale
//! evidence to delete. Admission is nonblocking, including within one process.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use fs2::FileExt as _;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) struct SnapshotAuthority {
    file: File,
    parent: File,
    parent_path: PathBuf,
    name: OsString,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn open_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(invalid("snapshot authority requires an absolute parent"));
    }
    let mut directory = File::from(rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty())?);
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                directory = File::from(rustix::fs::openat(
                    &directory,
                    name,
                    DIRECTORY_FLAGS,
                    Mode::empty(),
                )?);
            }
            _ => return Err(invalid("snapshot authority refuses parent traversal")),
        }
    }
    Ok(directory)
}

impl SnapshotAuthority {
    pub(super) fn acquire(primary: &Path, exclusive: bool) -> io::Result<Self> {
        let parent_path = primary
            .parent()
            .ok_or_else(|| invalid("snapshot primary has no parent"))?
            .to_path_buf();
        let mut name = primary
            .file_name()
            .ok_or_else(|| invalid("snapshot primary has no leaf"))?
            .to_os_string();
        name.push(".bak.authority.lock");
        let parent = open_directory(&parent_path)?;
        // Readers of an existing snapshot need no write permission. A legacy
        // snapshot first acquires its persistent lock via create-only admission.
        let access = if exclusive {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let read_flags = access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let (file, created) = match rustix::fs::openat(&parent, &name, read_flags, Mode::empty()) {
            Ok(file) => (File::from(file), false),
            Err(error) if error == rustix::io::Errno::NOENT => {
                match rustix::fs::openat(
                    &parent,
                    &name,
                    OFlags::RDWR
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::NONBLOCK
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                ) {
                    Ok(file) => (File::from(file), true),
                    Err(error) if error == rustix::io::Errno::EXIST => (
                        File::from(rustix::fs::openat(
                            &parent,
                            &name,
                            read_flags,
                            Mode::empty(),
                        )?),
                        false,
                    ),
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        let guard = Self {
            file,
            parent,
            parent_path,
            name,
        };
        guard.revalidate()?;
        if exclusive {
            guard.file.try_lock_exclusive()?;
        } else {
            guard.file.try_lock_shared()?;
        }
        guard.revalidate()?;
        if created {
            guard.file.sync_all()?;
            guard.parent.sync_all()?;
        }
        Ok(guard)
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        let retained = self.file.metadata()?;
        if !retained.is_file()
            || retained.nlink() != 1
            || retained.len() != 0
            || retained.mode() & 0o777 != 0o600
        {
            return Err(invalid(
                "snapshot authority must be an empty private single-link regular file",
            ));
        }
        let entry = rustix::fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(entry.st_mode) != FileType::RegularFile
            || entry.st_dev != retained.dev()
            || entry.st_ino != retained.ino()
        {
            return Err(invalid("snapshot authority lock identity changed"));
        }
        let current_parent = open_directory(&self.parent_path)?.metadata()?;
        let retained_parent = self.parent.metadata()?;
        if current_parent.dev() != retained_parent.dev()
            || current_parent.ino() != retained_parent.ino()
        {
            return Err(invalid("snapshot authority parent identity changed"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_readers_coexist_and_exclude_publishers_without_unlinking() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().canonicalize().unwrap().join("mail.sqlite3");
        let first = SnapshotAuthority::acquire(&primary, false).unwrap();
        let inode = first.file.metadata().unwrap().ino();
        let second = SnapshotAuthority::acquire(&primary, false).unwrap();
        assert_eq!(
            SnapshotAuthority::acquire(&primary, true)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        assert!(SnapshotAuthority::acquire(&primary, true).is_err());
        drop(second);
        let writer = SnapshotAuthority::acquire(&primary, true).unwrap();
        assert_eq!(writer.file.metadata().unwrap().ino(), inode);
        assert!(SnapshotAuthority::acquire(&primary, false).is_err());
        assert!(SnapshotAuthority::acquire(&primary, true).is_err());
        let other = primary.with_file_name("other.sqlite3");
        let independent = SnapshotAuthority::acquire(&other, true).unwrap();
        independent.revalidate().unwrap();
        drop(writer);
        assert_eq!(
            SnapshotAuthority::acquire(&primary, true)
                .unwrap()
                .file
                .metadata()
                .unwrap()
                .ino(),
            inode
        );
    }

    #[test]
    fn unsafe_lock_shapes_are_preserved_and_refused() {
        for shape in ["symlink", "hardlink", "directory", "bytes", "public"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let primary = root.join("mail.sqlite3");
            let lock = root.join("mail.sqlite3.bak.authority.lock");
            let sentinel = root.join("sentinel");
            std::fs::write(&sentinel, b"irreplaceable evidence").unwrap();
            match shape {
                "symlink" => std::os::unix::fs::symlink(&sentinel, &lock).unwrap(),
                "directory" => std::fs::create_dir(&lock).unwrap(),
                "hardlink" => std::fs::hard_link(&sentinel, &lock).unwrap(),
                _ => {
                    std::fs::write(
                        &lock,
                        if shape == "bytes" {
                            b"occupied".as_slice()
                        } else {
                            b""
                        },
                    )
                    .unwrap();
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        &lock,
                        std::fs::Permissions::from_mode(if shape == "public" {
                            0o644
                        } else {
                            0o600
                        }),
                    )
                    .unwrap();
                }
            }
            assert!(
                SnapshotAuthority::acquire(&primary, true).is_err(),
                "{shape}"
            );
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"irreplaceable evidence");
            assert!(std::fs::symlink_metadata(&lock).is_ok());
        }
    }

    #[test]
    fn namespace_replacement_is_not_permission_to_reacquire_or_delete() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let directory = root.join("snapshots");
        std::fs::create_dir(&directory).unwrap();
        let primary = directory.join("mail.sqlite3");
        let guard = SnapshotAuthority::acquire(&primary, true).unwrap();
        let original = directory.join("mail.sqlite3.bak.authority.lock");
        let displaced = root.join("retained-lock");
        std::fs::rename(&original, &displaced).unwrap();
        assert!(guard.revalidate().is_err());
        std::fs::rename(&displaced, &original).unwrap();
        guard.revalidate().unwrap();
        std::fs::rename(&directory, root.join("retained-directory")).unwrap();
        std::fs::create_dir(&directory).unwrap();
        assert!(guard.revalidate().is_err());
        drop(guard);
        assert!(
            root.join("retained-directory/mail.sqlite3.bak.authority.lock")
                .exists()
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    }
}
