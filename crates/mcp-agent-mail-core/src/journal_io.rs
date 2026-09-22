//! Handle-bound access to the private degraded-intent journal directory.
//!
//! The configured storage root is trusted and may itself be an operator-selected
//! symlink. Below that root, directory and file opens are relative to retained
//! handles and refuse symlinks. Writable files must be regular and have exactly
//! one link before permissions or contents change. Unix opens are nonblocking so
//! a substituted FIFO cannot wedge the fallback before its lock deadline.
//!
//! Binding checks detect observed directory/lock/log replacement; they do not
//! make a pathname immutable against a hostile process with the same filesystem
//! authority. Callers must serialize appenders, sync the file, sync the directory,
//! and recheck bindings before issuing a durable receipt. Nothing is deleted.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Access to one journal entry. No mode truncates existing contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalFileMode {
    Read,
    Lock,
    Append,
}

/// Retains both the selected root and its no-follow journal subdirectory.
#[derive(Debug)]
pub struct JournalDirectory {
    root_path: PathBuf,
    directory_name: String,
    root: File,
    directory: File,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Accept only a single portable component, never traversal, an alternate data
/// stream, or a Windows device name. Journal names are application constants.
pub fn validate_entry_name(name: &str) -> io::Result<()> {
    let mut components = Path::new(name).components();
    let normal =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if !normal || name.contains(['/', '\\', ':', '\0']) || name.ends_with(['.', ' ']) || device {
        return Err(invalid(
            "journal entry must be a non-device single path component",
        ));
    }
    Ok(())
}

impl JournalDirectory {
    /// Open the journal directory under a trusted configured root. Read-only
    /// callers pass `create = false`; they never create or chmod any entry.
    pub fn open(root_path: &Path, directory_name: &str, create: bool) -> io::Result<Self> {
        validate_entry_name(directory_name)?;
        let root_path = std::path::absolute(root_path)?;
        if create {
            std::fs::create_dir_all(&root_path)?;
        }
        let root = open_root(&root_path)?;
        let directory = open_directory_at(&root, directory_name, create)?;
        let journal = Self {
            root_path,
            directory_name: directory_name.to_string(),
            root,
            directory,
        };
        journal.validate_binding()?;
        #[cfg(unix)]
        if create {
            use std::os::unix::fs::PermissionsExt as _;
            journal
                .directory
                .set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(journal)
    }

    /// Open one entry without following the leaf or re-resolving its parent.
    /// Existing contents are preserved. Rejected aliases are never chmodded.
    pub fn open_file(&self, name: &str, mode: JournalFileMode) -> io::Result<File> {
        validate_entry_name(name)?;
        self.validate_binding()?;
        let file = match open_file_at(&self.directory, name, mode) {
            Ok(file) => file,
            Err(error) => {
                // A missing leaf in a displaced directory is not an empty
                // current journal. Preserve NotFound only for a current parent.
                self.validate_binding()?;
                return Err(error);
            }
        };
        check_regular_single_link(&file)?;
        self.validate_file(name, &file)?;
        if mode != JournalFileMode::Read {
            crate::disk::set_private_writable_file_permissions(&file)?;
        }
        Ok(file)
    }

    /// Check that a retained handle is still the unique named journal file.
    /// A changed lock name means its advisory lock no longer serializes peers.
    pub fn validate_file(&self, name: &str, file: &File) -> io::Result<()> {
        validate_entry_name(name)?;
        self.validate_binding()?;
        check_regular_single_link(file)?;
        let named =
            open_file_at(&self.directory, name, JournalFileMode::Read).map_err(binding_error)?;
        check_regular_single_link(&named)?;
        require_same_file(file, &named)
    }

    /// Sync the retained Unix directory handles, not a freshly resolved path.
    /// Windows retains its existing file-flush durability contract; directory
    /// flushing is not emulated by claiming that a pathname reopen is a sync.
    pub fn sync(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.directory.sync_all()?;
            self.root.sync_all()?;
        }
        self.validate_binding()
    }

    /// The absolute spelling frozen when this directory was opened.
    #[must_use]
    pub fn directory_path(&self) -> PathBuf {
        self.root_path.join(&self.directory_name)
    }

    fn validate_binding(&self) -> io::Result<()> {
        let root = open_root(&self.root_path).map_err(binding_error)?;
        require_same_file(&self.root, &root)?;
        let directory =
            open_directory_at(&self.root, &self.directory_name, false).map_err(binding_error)?;
        require_same_file(&self.directory, &directory)
    }
}

fn binding_error(error: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("journal authority could not be revalidated: {error}"),
    )
}

fn require_same_file(left: &File, right: &File) -> io::Result<()> {
    let left = same_file::Handle::from_file(left.try_clone()?)?;
    let right = same_file::Handle::from_file(right.try_clone()?)?;
    if left == right {
        Ok(())
    } else {
        Err(invalid(
            "journal directory or entry was replaced; no receipt may be issued",
        ))
    }
}

fn check_regular_single_link(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("journal entry is not a regular file"));
    }
    #[cfg(unix)]
    let links = {
        use std::os::unix::fs::MetadataExt as _;
        metadata.nlink()
    };
    #[cfg(windows)]
    let links = {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(invalid("journal entry is a reparse point"));
        }
        crate::disk::windows_file_link_count(file)?
    };
    #[cfg(not(any(unix, windows)))]
    let links = 0;
    if links != 1 {
        return Err(invalid(
            "journal entry does not have exactly one filesystem link",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_root(path: &Path) -> io::Result<File> {
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::Mode;
    open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &str, create: bool) -> io::Result<File> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::{Mode, mkdirat};
    if create {
        match mkdirat(parent, name, Mode::S_IRWXU) {
            Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
            Err(error) => return Err(error.into()),
        }
    }
    openat(
        parent,
        name,
        OFlag::O_RDONLY
            | OFlag::O_DIRECTORY
            | OFlag::O_CLOEXEC
            | OFlag::O_NOFOLLOW
            | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
}

#[cfg(unix)]
fn open_file_at(parent: &File, name: &str, mode: JournalFileMode) -> io::Result<File> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    let access = match mode {
        JournalFileMode::Read => OFlag::O_RDONLY,
        JournalFileMode::Lock => OFlag::O_RDWR | OFlag::O_CREAT,
        JournalFileMode::Append => OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_APPEND,
    };
    openat(
        parent,
        name,
        access | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .map(File::from)
    .map_err(io::Error::from)
}

#[cfg(windows)]
fn open_root(path: &Path) -> io::Result<File> {
    cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
        .map(cap_std::fs::Dir::into_std_file)
}

#[cfg(windows)]
fn open_directory_at(parent: &File, name: &str, create: bool) -> io::Result<File> {
    use cap_fs_ext::DirExt as _;
    use std::os::windows::fs::MetadataExt as _;
    let parent = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    if create {
        match parent.create_dir(name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let directory = parent.open_dir_nofollow(name)?.into_std_file();
    if directory.metadata()?.file_attributes() & 0x0000_0400 != 0 {
        return Err(invalid("journal directory is a reparse point"));
    }
    Ok(directory)
}

#[cfg(windows)]
fn open_file_at(parent: &File, name: &str, mode: JournalFileMode) -> io::Result<File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    let parent = cap_std::fs::Dir::from_std_file(parent.try_clone()?);
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    match mode {
        JournalFileMode::Read => {}
        JournalFileMode::Lock => {
            options.write(true).create(true);
        }
        JournalFileMode::Append => {
            options.append(true).create(true);
        }
    }
    parent
        .open_with(name, &options)
        .map(cap_std::fs::File::into_std)
}

#[cfg(not(any(unix, windows)))]
fn open_root(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-bound journals are unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn open_directory_at(_parent: &File, _name: &str, _create: bool) -> io::Result<File> {
    open_root(Path::new(""))
}

#[cfg(not(any(unix, windows)))]
fn open_file_at(_parent: &File, _name: &str, _mode: JournalFileMode) -> io::Result<File> {
    open_root(Path::new(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    const DIRECTORY: &str = "degraded_intents";

    #[test]
    fn read_only_open_does_not_create_missing_authority() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            JournalDirectory::open(root.path(), DIRECTORY, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn real_append_and_read_preserve_existing_bytes_and_names() {
        let root = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        for bytes in [b"first\n".as_slice(), b"second\n".as_slice()] {
            let mut file = journal
                .open_file("ack.jsonl", JournalFileMode::Append)
                .unwrap();
            file.write_all(bytes).unwrap();
            file.sync_all().unwrap();
            journal.sync().unwrap();
            journal.validate_file("ack.jsonl", &file).unwrap();
        }
        let mut file = journal
            .open_file("ack.jsonl", JournalFileMode::Read)
            .unwrap();
        let mut actual = String::new();
        file.read_to_string(&mut actual).unwrap();
        assert_eq!(actual, "first\nsecond\n");
    }

    #[test]
    fn names_cannot_escape_or_select_alternate_streams_or_devices() {
        for name in [
            "",
            ".",
            "..",
            "../outside",
            "/outside",
            "a/b",
            "a\\b",
            "a:stream",
            "NUL",
            "con.txt",
            "COM1",
            "LPT9.txt",
            "trailing.",
            "trailing ",
            "a\0b",
        ] {
            assert!(validate_entry_name(name).is_err(), "{name:?}");
        }
        for name in [
            DIRECTORY,
            "acknowledge_message.jsonl",
            ".acknowledge_message.jsonl.lock",
            "unicode-猫.jsonl",
        ] {
            validate_entry_name(name).unwrap();
        }
    }

    #[test]
    fn hard_link_alias_is_rejected_without_changing_source_bytes() {
        let root = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        let outside = root.path().join("source");
        std::fs::write(&outside, b"must remain unchanged").unwrap();
        std::fs::hard_link(&outside, root.path().join(DIRECTORY).join("alias")).unwrap();
        for mode in [
            JournalFileMode::Read,
            JournalFileMode::Lock,
            JournalFileMode::Append,
        ] {
            assert!(journal.open_file("alias", mode).is_err());
        }
        assert_eq!(std::fs::read(&outside).unwrap(), b"must remain unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_rejection_precedes_permission_changes() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        let source = root.path().join("source");
        std::fs::write(&source, b"evidence").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::fs::hard_link(&source, root.path().join(DIRECTORY).join("alias")).unwrap();
        assert!(journal.open_file("alias", JournalFileMode::Append).is_err());
        assert_eq!(
            std::fs::metadata(source).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_leaf_and_directory_are_refused_without_touching_targets() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        let source = outside.path().join("source");
        std::fs::write(&source, b"original").unwrap();
        symlink(&source, root.path().join(DIRECTORY).join("alias")).unwrap();
        for mode in [
            JournalFileMode::Read,
            JournalFileMode::Lock,
            JournalFileMode::Append,
        ] {
            assert!(journal.open_file("alias", mode).is_err());
        }
        std::fs::rename(root.path().join(DIRECTORY), root.path().join("preserved")).unwrap();
        symlink(outside.path(), root.path().join(DIRECTORY)).unwrap();
        assert!(JournalDirectory::open(root.path(), DIRECTORY, true).is_err());
        assert!(
            journal
                .open_file("new-entry", JournalFileMode::Append)
                .is_err()
        );
        assert_eq!(std::fs::read(source).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_and_directory_entries_are_not_read_or_chmodded() {
        let root = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        let fifo = root.path().join(DIRECTORY).join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        std::fs::create_dir(root.path().join(DIRECTORY).join("directory")).unwrap();
        let started = std::time::Instant::now();
        for name in ["fifo", "directory"] {
            for mode in [
                JournalFileMode::Read,
                JournalFileMode::Lock,
                JournalFileMode::Append,
            ] {
                assert!(journal.open_file(name, mode).is_err());
            }
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn replaced_lock_or_log_cannot_validate_a_durable_receipt() {
        let root = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        for name in ["ack.jsonl", ".ack.lock"] {
            let file = journal.open_file(name, JournalFileMode::Append).unwrap();
            let path = root.path().join(DIRECTORY).join(name);
            std::fs::rename(&path, path.with_extension("preserved")).unwrap();
            std::fs::write(&path, b"replacement").unwrap();
            assert!(journal.validate_file(name, &file).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn retained_directory_never_redirects_an_open_to_its_replacement() {
        let root = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        std::fs::rename(root.path().join(DIRECTORY), root.path().join("preserved")).unwrap();
        std::fs::create_dir(root.path().join(DIRECTORY)).unwrap();
        assert!(
            journal
                .open_file("ack.jsonl", JournalFileMode::Append)
                .is_err()
        );
        assert!(journal.sync().is_err());
        assert_eq!(
            std::fs::read_dir(root.path().join(DIRECTORY))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("preserved"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_added_hard_link_invalidates_a_previously_open_file() {
        let root = tempfile::tempdir().unwrap();
        let journal = JournalDirectory::open(root.path(), DIRECTORY, true).unwrap();
        let file = journal
            .open_file("ack.jsonl", JournalFileMode::Append)
            .unwrap();
        std::fs::hard_link(
            root.path().join(DIRECTORY).join("ack.jsonl"),
            root.path().join("alias"),
        )
        .unwrap();
        assert!(journal.validate_file("ack.jsonl", &file).is_err());
    }
}
