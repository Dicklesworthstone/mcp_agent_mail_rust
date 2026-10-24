//! Proof that "never used" integrity findings are never-written pages.
//!
//! `PRAGMA integrity_check` reports a page that no b-tree and no freelist
//! reaches as `Page N: never used`. Two very different defects share that row:
//!
//! - **Stranded content** (GH#247): a b-tree lost its root, so its pages are
//!   unreachable but still hold rows. That is data loss.
//! - **Never-written pages**: the file was extended by a block of pages that
//!   nothing ever wrote to or linked in (GH#291). Every table and index is
//!   intact; the pages are leaked space that a `VACUUM`-class rewrite drops.
//!
//! The row text cannot tell them apart, and [`crate::integrity`] therefore
//! caps the benign reading at [`crate::integrity::BENIGN_UNUSED_PAGE_ROW_LIMIT`]
//! rows. This module replaces "trust the message" with evidence: a reported
//! page is accepted only when its every stored image is zero-filled, in the
//! main database file and in every WAL frame that names it. A page that ever
//! held a b-tree node starts with a non-zero page-type byte, so stranded
//! content cannot pass.
//!
//! The reader opens files with ordinary descriptors, so it must only be
//! pointed at a **private copy** of a database family. Closing a descriptor on
//! a live database file drops the process's POSIX locks on it.

use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Magic string at the start of every SQLite main database file.
const SQLITE_HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// Size of the SQLite main database file header.
const SQLITE_HEADER_LEN: usize = 100;
/// Size of the WAL file header.
const WAL_HEADER_LEN: usize = 32;
/// Size of the header that precedes each page image in a WAL file.
const WAL_FRAME_HEADER_LEN: usize = 24;
/// WAL magic numbers (big-endian checksums / little-endian checksums).
const WAL_MAGIC: [u32; 2] = [0x377f_0682, 0x377f_0683];

/// Which stored image of a page held non-zero bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageImageSource {
    /// The page's slot in the main database file.
    MainFile,
    /// A frame of the write-ahead log that names the page.
    WalFrame,
}

impl fmt::Display for PageImageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MainFile => "main database file",
            Self::WalFrame => "WAL frame",
        })
    }
}

/// Why a set of "never used" findings could not be proven never-written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeverWrittenProofRefusal {
    /// The caller passed no pages.
    NoPages,
    /// The main file is not a readable SQLite database file.
    UnreadableMainFile(String),
    /// A reported page does not lie wholly inside the main file.
    PageOutsideMainFile { page: u32 },
    /// A stored image of a reported page holds non-zero bytes.
    PageHasContent { page: u32, source: PageImageSource },
    /// A WAL file exists but cannot be read as one.
    UnreadableWal(String),
}

impl fmt::Display for NeverWrittenProofRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPages => f.write_str("no never-used pages were reported"),
            Self::UnreadableMainFile(reason) => {
                write!(f, "the main database file could not be read: {reason}")
            }
            Self::PageOutsideMainFile { page } => {
                write!(
                    f,
                    "page {page} does not lie wholly inside the main database file"
                )
            }
            Self::PageHasContent { page, source } => {
                write!(f, "page {page} holds non-zero bytes in the {source}")
            }
            Self::UnreadableWal(reason) => write!(f, "the WAL file could not be read: {reason}"),
        }
    }
}

/// Pages reported as never used and proven never written.
///
/// Every page listed here is zero-filled in the main file and in every WAL
/// frame that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeverWrittenOrphanPages {
    /// Ascending, without duplicates.
    pages: Vec<u32>,
    page_size: u32,
}

impl NeverWrittenOrphanPages {
    /// Number of proven pages.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Page size of the inspected database, in bytes.
    #[must_use]
    pub const fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Space the proven pages occupy, in bytes.
    #[must_use]
    pub fn leaked_bytes(&self) -> u64 {
        u64::try_from(self.pages.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.page_size))
    }

    /// Whether every page of `pages` is in the proven set.
    #[must_use]
    pub fn contains_all(&self, pages: &[u32]) -> bool {
        pages
            .iter()
            .all(|page| self.pages.binary_search(page).is_ok())
    }

    /// The proven pages as inclusive runs of consecutive page numbers.
    #[must_use]
    pub fn runs(&self) -> Vec<(u32, u32)> {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &page in &self.pages {
            match runs.last_mut() {
                Some((_, last)) if last.checked_add(1) == Some(page) => *last = page,
                _ => runs.push((page, page)),
            }
        }
        runs
    }

    /// Compact operator rendering: count, size and the first few page runs.
    #[must_use]
    pub fn describe(&self) -> String {
        /// Runs shown before eliding. A leak is normally one contiguous block.
        const SHOWN_RUNS: usize = 4;
        let runs = self.runs();
        let mut shown: Vec<String> = runs
            .iter()
            .take(SHOWN_RUNS)
            .map(|&(first, last)| {
                if first == last {
                    first.to_string()
                } else {
                    format!("{first}-{last}")
                }
            })
            .collect();
        if runs.len() > SHOWN_RUNS {
            shown.push(format!("+{} more run(s)", runs.len() - SHOWN_RUNS));
        }
        format!(
            "{} never-written orphan page(s), {} bytes, page(s) {}",
            self.pages.len(),
            self.leaked_bytes(),
            shown.join(", ")
        )
    }
}

/// The page number of one "never used" integrity-check row.
///
/// Understands canonical SQLite's `Page N: never used` (and its older
/// `Page N is never used`) and FrankenSQLite's `page N is never used`, with or
/// without the `database disk image is malformed: ` prefix the primary engine
/// puts in front of its complaint. Anything else, including any trailing
/// text, is not a never-used row.
#[must_use]
pub fn never_used_page_number(detail: &str) -> Option<u32> {
    let lower = detail.trim().to_ascii_lowercase();
    let row = lower
        .strip_prefix("database disk image is malformed: ")
        .unwrap_or(&lower);
    let rest = row.strip_prefix("page ")?;
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, tail) = rest.split_at(digits_len);
    if tail != ": never used" && tail != " is never used" {
        return None;
    }
    digits.parse::<u32>().ok().filter(|page| *page >= 1)
}

/// The reported pages when EVERY verdict-carrying row is a never-used row.
///
/// Section headers (`*** in database main ***`) and blank rows carry no
/// verdict and are skipped. Any other row, including a lone `ok`, yields
/// `None`: this is the page set of a result that complains about unused pages
/// and about nothing else. Returned ascending and without duplicates.
#[must_use]
pub fn never_used_page_numbers(details: &[String]) -> Option<Vec<u32>> {
    let mut pages = Vec::new();
    for detail in details {
        let trimmed = detail.trim();
        if trimmed.is_empty() || (trimmed.starts_with("***") && trimmed.ends_with("***")) {
            continue;
        }
        pages.push(never_used_page_number(trimmed)?);
    }
    if pages.is_empty() {
        return None;
    }
    pages.sort_unstable();
    pages.dedup();
    Some(pages)
}

/// The WAL that belongs to the main database file at `main_path`.
fn wal_path_for(main_path: &Path) -> PathBuf {
    let mut name = main_path.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

/// Read the page size from a SQLite main database file header.
fn read_page_size(main: &mut File) -> Result<u32, NeverWrittenProofRefusal> {
    let mut header = [0_u8; SQLITE_HEADER_LEN];
    main.rewind()
        .and_then(|()| main.read_exact(&mut header))
        .map_err(|error| NeverWrittenProofRefusal::UnreadableMainFile(error.to_string()))?;
    if &header[..16] != SQLITE_HEADER_MAGIC {
        return Err(NeverWrittenProofRefusal::UnreadableMainFile(
            "missing SQLite header magic".to_string(),
        ));
    }
    // A stored value of 1 means 65536; every other valid size is a power of
    // two between 512 and 32768.
    let page_size = match u16::from_be_bytes([header[16], header[17]]) {
        1 => 65_536_u32,
        raw => u32::from(raw),
    };
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(NeverWrittenProofRefusal::UnreadableMainFile(format!(
            "invalid page size {page_size}"
        )));
    }
    Ok(page_size)
}

/// Require every reported page to be zero-filled in the main file.
fn require_zero_in_main_file(
    main: &mut File,
    page_size: u32,
    pages: &[u32],
) -> Result<(), NeverWrittenProofRefusal> {
    let unreadable =
        |error: std::io::Error| NeverWrittenProofRefusal::UnreadableMainFile(error.to_string());
    let file_len = main.metadata().map_err(unreadable)?.len();
    let mut image = vec![0_u8; page_size as usize];
    for &page in pages {
        let offset = u64::from(page - 1) * u64::from(page_size);
        if offset
            .checked_add(u64::from(page_size))
            .is_none_or(|end| end > file_len)
        {
            return Err(NeverWrittenProofRefusal::PageOutsideMainFile { page });
        }
        main.seek(SeekFrom::Start(offset))
            .and_then(|_| main.read_exact(&mut image))
            .map_err(unreadable)?;
        if image.iter().any(|byte| *byte != 0) {
            return Err(NeverWrittenProofRefusal::PageHasContent {
                page,
                source: PageImageSource::MainFile,
            });
        }
    }
    Ok(())
}

/// Require every WAL frame that names a reported page to be zero-filled.
///
/// Frame validity (salts, checksums, commit boundaries) is deliberately not
/// evaluated: a frame the engine would ignore is still required to be zero,
/// so the proof holds for whichever frames a reader ends up honouring.
fn require_zero_in_wal(
    wal_path: &Path,
    page_size: u32,
    pages: &[u32],
) -> Result<(), NeverWrittenProofRefusal> {
    let unreadable =
        |error: std::io::Error| NeverWrittenProofRefusal::UnreadableWal(error.to_string());
    let wal = match File::open(wal_path) {
        Ok(wal) => wal,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(unreadable(error)),
    };
    let wal_len = wal.metadata().map_err(unreadable)?.len();
    if wal_len <= WAL_HEADER_LEN as u64 {
        // No frame can exist: the smallest frame is a header plus one page.
        return Ok(());
    }
    let mut reader = BufReader::new(wal);
    let mut header = [0_u8; WAL_HEADER_LEN];
    reader.read_exact(&mut header).map_err(unreadable)?;
    let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if !WAL_MAGIC.contains(&magic) {
        return Err(NeverWrittenProofRefusal::UnreadableWal(
            "missing WAL header magic".to_string(),
        ));
    }
    let wal_page_size = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    if wal_page_size != page_size {
        return Err(NeverWrittenProofRefusal::UnreadableWal(format!(
            "WAL page size {wal_page_size} differs from the database page size {page_size}"
        )));
    }
    let frame_len = WAL_FRAME_HEADER_LEN as u64 + u64::from(page_size);
    let complete_frames = (wal_len - WAL_HEADER_LEN as u64) / frame_len;
    let mut frame_header = [0_u8; WAL_FRAME_HEADER_LEN];
    let mut image = vec![0_u8; page_size as usize];
    for _ in 0..complete_frames {
        reader.read_exact(&mut frame_header).map_err(unreadable)?;
        let page = u32::from_be_bytes([
            frame_header[0],
            frame_header[1],
            frame_header[2],
            frame_header[3],
        ]);
        if pages.binary_search(&page).is_ok() {
            reader.read_exact(&mut image).map_err(unreadable)?;
            if image.iter().any(|byte| *byte != 0) {
                return Err(NeverWrittenProofRefusal::PageHasContent {
                    page,
                    source: PageImageSource::WalFrame,
                });
            }
        } else {
            reader
                .seek_relative(i64::from(page_size))
                .map_err(unreadable)?;
        }
    }
    Ok(())
}

/// Prove that every page of `pages` was never written.
///
/// `main_path` must name the main file of a **private copy** of a database
/// family (see the module documentation); its WAL, when present, is the
/// sibling `<main_path>-wal`. `pages` are the page numbers an integrity check
/// of that same copy reported as never used.
///
/// The proof fails closed. It is refused when a page lies outside the main
/// file, when any stored image of a page holds a non-zero byte, and when the
/// main file or an existing WAL cannot be read as what it should be.
pub fn prove_pages_never_written(
    main_path: &Path,
    pages: &[u32],
) -> Result<NeverWrittenOrphanPages, NeverWrittenProofRefusal> {
    if pages.is_empty() {
        return Err(NeverWrittenProofRefusal::NoPages);
    }
    let mut pages = pages.to_vec();
    pages.sort_unstable();
    pages.dedup();
    if pages[0] == 0 {
        return Err(NeverWrittenProofRefusal::PageOutsideMainFile { page: 0 });
    }
    let mut main = File::open(main_path)
        .map_err(|error| NeverWrittenProofRefusal::UnreadableMainFile(error.to_string()))?;
    let page_size = read_page_size(&mut main)?;
    require_zero_in_main_file(&mut main, page_size, &pages)?;
    require_zero_in_wal(&wal_path_for(main_path), page_size, &pages)?;
    Ok(NeverWrittenOrphanPages { pages, page_size })
}

/// Real-file fixtures shared by this module's tests and the pool's.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::io::{Seek as _, SeekFrom, Write as _};
    use std::path::Path;

    /// Page size every fixture uses.
    pub const PAGE: u32 = 4096;

    pub fn populate(conn: &crate::CanonicalDbConn, table: &str, rows: u32) {
        conn.execute_raw(&format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, payload BLOB)"
        ))
        .expect("create table");
        conn.execute_raw(&format!(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < {rows}) \
             INSERT INTO {table} (payload) SELECT randomblob(1024) FROM seq"
        ))
        .expect("populate table");
    }

    /// A rollback-journal database with one populated table.
    pub fn seed_db(path: &Path) {
        let conn = crate::CanonicalDbConn::open_file(path.display().to_string())
            .expect("open canonical db");
        conn.execute_raw(&format!("PRAGMA page_size = {PAGE}"))
            .expect("page size");
        conn.execute_raw("PRAGMA journal_mode = DELETE")
            .expect("journal mode");
        populate(&conn, "kept", 64);
    }

    /// Page size recorded in the file's header.
    pub fn page_size(path: &Path) -> u32 {
        let header = std::fs::read(path).expect("read fixture");
        match u16::from_be_bytes([header[16], header[17]]) {
            1 => 65_536,
            raw => u32::from(raw),
        }
    }

    pub fn page_count(path: &Path) -> u32 {
        let size = u64::from(page_size(path));
        u32::try_from(std::fs::metadata(path).expect("metadata").len() / size)
            .expect("page count fits")
    }

    /// Extend the file by `extra` zero-filled pages nothing references.
    ///
    /// This is the GH#291 shape: the file grows by a block of pages that no
    /// b-tree and no freelist ever links in. The in-header page count is
    /// raised to match, as an engine that extended the file would leave it.
    pub fn append_never_written_pages(path: &Path, extra: u32) -> (u32, u32) {
        let size = page_size(path);
        let before = page_count(path);
        let after = before + extra;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .expect("open fixture");
        file.set_len(u64::from(after) * u64::from(size))
            .expect("extend fixture");
        file.seek(SeekFrom::Start(28)).expect("seek page count");
        file.write_all(&after.to_be_bytes())
            .expect("write page count");
        file.sync_all().expect("sync fixture");
        (before + 1, after)
    }

    /// GH#247's shape: a populated table loses its `sqlite_master` row, so its
    /// whole b-tree (about 500 pages) is stranded with its content intact.
    pub fn strand_populated_table(path: &Path) {
        let conn = crate::CanonicalDbConn::open_file(path.display().to_string())
            .expect("reopen canonical db");
        populate(&conn, "filler", 1500);
        conn.execute_raw("PRAGMA writable_schema = ON")
            .expect("writable schema on");
        conn.execute_raw("DELETE FROM sqlite_master WHERE name = 'filler'")
            .expect("strand filler b-tree");
        conn.execute_raw("PRAGMA writable_schema = OFF")
            .expect("writable schema off");
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{PAGE, append_never_written_pages, seed_db, strand_populated_table};
    use super::*;
    use crate::integrity::{self, CheckKind};
    use std::io::Write as _;

    fn details(conn: &crate::CanonicalDbConn) -> Vec<String> {
        let rows = integrity::probe_check_rows(
            |sql| conn.query_sync(sql, &[]).map_err(|error| error.to_string()),
            CheckKind::Full,
        )
        .expect("integrity_check rows");
        integrity::extract_check_details(&rows, CheckKind::Full)
    }

    fn canonical_details(path: &Path) -> Vec<String> {
        let conn = crate::CanonicalDbConn::open_file(path.display().to_string())
            .expect("open canonical db");
        details(&conn)
    }

    fn overwrite(path: &Path, offset: u64, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open fixture");
        file.seek(SeekFrom::Start(offset)).expect("seek");
        file.write_all(bytes).expect("write");
        file.sync_all().expect("sync fixture");
    }

    /// A WAL file holding one frame per `(page, fill)` entry.
    ///
    /// Checksums are left zero: the proof must not depend on frame validity.
    fn write_wal(main_path: &Path, frames: &[(u32, u8)]) {
        let mut wal = Vec::new();
        wal.extend_from_slice(&WAL_MAGIC[0].to_be_bytes());
        wal.extend_from_slice(&3_007_000_u32.to_be_bytes());
        wal.extend_from_slice(&PAGE.to_be_bytes());
        wal.resize(WAL_HEADER_LEN, 0);
        for &(page, fill) in frames {
            let mut frame_header = [0_u8; WAL_FRAME_HEADER_LEN];
            frame_header[..4].copy_from_slice(&page.to_be_bytes());
            wal.extend_from_slice(&frame_header);
            wal.extend(std::iter::repeat_n(fill, PAGE as usize));
        }
        std::fs::write(wal_path_for(main_path), wal).expect("write wal");
    }

    #[test]
    fn parses_both_engine_spellings_and_nothing_else() {
        assert_eq!(never_used_page_number("Page 563: never used"), Some(563));
        assert_eq!(never_used_page_number("  Page 7 is never used "), Some(7));
        assert_eq!(never_used_page_number("page 563 is never used"), Some(563));
        assert_eq!(
            never_used_page_number("database disk image is malformed: page 563 is never used"),
            Some(563)
        );
        for other in [
            "ok",
            "Page 0: never used",
            "Page : never used",
            "Page 12: never used and more",
            "Page 12: pointer map referenced",
            "Freelist: 2 of 3 pages missing from overflow list starting at 0",
            "row 4 missing from index idx_messages_thread",
            "Tree 12 page 563: never used",
        ] {
            assert_eq!(never_used_page_number(other), None, "{other}");
        }
    }

    #[test]
    fn page_set_requires_every_row_to_be_a_never_used_row() {
        let only: Vec<String> = [
            "*** in database main ***",
            "Page 9: never used",
            "Page 8: never used",
            "Page 9: never used",
        ]
        .map(str::to_string)
        .to_vec();
        assert_eq!(never_used_page_numbers(&only), Some(vec![8, 9]));

        let mixed: Vec<String> = [
            "Page 8: never used",
            "wrong # of entries in index idx_messages_thread",
        ]
        .map(str::to_string)
        .to_vec();
        assert_eq!(never_used_page_numbers(&mixed), None);
        assert_eq!(never_used_page_numbers(&["ok".to_string()]), None);
        assert_eq!(never_used_page_numbers(&[]), None);
        assert_eq!(
            never_used_page_numbers(&["*** in database main ***".to_string()]),
            None
        );
    }

    /// The live defect's shape, at one page, just past the old row limit, and
    /// at the count the live mailbox carried (425).
    #[test]
    fn never_written_extension_block_is_proven_at_any_size() {
        for extra in [1_u32, 17, 425] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("leak.sqlite3");
            seed_db(&path);
            let (first, last) = append_never_written_pages(&path, extra);

            let reported = canonical_details(&path);
            let pages = never_used_page_numbers(&reported)
                .unwrap_or_else(|| panic!("only never-used rows expected: {reported:?}"));
            assert_eq!(
                pages,
                (first..=last).collect::<Vec<u32>>(),
                "canonical SQLite must report exactly the appended block"
            );

            let proven = prove_pages_never_written(&path, &pages).expect("proof");
            assert_eq!(proven.page_count(), extra as usize);
            assert_eq!(proven.page_size(), PAGE);
            assert_eq!(proven.leaked_bytes(), u64::from(extra) * u64::from(PAGE));
            assert_eq!(proven.runs(), vec![(first, last)]);
            assert!(proven.contains_all(&[first, last]));
            assert!(!proven.contains_all(&[first - 1]));
        }
    }

    /// GH#247's shape at the same magnitude: a populated table loses its
    /// `sqlite_master` row, so canonical SQLite reports its pages with the very
    /// same `never used` rows. They hold b-tree content and must be refused.
    #[test]
    fn stranded_btree_pages_are_refused_at_the_same_row_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stranded.sqlite3");
        seed_db(&path);
        strand_populated_table(&path);

        let reported = canonical_details(&path);
        let pages = never_used_page_numbers(&reported)
            .unwrap_or_else(|| panic!("only never-used rows expected: {reported:?}"));
        assert!(
            pages.len() >= 425,
            "fixture must strand at least as many pages as the live leak: {}",
            pages.len()
        );
        match prove_pages_never_written(&path, &pages) {
            Err(NeverWrittenProofRefusal::PageHasContent {
                source: PageImageSource::MainFile,
                ..
            }) => {}
            other => panic!("stranded b-tree pages must be refused: {other:?}"),
        }
    }

    #[test]
    fn one_orphan_page_with_a_btree_type_byte_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("typed.sqlite3");
        seed_db(&path);
        let (first, last) = append_never_written_pages(&path, 17);
        let tainted = first + 5;
        // 0x0D is the page-type byte of a table leaf.
        overwrite(&path, u64::from(tainted - 1) * u64::from(PAGE), &[0x0D]);

        let pages: Vec<u32> = (first..=last).collect();
        assert_eq!(
            prove_pages_never_written(&path, &pages),
            Err(NeverWrittenProofRefusal::PageHasContent {
                page: tainted,
                source: PageImageSource::MainFile,
            })
        );
    }

    #[test]
    fn content_that_exists_only_in_the_wal_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal-content.sqlite3");
        seed_db(&path);
        let (first, last) = append_never_written_pages(&path, 17);
        let pages: Vec<u32> = (first..=last).collect();

        // Frames for other pages, and zero-filled frames for reported pages,
        // do not disturb the proof.
        write_wal(&path, &[(1, 0xAB), (first, 0), (2, 0xCD), (last, 0)]);
        prove_pages_never_written(&path, &pages).expect("zero-filled frames keep the proof");

        // One frame with content for a reported page refuses it, although the
        // main-file image is zero and the frame carries no valid checksum.
        write_wal(&path, &[(1, 0xAB), (first + 3, 0x0D)]);
        assert_eq!(
            prove_pages_never_written(&path, &pages),
            Err(NeverWrittenProofRefusal::PageHasContent {
                page: first + 3,
                source: PageImageSource::WalFrame,
            })
        );
    }

    #[test]
    fn unreadable_evidence_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("refusals.sqlite3");
        seed_db(&path);
        let (first, last) = append_never_written_pages(&path, 4);
        let pages: Vec<u32> = (first..=last).collect();

        assert_eq!(
            prove_pages_never_written(&path, &[]),
            Err(NeverWrittenProofRefusal::NoPages)
        );
        assert_eq!(
            prove_pages_never_written(&path, &[last + 1]),
            Err(NeverWrittenProofRefusal::PageOutsideMainFile { page: last + 1 })
        );
        // A page of the populated table is never proven, whatever a row says.
        assert!(matches!(
            prove_pages_never_written(&path, &[2]),
            Err(NeverWrittenProofRefusal::PageHasContent { page: 2, .. })
        ));

        std::fs::write(wal_path_for(&path), vec![0x55_u8; 4096]).expect("garbage wal");
        assert!(matches!(
            prove_pages_never_written(&path, &pages),
            Err(NeverWrittenProofRefusal::UnreadableWal(_))
        ));
        // A WAL too short to hold a frame is an empty WAL.
        std::fs::write(wal_path_for(&path), [0_u8; 32]).expect("empty wal");
        prove_pages_never_written(&path, &pages).expect("empty WAL keeps the proof");

        let not_a_db = dir.path().join("not-a-db.bin");
        std::fs::write(&not_a_db, vec![0_u8; 8192]).expect("write");
        assert!(matches!(
            prove_pages_never_written(&not_a_db, &[2]),
            Err(NeverWrittenProofRefusal::UnreadableMainFile(_))
        ));
    }

    #[test]
    fn describe_names_count_bytes_and_runs() {
        let proven = NeverWrittenOrphanPages {
            pages: vec![563, 564, 565, 900],
            page_size: PAGE,
        };
        assert_eq!(proven.runs(), vec![(563, 565), (900, 900)]);
        assert_eq!(
            proven.describe(),
            "4 never-written orphan page(s), 16384 bytes, page(s) 563-565, 900"
        );
    }
}
