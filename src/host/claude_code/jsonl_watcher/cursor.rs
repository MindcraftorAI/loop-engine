//! Per-file cursor state + tail-as-it-grows logic.
//!
//! State machine semantics per `docs/research/day-13-learn-notes.md`:
//!   - Rotation: stat.inode != cursor.inode → reset offset to 0
//!   - Truncation: stat.size < cursor.offset → reset offset to 0
//!   - No change: stat.size == cursor.offset → no-op
//!   - Append: read offset..stat.size, parse lines, advance offset
//!   - Partial line at tail: don't advance past last newline

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Per-file state for tail-as-it-grows. One per JSONL file under watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCursor {
    pub path: PathBuf,
    pub session_id: String,
    pub inode: u64,
    pub offset: u64,
    pub last_size: u64,
    pub last_mtime_ms: i64,
    /// Number of parse failures since the last ParseError event emit.
    /// The runner emits one aggregated event per `PARSE_ERROR_REPORT_EVERY`.
    pub parse_error_count: u32,
}

/// Action the cursor recommends after re-stating the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAction {
    /// File size unchanged; no read needed.
    NoChange,
    /// File grew; read from `offset..stat.size`.
    Append { read_bytes: u64 },
    /// File was rotated (inode changed) or truncated (size < offset).
    /// Cursor offset has been reset to 0; read the whole file.
    ReplayFromStart { total_bytes: u64 },
    /// File was deleted between events.
    Removed,
}

/// Result of `read_appended`. Audit Day 13 A2: caller must use
/// `actual_read - fragment_len` to advance offset, not the requested
/// `limit` (which may be larger than what was actually readable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAppendedResult {
    pub lines: Vec<String>,
    pub actual_read: u64,
    pub fragment_len: u64,
}

impl ReadAppendedResult {
    /// The number of bytes the cursor should advance past — only complete
    /// lines, never into the trailing fragment.
    pub fn advance(&self) -> u64 {
        self.actual_read.saturating_sub(self.fragment_len)
    }
}

impl FileCursor {
    /// Initialize a cursor that starts at the current file size (tail-from-now).
    /// Per learn note decision: we do NOT replay history on startup.
    pub fn new_at_eof(path: PathBuf, session_id: String) -> Result<Self> {
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("stat {} for cursor init", path.display()))?;
        Ok(Self {
            path,
            session_id,
            inode: metadata.ino(),
            offset: metadata.len(),
            last_size: metadata.len(),
            last_mtime_ms: metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000,
            parse_error_count: 0,
        })
    }

    /// Initialize a cursor at offset 0 (replay-from-start). Used for newly-
    /// detected files (SessionStarted) so we ingest any content already there.
    pub fn new_at_start(path: PathBuf, session_id: String) -> Result<Self> {
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("stat {} for cursor init", path.display()))?;
        Ok(Self {
            path,
            session_id,
            inode: metadata.ino(),
            offset: 0,
            last_size: metadata.len(),
            last_mtime_ms: metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000,
            parse_error_count: 0,
        })
    }

    /// Re-stat the file and determine what action to take. Updates internal
    /// state (inode + last_size + last_mtime_ms) to reflect the new stat.
    /// Returns the action so the caller can decide whether to read.
    pub fn classify(&mut self) -> Result<CursorAction> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CursorAction::Removed);
            }
            Err(e) => return Err(anyhow!("stat {}: {e}", self.path.display())),
        };

        let new_inode = metadata.ino();
        let new_size = metadata.len();
        let new_mtime_ms = metadata.mtime() * 1000 + metadata.mtime_nsec() / 1_000_000;

        // Rotation (atomic-rename swap): inode changed → reset, replay.
        if new_inode != self.inode {
            self.inode = new_inode;
            self.offset = 0;
            self.last_size = new_size;
            self.last_mtime_ms = new_mtime_ms;
            return Ok(CursorAction::ReplayFromStart {
                total_bytes: new_size,
            });
        }

        // Truncation: size shrunk → reset offset, replay.
        if new_size < self.offset {
            self.offset = 0;
            self.last_size = new_size;
            self.last_mtime_ms = new_mtime_ms;
            return Ok(CursorAction::ReplayFromStart {
                total_bytes: new_size,
            });
        }

        // No change: same size since last classify.
        if new_size == self.offset {
            self.last_mtime_ms = new_mtime_ms;
            return Ok(CursorAction::NoChange);
        }

        // Append: file grew.
        let read_bytes = new_size - self.offset;
        self.last_size = new_size;
        self.last_mtime_ms = new_mtime_ms;
        Ok(CursorAction::Append { read_bytes })
    }

    /// Read the appended bytes from the file starting at `from`. Returns
    /// the parsed complete lines, the actual bytes read, and the trailing
    /// fragment length.
    ///
    /// Audit Day 13 — A2 fix: actual bytes read may be less than `limit`
    /// (short read at EOF, or file shrank between classify and read). The
    /// caller must advance `offset` by `actual_read - fragment_len`, NOT
    /// by `limit - fragment_len` (the prior bug consumed phantom bytes).
    pub fn read_appended(&self, from: u64, limit: u64) -> Result<ReadAppendedResult> {
        let mut file =
            File::open(&self.path).with_context(|| format!("open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(from))
            .with_context(|| format!("seek {} to {from}", self.path.display()))?;

        let mut buf = vec![0u8; limit as usize];
        let read = file
            .read(&mut buf)
            .with_context(|| format!("read {}", self.path.display()))?;
        buf.truncate(read);

        // Identify the last complete-line boundary.
        let last_newline = buf.iter().rposition(|&b| b == b'\n');
        let (complete_bytes, fragment_bytes) = match last_newline {
            Some(idx) => (&buf[..=idx], &buf[idx + 1..]),
            None => (&[][..], &buf[..]),
        };

        let lines: Vec<String> = if complete_bytes.is_empty() {
            Vec::new()
        } else {
            std::str::from_utf8(complete_bytes)
                .with_context(|| format!("utf8 in {}", self.path.display()))?
                .split('\n')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        };
        Ok(ReadAppendedResult {
            lines,
            actual_read: read as u64,
            fragment_len: fragment_bytes.len() as u64,
        })
    }

    /// Helper: derive session_id from the JSONL filename (stem before `.jsonl`).
    /// Falls back to the full stem if the format isn't UUID-like.
    pub fn session_id_from_path(path: &Path) -> String {
        path.file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn append_file(path: &Path, more: &str) {
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(more.as_bytes()).unwrap();
    }

    #[test]
    fn new_at_eof_sets_offset_to_file_size() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "session.jsonl", "line1\nline2\n");
        let cursor = FileCursor::new_at_eof(path.clone(), "s1".into()).unwrap();
        assert_eq!(cursor.offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn classify_detects_append() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "line1\n");
        let mut cursor = FileCursor::new_at_eof(path.clone(), "s".into()).unwrap();
        append_file(&path, "line2\n");
        match cursor.classify().unwrap() {
            CursorAction::Append { read_bytes } => {
                assert_eq!(read_bytes, 6); // "line2\n"
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    #[test]
    fn classify_detects_no_change() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "line1\n");
        let mut cursor = FileCursor::new_at_eof(path, "s".into()).unwrap();
        match cursor.classify().unwrap() {
            CursorAction::NoChange => {}
            other => panic!("expected NoChange, got {other:?}"),
        }
    }

    #[test]
    fn classify_detects_truncation() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "long content here line\n");
        let mut cursor = FileCursor::new_at_eof(path.clone(), "s".into()).unwrap();
        // Truncate to a smaller size.
        std::fs::write(&path, "short\n").unwrap();
        match cursor.classify().unwrap() {
            CursorAction::ReplayFromStart { .. } => {
                assert_eq!(cursor.offset, 0);
            }
            other => panic!("expected ReplayFromStart, got {other:?}"),
        }
    }

    #[test]
    fn classify_detects_rotation() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "line1\n");
        let mut cursor = FileCursor::new_at_eof(path.clone(), "s".into()).unwrap();
        let original_inode = cursor.inode;

        // Atomic-rename pattern: write new file, rename over original.
        let tmp = dir.path().join("s.jsonl.tmp");
        std::fs::write(&tmp, "new content\n").unwrap();
        std::fs::rename(&tmp, &path).unwrap();

        match cursor.classify().unwrap() {
            CursorAction::ReplayFromStart { .. } => {
                assert_eq!(cursor.offset, 0);
                assert_ne!(cursor.inode, original_inode);
            }
            other => panic!("expected ReplayFromStart, got {other:?}"),
        }
    }

    #[test]
    fn classify_detects_removed_file() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "line1\n");
        let mut cursor = FileCursor::new_at_eof(path.clone(), "s".into()).unwrap();
        std::fs::remove_file(&path).unwrap();
        match cursor.classify().unwrap() {
            CursorAction::Removed => {}
            other => panic!("expected Removed, got {other:?}"),
        }
    }

    #[test]
    fn read_appended_splits_complete_lines() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "line1\nline2\nline3\n");
        let cursor = FileCursor::new_at_start(path, "s".into()).unwrap();
        let result = cursor.read_appended(0, 18).unwrap();
        assert_eq!(result.lines, vec!["line1", "line2", "line3"]);
        assert_eq!(result.fragment_len, 0);
        assert_eq!(result.actual_read, 18);
    }

    #[test]
    fn read_appended_preserves_trailing_fragment() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "complete\npartial");
        let cursor = FileCursor::new_at_start(path, "s".into()).unwrap();
        let result = cursor.read_appended(0, 17).unwrap();
        assert_eq!(result.lines, vec!["complete"]);
        // "partial" is 7 chars — the trailing fragment that has no \n.
        assert_eq!(result.fragment_len, 7);
        assert_eq!(result.actual_read, 16); // "complete\npartial" = 16 bytes
        assert_eq!(result.advance(), 9); // past "complete\n" only
    }

    #[test]
    fn read_appended_handles_only_fragment() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "s.jsonl", "no_newline_here");
        let cursor = FileCursor::new_at_start(path, "s".into()).unwrap();
        let result = cursor.read_appended(0, 15).unwrap();
        assert!(result.lines.is_empty());
        assert_eq!(result.fragment_len, 15);
        assert_eq!(result.actual_read, 15);
        assert_eq!(result.advance(), 0);
    }

    #[test]
    fn session_id_from_path_uses_stem() {
        let path = Path::new("/tmp/-Users-x-loop/abc-123.jsonl");
        assert_eq!(FileCursor::session_id_from_path(path), "abc-123");
    }
}
