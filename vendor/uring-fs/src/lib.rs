//! Deterministic file-cursor I/O runtime used by the SQLite VFS data path.
//!
//! Design goals:
//! - Safety: zero `unsafe`, zero background ownership tricks.
//! - Correctness: strict read/write cursor semantics on the provided file.
//! - Reliability: no panic-based control flow.
//! - Performance: minimal overhead (direct syscall-backed std I/O).

use std::fs::File;
use std::future::Future;
use std::io;
use std::io::{Read, Write};

/// File I/O runtime handle.
///
/// The current implementation performs direct blocking I/O and returns
/// immediately-ready futures so call sites can keep a future-based contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct IoUring;

impl IoUring {
    /// Construct a runtime handle.
    pub fn new() -> io::Result<Self> {
        Ok(Self)
    }

    /// Read up to `size` bytes from the file's current cursor position.
    ///
    /// The returned vector is truncated to the number of bytes actually read.
    pub fn read<'a>(
        &'a self,
        file: &'a File,
        size: u32,
    ) -> impl Future<Output = io::Result<Vec<u8>>> + 'a {
        async move {
            let mut out = vec![0_u8; size as usize];
            let bytes = read_retry_interrupt(file, &mut out)?;
            out.truncate(bytes);
            Ok(out)
        }
    }

    /// Write the full buffer to the file's current cursor position.
    pub fn write<'a>(
        &'a self,
        file: &'a File,
        buffer: Vec<u8>,
    ) -> impl Future<Output = io::Result<()>> + 'a {
        async move {
            let mut written = 0_usize;

            while written < buffer.len() {
                let advanced = write_retry_interrupt(file, &buffer[written..])?;
                if advanced == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write returned 0 before completing the payload",
                    ));
                }
                written += advanced;
            }

            Ok(())
        }
    }
}

fn read_retry_interrupt(file: &File, buf: &mut [u8]) -> io::Result<usize> {
    let mut handle = file;
    loop {
        match handle.read(buf) {
            Ok(n) => return Ok(n),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

fn write_retry_interrupt(file: &File, buf: &[u8]) -> io::Result<usize> {
    let mut handle = file;
    loop {
        match handle.write(buf) {
            Ok(n) => return Ok(n),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Seek;
    use std::io::SeekFrom;

    use super::IoUring;

    fn open_rw(path: &std::path::Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .expect("open rw file")
    }

    #[test]
    fn read_advances_file_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cursor.bin");
        std::fs::write(&path, b"abcdef").expect("seed file");

        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open ro");

        let mut seek_ref = &file;
        seek_ref.seek(SeekFrom::Start(0)).expect("seek start");

        let runtime = IoUring::new().expect("runtime");
        let first = pollster::block_on(runtime.read(&file, 3)).expect("first read");
        let second = pollster::block_on(runtime.read(&file, 3)).expect("second read");

        assert_eq!(first, b"abc");
        assert_eq!(second, b"def");
    }

    #[test]
    fn write_persists_full_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("write.bin");
        let file = open_rw(&path);

        let runtime = IoUring::new().expect("runtime");
        pollster::block_on(runtime.write(&file, b"hello world".to_vec())).expect("write");

        let data = std::fs::read(&path).expect("read file");
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn read_after_write_from_same_descriptor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rw.bin");
        let file = open_rw(&path);

        let runtime = IoUring::new().expect("runtime");
        pollster::block_on(runtime.write(&file, b"xyz123".to_vec())).expect("write");

        let mut seek_ref = &file;
        seek_ref.seek(SeekFrom::Start(0)).expect("seek back");

        let data = pollster::block_on(runtime.read(&file, 6)).expect("read");
        assert_eq!(data, b"xyz123");
    }
}
