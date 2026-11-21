//! Bounded read-only JSONL scan for diagnostics. Never loads or replays a session tree.
use crate::store::{decode_header, open_regular_session, visit_session_files};
use crate::{FsStore, SessionHeader};
use oxa_proto::{codes, OxaError};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Take};
use std::path::{Path, PathBuf};

const LOOKUP_ENTRIES: usize = 8192;
const HEADER_BYTES: u64 = 16 * 1024;
const LINE_BYTES: u64 = 2 * 1024 * 1024;
const SCAN_BYTES: u64 = 256 * 1024 * 1024;
const SCAN_LINES: usize = 200_000;

pub struct SessionScan {
    reader: BufReader<Take<File>>,
    limited: bool,
}

impl FsStore {
    /// Open a bounded snapshot by path, exact id, or unique id prefix. This creates no files
    /// or locks. For large session stores, a direct path bypasses the id lookup budget.
    pub fn open_scan(&self, reference: &str) -> Result<SessionScan, OxaError> {
        if reference.is_empty() {
            return Err(OxaError::new(codes::USAGE, "missing session reference"));
        }
        let (scan, _) = if reference.contains('/') || reference.ends_with(".jsonl") {
            open(Path::new(reference))?
        } else {
            let (id, path) = resolve_id(self.root(), reference)?;
            let (scan, header) = open(&path)?;
            if header.id != id {
                return Err(OxaError::new(codes::INVALID, "session changed during lookup"));
            }
            (scan, header)
        };
        Ok(scan)
    }
}

fn resolve_id(root: &Path, query: &str) -> Result<(String, PathBuf), OxaError> {
    let mut exact: Option<PathBuf> = None;
    let mut prefix = None;
    let mut ambiguous = false;
    let complete = visit_session_files(root, None, LOOKUP_ENTRIES, |path| {
        let Ok((_, header)) = open(&path) else { return };
        if header.id == query {
            // Match the store's sorted exact-id preference.
            if exact.as_ref().is_none_or(|prior| path < *prior) {
                exact = Some(path);
            }
        } else if header.id.starts_with(query) {
            ambiguous |= prefix.is_some();
            prefix = Some((header.id, path));
        }
    });
    if !complete {
        return Err(OxaError::new(
            codes::INVALID,
            "session lookup limit reached; give the session file path",
        ));
    }
    if let Some(path) = exact {
        return Ok((query.to_owned(), path));
    }
    if ambiguous {
        return Err(OxaError::new(
            codes::INVALID,
            "ambiguous session prefix; give the exact id or file path",
        ));
    }
    prefix.ok_or_else(|| OxaError::new(codes::NOT_FOUND, "no matching session"))
}

fn open(path: &Path) -> Result<(SessionScan, SessionHeader), OxaError> {
    let hide = |e: OxaError| OxaError::new(e.code, "cannot open a regular session file");
    let file = open_regular_session(path).map_err(hide)?;
    let size = file.metadata().map_err(read_error)?.len();
    let mut reader = BufReader::new(file.take(size.min(SCAN_BYTES)));
    let mut line = Vec::new();
    (&mut reader)
        .take(HEADER_BYTES)
        .read_until(b'\n', &mut line)
        .map_err(read_error)?;
    if line.pop() != Some(b'\n') {
        return Err(OxaError::new(
            codes::INVALID,
            "missing, torn or oversized session header",
        ));
    }
    let header = decode_header(&line).map_err(|_| OxaError::new(codes::INVALID, "invalid session header"))?;
    Ok((
        SessionScan {
            reader,
            limited: size > SCAN_BYTES,
        },
        header,
    ))
}

fn read_error(error: io::Error) -> OxaError {
    OxaError::io("cannot read session report input", &error)
}

impl SessionScan {
    /// Visit complete lines in physical append order, with `None` for a skipped oversized
    /// line. Returns false if there were gaps, a torn tail or a scan limit. Appends beyond
    /// the size observed at open are not read.
    pub fn visit(
        mut self,
        mut visit: impl FnMut(Option<&[u8]>) -> Result<(), OxaError>,
    ) -> Result<bool, OxaError> {
        let mut complete = !self.limited;
        let mut line = Vec::new();
        for _ in 0..SCAN_LINES {
            line.clear();
            (&mut self.reader)
                .take(LINE_BYTES)
                .read_until(b'\n', &mut line)
                .map_err(read_error)?;
            if line.is_empty() {
                return Ok(complete && self.reader.get_ref().limit() == 0);
            }
            if line.last() == Some(&b'\n') {
                visit(Some(&line))?;
                continue;
            }
            complete = false;
            if self.reader.skip_until(b'\n').map_err(read_error)? == 0 {
                return Ok(false);
            }
            visit(None)?;
        }
        Ok(complete
            && self.reader.fill_buf().map_err(read_error)?.is_empty()
            && self.reader.get_ref().limit() == 0)
    }
}
