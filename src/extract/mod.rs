use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::Path;
use std::time::Instant;

use quick_xml::Reader;
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};
use quick_xml::reader::NsReader;
use zip::ZipArchive;
use zip::read::{ZipReadOptions, read_zipfile_from_stream, read_zipfile_from_stream_with_options};

use crate::config::Limits;

pub mod code;
pub mod csv;
pub mod docx;
pub mod html;
pub mod markdown;
pub mod odt;
pub mod pdf;
pub mod sheet;
pub mod text;

const MAX_BINARY_CHECK_BYTES: usize = 8 * 1024;
const MAX_GENERATED_CHECK_BYTES: usize = 4 * 1024;
const MAX_MEAN_LINE_LEN: usize = 250;
const MAX_ZIP_RATIO: u64 = 100;
const MAX_ZIP_UNCOMPRESSED_BYTES: u64 = 500 * 1024 * 1024;
pub(crate) const MAX_ZIP_XML_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const OLE2_MAGIC: &[u8; 8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
pub(crate) const ODF_ODT_MIME: &str = "application/vnd.oasis.opendocument.text";
pub(crate) const ODF_ODS_MIME: &str = "application/vnd.oasis.opendocument.spreadsheet";
const DC_NAMESPACE: &str = "http://purl.org/dc/elements/1.1/";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OoxmlKind {
    Docx,
    Xlsx,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileType {
    Markdown,
    Text,
    Html,
    Pdf,
    Docx,
    Odt,
    Xlsx,
    Ods,
    Xls,
    Csv,
    Code(&'static str),
}

impl FileType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::Html => "html",
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Xlsx => "xlsx",
            Self::Ods => "ods",
            Self::Xls => "xls",
            Self::Csv => "csv",
            Self::Code(_) => "code",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentKind {
    Page(u32),
    Section {
        heading: Option<String>,
    },
    Sheet {
        name: String,
        first_row: u32,
        last_row: u32,
        row_numbers: Vec<u32>,
    },
    Whole,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    pub kind: SegmentKind,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Extracted {
    pub title: String,
    pub file_type: FileType,
    pub segments: Vec<Segment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Skip {
    Mismatch,
    Binary,
    Minified,
    Generated,
    TooLarge,
    Unsupported,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Mismatch => "mismatch",
            Self::Binary => "binary",
            Self::Minified => "minified",
            Self::Generated => "generated",
            Self::TooLarge => "too_large",
            Self::Unsupported => "unsupported",
        };
        f.write_str(label)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtractError {
    Skipped(Skip),
    ZipBomb,
    TooLarge,
    Timeout,
    Parse(String),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skipped(skip) => write!(f, "skipped: {skip}"),
            Self::ZipBomb => f.write_str("zip decompression limit exceeded"),
            Self::TooLarge => f.write_str("extracted content limit exceeded"),
            Self::Timeout => f.write_str("timeout"),
            Self::Parse(msg) => write!(f, "{msg}"),
        }
    }
}

impl Error for ExtractError {}

impl From<Skip> for ExtractError {
    fn from(skip: Skip) -> Self {
        Self::Skipped(skip)
    }
}

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

struct DeadlineGuard;

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        DEADLINE.with(|d| d.set(None));
    }
}

pub(crate) fn check_deadline() -> Result<(), ExtractError> {
    if let Some(deadline) = DEADLINE.with(Cell::get)
        && Instant::now() >= deadline
    {
        return Err(ExtractError::Timeout);
    }
    Ok(())
}

pub(crate) struct TextBudget {
    used: usize,
    max: usize,
    deadline: Option<Instant>,
    ticks: u32,
}

impl TextBudget {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            used: 0,
            max,
            deadline: DEADLINE.with(Cell::get),
            ticks: 0,
        }
    }

    pub(crate) fn used(&self) -> usize {
        self.used
    }

    pub(crate) fn remaining(&self) -> usize {
        if self.max == 0 {
            usize::MAX
        } else {
            self.max.saturating_sub(self.used)
        }
    }

    pub(crate) fn charge(&mut self, bytes: usize) -> Result<(), ExtractError> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks & 0xFF == 0
            && let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(ExtractError::Timeout);
        }
        self.used = self.used.saturating_add(bytes);
        if self.max > 0 && self.used > self.max {
            return Err(ExtractError::TooLarge);
        }
        Ok(())
    }

    pub(crate) fn push_str(&mut self, dst: &mut String, s: &str) -> Result<(), ExtractError> {
        self.charge(s.len())?;
        dst.push_str(s);
        Ok(())
    }

    pub(crate) fn push_char(&mut self, dst: &mut String, ch: char) -> Result<(), ExtractError> {
        self.charge(ch.len_utf8())?;
        dst.push(ch);
        Ok(())
    }

    pub(crate) fn push_spaces(
        &mut self,
        dst: &mut String,
        count: usize,
    ) -> Result<(), ExtractError> {
        self.charge(count)?;
        dst.extend(std::iter::repeat_n(' ', count));
        Ok(())
    }
}

pub fn detect(relpath: &str, head: &[u8]) -> Result<FileType, Skip> {
    let ext = extension_lower(relpath);

    match ext.as_deref() {
        Some("pdf") => {
            if head.starts_with(b"%PDF-") {
                return Ok(FileType::Pdf);
            }
            return Err(Skip::Mismatch);
        }
        Some("docx") => {
            if check_ooxml_head(head, OoxmlKind::Docx) {
                return Ok(FileType::Docx);
            }
            return Err(Skip::Mismatch);
        }
        Some("xlsx") => {
            if check_ooxml_head(head, OoxmlKind::Xlsx) {
                return Ok(FileType::Xlsx);
            }
            return Err(Skip::Mismatch);
        }
        Some("odt") => {
            if check_odf_head(head, ODF_ODT_MIME) {
                return Ok(FileType::Odt);
            }
            return Err(Skip::Mismatch);
        }
        Some("ods") => {
            if check_odf_head(head, ODF_ODS_MIME) {
                return Ok(FileType::Ods);
            }
            return Err(Skip::Mismatch);
        }
        Some("xls") => {
            if head.starts_with(OLE2_MAGIC) {
                return Ok(FileType::Xls);
            }
            return Err(Skip::Mismatch);
        }
        _ => {}
    }

    let file_type = match ext.as_deref() {
        Some("html" | "htm" | "xhtml") => FileType::Html,
        Some("md" | "markdown" | "mdx") => FileType::Markdown,
        Some("csv") => FileType::Csv,
        Some(
            "txt" | "text" | "rst" | "adoc" | "asciidoc" | "org" | "log" | "cfg" | "ini" | "conf",
        ) => FileType::Text,
        Some(other) => {
            if let Some(lang) = code::language_for_ext(other) {
                FileType::Code(lang)
            } else {
                return Err(Skip::Unsupported);
            }
        }
        None => {
            if is_known_plain_filename(relpath) {
                FileType::Text
            } else {
                return Err(Skip::Unsupported);
            }
        }
    };

    check_text_heuristics(relpath, file_type, head, true)?;
    Ok(file_type)
}

pub fn extract_with_deadline(
    relpath: &str,
    file_type: FileType,
    bytes: &[u8],
    limits: &Limits,
    deadline: Instant,
) -> Result<Extracted, ExtractError> {
    DEADLINE.with(|d| d.set(Some(deadline)));
    let _guard = DeadlineGuard;
    check_deadline()?;
    let res = extract(relpath, file_type, bytes, limits)?;
    check_deadline()?;
    Ok(res)
}

pub fn extract(
    relpath: &str,
    file_type: FileType,
    bytes: &[u8],
    limits: &Limits,
) -> Result<Extracted, ExtractError> {
    let max_bytes = limits.max_file_mb.saturating_mul(1024 * 1024);
    if max_bytes > 0 && (bytes.len() as u64) > max_bytes {
        return Err(ExtractError::Skipped(Skip::TooLarge));
    }

    match file_type {
        FileType::Pdf => pdf::extract(relpath, bytes, limits.max_text_bytes),
        FileType::Docx => docx::extract(relpath, bytes, limits.max_text_bytes),
        FileType::Odt => odt::extract(relpath, bytes, limits.max_text_bytes),
        FileType::Xlsx | FileType::Ods | FileType::Xls => {
            sheet::extract(relpath, file_type, bytes, limits)
        }
        FileType::Html => {
            let s = check_utf8_and_nul(bytes, false)?;
            html::extract(relpath, s, limits.max_text_bytes)
        }
        FileType::Markdown => {
            let s = check_text_heuristics(relpath, file_type, bytes, false)?;
            markdown::extract(relpath, s, limits.max_text_bytes)
        }
        FileType::Text => {
            let s = check_text_heuristics(relpath, file_type, bytes, false)?;
            text::extract(relpath, s, limits.max_text_bytes)
        }
        FileType::Csv => {
            check_text_heuristics(relpath, file_type, bytes, false)?;
            csv::extract(relpath, bytes, limits.max_text_bytes)
        }
        FileType::Code(lang) => {
            let s = check_text_heuristics(relpath, file_type, bytes, false)?;
            code::extract(relpath, lang, s, limits.max_text_bytes)
        }
    }
}

fn check_utf8_and_nul(bytes: &[u8], allow_truncated_tail: bool) -> Result<&str, Skip> {
    let nul_window = &bytes[..bytes.len().min(MAX_BINARY_CHECK_BYTES)];
    if nul_window.contains(&0) {
        return Err(Skip::Binary);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(err) if allow_truncated_tail && err.error_len().is_none() && bytes.len() >= 4096 => {
            std::str::from_utf8(&bytes[..err.valid_up_to()]).map_err(|_| Skip::Binary)
        }
        Err(_) => Err(Skip::Binary),
    }
}

fn check_text_heuristics<'a>(
    relpath: &str,
    file_type: FileType,
    bytes: &'a [u8],
    allow_truncated_tail: bool,
) -> Result<&'a str, Skip> {
    let s = check_utf8_and_nul(bytes, allow_truncated_tail)?;

    if matches!(
        file_type,
        FileType::Text | FileType::Csv | FileType::Code(_)
    ) {
        let head_str = prefix_str(s, MAX_GENERATED_CHECK_BYTES);
        if is_generated(head_str) {
            return Err(Skip::Generated);
        }
        if is_minified_ext(relpath) && is_minified(s) {
            return Err(Skip::Minified);
        }
    }

    Ok(s)
}

fn is_generated(head_4k: &str) -> bool {
    if head_4k.contains("@generated") {
        return true;
    }
    if let Some(pos) = head_4k.find("Code generated")
        && head_4k[pos..].contains("DO NOT EDIT")
    {
        return true;
    }
    false
}

fn is_minified_ext(relpath: &str) -> bool {
    matches!(
        extension_lower(relpath).as_deref(),
        Some("js" | "css" | "json" | "map")
    )
}

fn is_minified(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut line_count: usize = 0;
    let mut total_len: usize = 0;
    for line in s.lines() {
        line_count += 1;
        total_len += line.len();
    }
    let line_count = line_count.max(1);
    total_len > MAX_MEAN_LINE_LEN * line_count
}

fn prefix_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn check_odf_head(head: &[u8], expected_mime: &str) -> bool {
    if !head.starts_with(b"PK\x03\x04") {
        return false;
    }
    let mut cursor = Cursor::new(head);
    let Ok(Some(entry)) = read_zipfile_from_stream(&mut cursor) else {
        return false;
    };
    if entry.name_raw() != b"mimetype" {
        return false;
    }
    let mut buf = Vec::with_capacity(128);
    if entry.take(128).read_to_end(&mut buf).is_err() {
        return false;
    }
    std::str::from_utf8(&buf).ok().map(str::trim) == Some(expected_mime)
}

pub(crate) fn verify_odf_mimetype(bytes: &[u8], expected_mime: &str) -> Result<(), ExtractError> {
    if !bytes.starts_with(b"PK\x03\x04") {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|_| ExtractError::Skipped(Skip::Mismatch))?;
    if archive.is_empty() {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    let entry = archive
        .by_index(0)
        .map_err(|_| ExtractError::Skipped(Skip::Mismatch))?;
    if entry.name_raw() != b"mimetype" {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    let mut buf = Vec::with_capacity(128);
    entry
        .take(128)
        .read_to_end(&mut buf)
        .map_err(|_| ExtractError::Skipped(Skip::Mismatch))?;
    if std::str::from_utf8(&buf).ok().map(str::trim) != Some(expected_mime) {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    Ok(())
}

fn check_ooxml_head(head: &[u8], kind: OoxmlKind) -> bool {
    if !head.starts_with(b"PK\x03\x04") {
        return false;
    }
    if ZipArchive::new(Cursor::new(head)).is_ok() {
        return verify_ooxml_content_type(head, kind).is_ok();
    }
    scan_local_zip_headers(head, b"[Content_Types].xml")
}

pub(crate) fn verify_ooxml_content_type(bytes: &[u8], kind: OoxmlKind) -> Result<(), ExtractError> {
    let mut read_so_far = 0;
    let Some(matched) =
        with_zip_xml_reader(bytes, "[Content_Types].xml", &mut read_so_far, |reader| {
            has_ooxml_main_content_type(reader, kind)
        })?
    else {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    };
    if !matched {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    Ok(())
}

fn has_ooxml_main_content_type(
    reader: &mut dyn BufRead,
    kind: OoxmlKind,
) -> Result<bool, ExtractError> {
    let mut xml_reader = Reader::from_reader(reader);
    let mut event_buf = Vec::new();

    loop {
        event_buf.clear();
        match xml_reader.read_event_into(&mut event_buf) {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                let qname = e.name();
                let tag = local_name(qname.as_ref());
                if tag == "Override" || tag == "Default" {
                    for attr in e.attributes().flatten() {
                        if local_name(attr.key.as_ref()) == "ContentType"
                            && matches_ooxml_main_type(attr.value.as_ref(), kind)
                        {
                            return Ok(true);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return Err(ExtractError::Skipped(Skip::Mismatch)),
            _ => {}
        }
    }
    Ok(false)
}

fn matches_ooxml_main_type(content_type: &str, kind: OoxmlKind) -> bool {
    let ct = content_type.trim();
    match kind {
        OoxmlKind::Docx => {
            (ct.starts_with("application/vnd.openxmlformats-officedocument.wordprocessingml.")
                || ct.starts_with("application/vnd.ms-word."))
                && ct.ends_with(".main+xml")
        }
        OoxmlKind::Xlsx => {
            (ct.starts_with("application/vnd.openxmlformats-officedocument.spreadsheetml.")
                || ct.starts_with("application/vnd.ms-excel."))
                && ct.ends_with(".main+xml")
        }
    }
}

fn scan_local_zip_headers(buf: &[u8], expected: &[u8]) -> bool {
    let mut pos = 0;
    while pos + 30 <= buf.len() {
        if &buf[pos..pos + 4] != b"PK\x03\x04" {
            break;
        }
        let name_len = u16::from_le_bytes([buf[pos + 26], buf[pos + 27]]) as usize;
        let extra_len = u16::from_le_bytes([buf[pos + 28], buf[pos + 29]]) as usize;
        let comp_size =
            u32::from_le_bytes([buf[pos + 18], buf[pos + 19], buf[pos + 20], buf[pos + 21]])
                as usize;
        let name_start = pos + 30;
        let Some(name_end) = name_start.checked_add(name_len) else {
            break;
        };
        if name_end > buf.len() {
            break;
        }
        if &buf[name_start..name_end] == expected {
            return true;
        }
        let Some(data_start) = name_end.checked_add(extra_len) else {
            break;
        };
        let Some(next) = data_start.checked_add(comp_size) else {
            break;
        };
        if next <= pos {
            break;
        }
        pos = next;
    }
    false
}

pub(crate) fn zip_budget(archive_len: usize) -> u64 {
    (archive_len as u64)
        .saturating_mul(MAX_ZIP_RATIO)
        .min(MAX_ZIP_UNCOMPRESSED_BYTES)
}

pub(crate) fn verify_zip_guard(bytes: &[u8]) -> Result<(), ExtractError> {
    if !bytes.starts_with(b"PK\x03\x04") {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|_| ExtractError::Skipped(Skip::Mismatch))?;

    let budget = zip_budget(bytes.len());
    let mut declared_total: u64 = 0;
    let mut entries_meta = Vec::with_capacity(archive.len());

    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| ExtractError::Parse(format!("zip header error: {e}")))?;
        declared_total = declared_total.saturating_add(entry.size());
        if declared_total > budget {
            return Err(ExtractError::ZipBomb);
        }
        let is_xml = entry.name_raw().ends_with(b".xml");
        if is_xml && entry.size() > MAX_ZIP_XML_BYTES {
            return Err(ExtractError::TooLarge);
        }
        entries_meta.push((entry.header_start(), entry.compressed_size(), is_xml));
    }

    let mut actual_total: u64 = 0;
    let mut cursor = Cursor::new(bytes);
    for (header_start, comp_size, is_xml) in entries_meta {
        check_deadline()?;
        cursor.set_position(header_start);
        let opts = ZipReadOptions::new()
            .ignore_crc32(true)
            .override_compressed_size(comp_size)
            .override_uncompressed_size(u64::MAX);
        let entry = read_zipfile_from_stream_with_options(&mut cursor, opts)
            .map_err(|e| ExtractError::Parse(format!("zip entry error: {e}")))?
            .ok_or_else(|| ExtractError::Parse("missing zip local header".into()))?;
        let remaining_budget = budget.saturating_sub(actual_total);
        let limit = if is_xml {
            remaining_budget.min(MAX_ZIP_XML_BYTES)
        } else {
            remaining_budget
        };
        let mut limited = entry.take(limit + 1);
        let copied = io::copy(&mut limited, &mut io::sink())
            .map_err(|e| ExtractError::Parse(format!("zip read error: {e}")))?;
        actual_total = actual_total.saturating_add(copied);
        if actual_total > budget {
            return Err(ExtractError::ZipBomb);
        }
        if is_xml && copied > MAX_ZIP_XML_BYTES {
            return Err(ExtractError::TooLarge);
        }
    }

    Ok(())
}

struct ZipMemberReader<'a, R> {
    inner: R,
    member_read: u64,
    member_limit: u64,
    total_read: &'a mut u64,
    total_budget: u64,
    exceeded_member: bool,
    exceeded_total: bool,
}

impl<R: Read> Read for ZipMemberReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.exceeded_total || self.exceeded_member {
            return Err(io::Error::other("zip limit exceeded"));
        }
        let n = self.inner.read(buf)?;
        *self.total_read = self.total_read.saturating_add(n as u64);
        self.member_read = self.member_read.saturating_add(n as u64);
        if *self.total_read > self.total_budget {
            self.exceeded_total = true;
            return Err(io::Error::other("zip total budget exceeded"));
        }
        if self.member_read > self.member_limit {
            self.exceeded_member = true;
            return Err(io::Error::other("zip xml member limit exceeded"));
        }
        Ok(n)
    }
}

pub(crate) fn with_zip_xml_reader<T, F>(
    bytes: &[u8],
    member_name: &str,
    read_so_far: &mut u64,
    f: F,
) -> Result<Option<T>, ExtractError>
where
    F: FnOnce(&mut dyn BufRead) -> Result<T, ExtractError>,
{
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|_| ExtractError::Skipped(Skip::Mismatch))?;
    let budget = zip_budget(bytes.len());

    let (header_start, comp_size) = {
        let Ok(entry) = archive.by_name_raw(member_name.as_bytes()) else {
            return Ok(None);
        };
        if read_so_far.saturating_add(entry.size()) > budget {
            return Err(ExtractError::ZipBomb);
        }
        if entry.size() > MAX_ZIP_XML_BYTES {
            return Err(ExtractError::TooLarge);
        }
        (entry.header_start(), entry.compressed_size())
    };

    let mut cursor = Cursor::new(bytes);
    cursor.set_position(header_start);
    let opts = ZipReadOptions::new()
        .ignore_crc32(true)
        .override_compressed_size(comp_size)
        .override_uncompressed_size(u64::MAX);
    let entry = read_zipfile_from_stream_with_options(&mut cursor, opts)
        .map_err(|e| ExtractError::Parse(format!("zip entry error: {e}")))?
        .ok_or_else(|| ExtractError::Parse("missing zip local header".into()))?;

    let mut member_reader = ZipMemberReader {
        inner: entry,
        member_read: 0,
        member_limit: MAX_ZIP_XML_BYTES,
        total_read: read_so_far,
        total_budget: budget,
        exceeded_member: false,
        exceeded_total: false,
    };

    let res = {
        let mut buf_reader = BufReader::new(&mut member_reader);
        f(&mut buf_reader)
    };

    if member_reader.exceeded_total {
        return Err(ExtractError::ZipBomb);
    }
    if member_reader.exceeded_member {
        return Err(ExtractError::TooLarge);
    }

    res.map(Some)
}

pub(crate) fn parse_dc_title(reader: &mut dyn BufRead) -> Result<Option<String>, ExtractError> {
    let mut ns_reader = NsReader::from_reader(reader);
    let mut event_buf = Vec::new();
    let mut in_dc_title = false;
    let mut text_buf = String::new();

    loop {
        event_buf.clear();
        match ns_reader.read_resolved_event_into(&mut event_buf) {
            Ok((ResolveResult::Bound(Namespace(DC_NAMESPACE)), Event::Start(e)))
                if e.local_name().as_ref() == "title" =>
            {
                in_dc_title = true;
                text_buf.clear();
            }
            Ok((ResolveResult::Bound(Namespace(DC_NAMESPACE)), Event::End(e)))
                if e.local_name().as_ref() == "title" && in_dc_title =>
            {
                let trimmed = text_buf.trim();
                if !trimmed.is_empty() {
                    return Ok(Some(trimmed.to_string()));
                }
                in_dc_title = false;
            }
            Ok((_, Event::Text(e))) if in_dc_title => {
                if text_buf.len() < 64 * 1024 {
                    text_buf.push_str(e.as_ref());
                }
            }
            Ok((_, Event::GeneralRef(r))) if in_dc_title => {
                if text_buf.len() < 64 * 1024 {
                    append_general_ref(&r, &mut text_buf);
                }
            }
            Ok((_, Event::CData(e))) if in_dc_title => {
                if text_buf.len() < 64 * 1024 {
                    text_buf.push_str(e.as_ref());
                }
            }
            Ok((_, Event::Eof)) | Err(_) => break,
            _ => {}
        }
    }
    Ok(None)
}

pub(crate) fn file_name(relpath: &str) -> String {
    Path::new(relpath)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(relpath)
        .to_string()
}

pub(crate) fn line_count(text: &str) -> u32 {
    (text.lines().count() as u32).max(1)
}

pub(crate) fn local_name(qname: &str) -> &str {
    match qname.rfind(':') {
        Some(idx) => &qname[idx + 1..],
        None => qname,
    }
}

pub(crate) fn append_general_ref(r: &quick_xml::events::BytesRef<'_>, out: &mut String) {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        out.push(ch);
        return;
    }
    match r.as_ref() {
        "amp" => out.push('&'),
        "lt" => out.push('<'),
        "gt" => out.push('>'),
        "quot" => out.push('"'),
        "apos" => out.push('\''),
        _ => {}
    }
}

pub(crate) fn append_general_ref_budget(
    r: &quick_xml::events::BytesRef<'_>,
    out: &mut String,
    budget: &mut TextBudget,
) -> Result<(), ExtractError> {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        return budget.push_char(out, ch);
    }
    match r.as_ref() {
        "amp" => budget.push_char(out, '&'),
        "lt" => budget.push_char(out, '<'),
        "gt" => budget.push_char(out, '>'),
        "quot" => budget.push_char(out, '"'),
        "apos" => budget.push_char(out, '\''),
        _ => Ok(()),
    }
}

fn extension_lower(relpath: &str) -> Option<String> {
    Path::new(relpath)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
}

fn is_known_plain_filename(relpath: &str) -> bool {
    let name = file_name(relpath).to_ascii_uppercase();
    matches!(
        name.as_str(),
        "README" | "LICENSE" | "COPYING" | "CHANGELOG" | "AUTHORS" | "NOTICE" | "TODO"
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Instant;

    use pdfium_render::prelude::*;
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn build_zip(entries: &[(&str, &[u8], CompressionMethod)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            for &(name, data, method) in entries {
                let opts = SimpleFileOptions::default().compression_method(method);
                zw.start_file(name, opts).unwrap();
                zw.write_all(data).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    fn build_docx(dc_title: Option<&str>, heading: Option<&str>, body_text: &str) -> Vec<u8> {
        let content_types = br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
        let heading_xml = heading
            .map(|h| {
                format!(
                    r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>{h}</w:t></w:r></w:p>"#
                )
            })
            .unwrap_or_default();
        let doc_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    {heading_xml}
    <w:p><w:r><w:t>{body_text}</w:t></w:r></w:p>
  </w:body>
</w:document>"#
        );
        let mut entries: Vec<(&str, &[u8], CompressionMethod)> = vec![
            (
                "[Content_Types].xml",
                content_types,
                CompressionMethod::Deflated,
            ),
            (
                "word/document.xml",
                doc_xml.as_bytes(),
                CompressionMethod::Deflated,
            ),
        ];
        let core_xml: String;
        if let Some(t) = dc_title {
            core_xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <dc:title>{t}</dc:title>
</cp:coreProperties>"#
            );
            entries.push((
                "docProps/core.xml",
                core_xml.as_bytes(),
                CompressionMethod::Deflated,
            ));
        }
        build_zip(&entries)
    }

    fn build_odt(dc_title: Option<&str>, heading: Option<&str>, body_text: &str) -> Vec<u8> {
        let mimetype = ODF_ODT_MIME.as_bytes();
        let heading_xml = heading
            .map(|h| format!(r#"<text:h text:outline-level="1">{h}</text:h>"#))
            .unwrap_or_default();
        let content_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:text>
    {heading_xml}
    <text:p>{body_text}</text:p>
  </office:text></office:body>
</office:document-content>"#
        );
        let mut entries: Vec<(&str, &[u8], CompressionMethod)> = vec![
            ("mimetype", mimetype, CompressionMethod::Stored),
            (
                "content.xml",
                content_xml.as_bytes(),
                CompressionMethod::Deflated,
            ),
        ];
        let meta_xml: String;
        if let Some(t) = dc_title {
            meta_xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <office:meta><dc:title>{t}</dc:title></office:meta>
</office:document-meta>"#
            );
            entries.push(("meta.xml", meta_xml.as_bytes(), CompressionMethod::Deflated));
        }
        build_zip(&entries)
    }

    fn build_ods(sheet_name: &str, cell_a1: &str, cell_b1: &str) -> Vec<u8> {
        let mimetype = ODF_ODS_MIME.as_bytes();
        let manifest_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0">
  <manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#;
        let content_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="{sheet_name}"><table:table-row><table:table-cell office:value-type="string"><text:p>{cell_a1}</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>{cell_b1}</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#
        );
        build_zip(&[
            ("mimetype", mimetype, CompressionMethod::Stored),
            (
                "META-INF/manifest.xml",
                manifest_xml,
                CompressionMethod::Deflated,
            ),
            (
                "content.xml",
                content_xml.as_bytes(),
                CompressionMethod::Deflated,
            ),
        ])
    }

    fn build_xlsx(sheet_name: &str, cell_a1: &str, cell_b1: &str) -> Vec<u8> {
        let sheet1 = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="inlineStr"><is><t>{cell_a1}</t></is></c>
      <c r="B1" t="inlineStr"><is><t>{cell_b1}</t></is></c>
    </row>
  </sheetData>
</worksheet>"#
        );
        build_xlsx_with_raw_sheet(sheet_name, &sheet1, None)
    }

    fn build_xlsx_with_raw_sheet(
        sheet_name: &str,
        sheet1_xml: &str,
        shared_strings_xml: Option<&str>,
    ) -> Vec<u8> {
        let content_types = br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
        let rels = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
        let workbook = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="{sheet_name}" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#
        );
        let wb_rels = br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
</Relationships>"#;
        let mut entries: Vec<(&str, &[u8], CompressionMethod)> = vec![
            (
                "[Content_Types].xml",
                content_types,
                CompressionMethod::Deflated,
            ),
            ("_rels/.rels", rels, CompressionMethod::Deflated),
            (
                "xl/workbook.xml",
                workbook.as_bytes(),
                CompressionMethod::Deflated,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                wb_rels,
                CompressionMethod::Deflated,
            ),
            (
                "xl/worksheets/sheet1.xml",
                sheet1_xml.as_bytes(),
                CompressionMethod::Deflated,
            ),
        ];
        if let Some(sst) = shared_strings_xml {
            entries.push((
                "xl/sharedStrings.xml",
                sst.as_bytes(),
                CompressionMethod::Deflated,
            ));
        }
        build_zip(&entries)
    }

    /// Builds a minimal valid 9-sector OLE2 (Compound File Binary) container with a single
    /// 4096-byte stream under the root entry.
    fn build_ole2_single_stream(stream_name: &str, stream_payload: &[u8]) -> Vec<u8> {
        let mut data = vec![0u8; 9 * 512];

        // Header (512 bytes)
        data[0..8].copy_from_slice(OLE2_MAGIC);
        data[24..26].copy_from_slice(&0x003Eu16.to_le_bytes()); // minor version
        data[26..28].copy_from_slice(&0x0003u16.to_le_bytes()); // version 3
        data[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes()); // little-endian
        data[30..32].copy_from_slice(&9u16.to_le_bytes()); // 512-byte sectors (2^9)
        data[32..34].copy_from_slice(&6u16.to_le_bytes()); // 64-byte mini sectors (2^6)
        data[44..48].copy_from_slice(&1u32.to_le_bytes()); // 1 FAT sector
        data[48..52].copy_from_slice(&1u32.to_le_bytes()); // dir start sector = 1
        data[56..60].copy_from_slice(&4096u32.to_le_bytes()); // mini stream cutoff
        data[60..64].copy_from_slice(&0xFFFFFFFEu32.to_le_bytes()); // mini FAT start = ENDOFCHAIN
        data[68..72].copy_from_slice(&0xFFFFFFFEu32.to_le_bytes()); // DIFAT start = ENDOFCHAIN
        data[76..80].copy_from_slice(&0u32.to_le_bytes()); // DIFAT[0] = sector 0
        for slot in data[80..512].as_chunks_mut::<4>().0 {
            slot.copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        }

        // Sector 0 (offset 512): FAT
        let fat = &mut data[512..1024];
        for slot in fat.as_chunks_mut::<4>().0 {
            slot.copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        }
        fat[0..4].copy_from_slice(&0xFFFFFFFDu32.to_le_bytes()); // sector 0: FATSECT
        fat[4..8].copy_from_slice(&0xFFFFFFFEu32.to_le_bytes()); // sector 1: Dir ENDOFCHAIN
        for s in 2u32..9u32 {
            let next = if s < 8 { s + 1 } else { 0xFFFFFFFE };
            let off = (s as usize) * 4;
            fat[off..off + 4].copy_from_slice(&next.to_le_bytes());
        }

        // Sector 1 (offset 1024): Directory (4 entries x 128 bytes)
        let write_dir_entry = |entry: &mut [u8],
                               name: &str,
                               obj_type: u8,
                               child: u32,
                               start_sec: u32,
                               stream_len: u32| {
            let mut u16_len = 0usize;
            for ch in name.encode_utf16() {
                let pos = u16_len * 2;
                entry[pos..pos + 2].copy_from_slice(&ch.to_le_bytes());
                u16_len += 1;
            }
            let name_bytes = ((u16_len + 1) * 2) as u16;
            entry[64..66].copy_from_slice(&name_bytes.to_le_bytes());
            entry[66] = obj_type;
            entry[67] = 1; // black node
            entry[68..72].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // left sibling
            entry[72..76].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // right sibling
            entry[76..80].copy_from_slice(&child.to_le_bytes());
            entry[116..120].copy_from_slice(&start_sec.to_le_bytes());
            entry[120..124].copy_from_slice(&stream_len.to_le_bytes());
        };

        let dir = &mut data[1024..1536];
        write_dir_entry(&mut dir[0..128], "Root Entry", 5, 1, 0xFFFFFFFE, 0);
        write_dir_entry(&mut dir[128..256], stream_name, 2, 0xFFFFFFFF, 2, 4096);

        // Sectors 2..=8 (offset 1536..5120): 4096-byte stream
        let copy_len = stream_payload.len().min(4096);
        data[1536..1536 + copy_len].copy_from_slice(&stream_payload[..copy_len]);
        data
    }

    fn biff_record(rec_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&rec_type.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn build_xls(sheet_name: &str, cell_a1: &str) -> Vec<u8> {
        let mut stream = Vec::new();

        // Workbook globals BOF (BIFF8 = 0x0600, dt = 0x0005)
        let mut bof_globals = vec![0u8; 16];
        bof_globals[0..2].copy_from_slice(&0x0600u16.to_le_bytes());
        bof_globals[2..4].copy_from_slice(&0x0005u16.to_le_bytes());
        stream.extend_from_slice(&biff_record(0x0809, &bof_globals));

        // BoundSheet8 (0x0085): 4-byte placeholder offset, 2-byte flags (0), ShortXLUnicodeString
        let name_bytes = sheet_name.as_bytes();
        let mut boundsheet = Vec::new();
        boundsheet.extend_from_slice(&0u32.to_le_bytes()); // patched below
        boundsheet.extend_from_slice(&0u16.to_le_bytes()); // visible worksheet
        boundsheet.push(name_bytes.len() as u8);
        boundsheet.push(0); // compressed 8-bit ASCII
        boundsheet.extend_from_slice(name_bytes);
        let bs_pos = stream.len();
        stream.extend_from_slice(&biff_record(0x0085, &boundsheet));

        // Workbook globals EOF (0x000A)
        stream.extend_from_slice(&biff_record(0x000A, &[]));

        // Patch BoundSheet8 stream offset to point here
        let sheet_offset = stream.len() as u32;
        stream[bs_pos + 4..bs_pos + 8].copy_from_slice(&sheet_offset.to_le_bytes());

        // Worksheet BOF (BIFF8 = 0x0600, dt = 0x0010 worksheet)
        let mut bof_sheet = vec![0u8; 16];
        bof_sheet[0..2].copy_from_slice(&0x0600u16.to_le_bytes());
        bof_sheet[2..4].copy_from_slice(&0x0010u16.to_le_bytes());
        stream.extend_from_slice(&biff_record(0x0809, &bof_sheet));

        // Dimensions (0x0200): r_first=0, r_last=1, c_first=0, c_last=1, reserved=0
        let mut dims = Vec::new();
        dims.extend_from_slice(&0u32.to_le_bytes());
        dims.extend_from_slice(&1u32.to_le_bytes());
        dims.extend_from_slice(&0u16.to_le_bytes());
        dims.extend_from_slice(&1u16.to_le_bytes());
        dims.extend_from_slice(&0u16.to_le_bytes());
        stream.extend_from_slice(&biff_record(0x0200, &dims));

        // Label (0x0204) at row 0, col 0
        let val_bytes = cell_a1.as_bytes();
        let mut label = Vec::new();
        label.extend_from_slice(&0u16.to_le_bytes()); // row 0
        label.extend_from_slice(&0u16.to_le_bytes()); // col 0
        label.extend_from_slice(&0u16.to_le_bytes()); // xf index
        label.extend_from_slice(&(val_bytes.len() as u16).to_le_bytes());
        label.push(0); // compressed 8-bit ASCII
        label.extend_from_slice(val_bytes);
        stream.extend_from_slice(&biff_record(0x0204, &label));

        // Worksheet EOF (0x000A)
        stream.extend_from_slice(&biff_record(0x000A, &[]));

        build_ole2_single_stream("Workbook", &stream)
    }

    fn build_pdf_with_lines(lines: &[&str], meta_title: Option<&str>) -> Vec<u8> {
        let pdfium = pdf::pdfium().expect("pdfium available");
        let mut doc = pdfium.create_new_pdf().unwrap();
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .unwrap();
        let font = doc.fonts_mut().helvetica();
        for (idx, &line) in lines.iter().enumerate() {
            let y = 750.0 - (idx as f32) * 24.0;
            let mut obj = PdfPageTextObject::new(&doc, line, font, PdfPoints::new(14.0)).unwrap();
            obj.translate(PdfPoints::new(50.0), PdfPoints::new(y))
                .unwrap();
            page.objects_mut().add_text_object(obj).unwrap();
        }
        let mut bytes = doc.save_to_bytes().unwrap();
        if let Some(title) = meta_title {
            inject_pdf_title(&mut bytes, title);
        }
        if !bytes[..bytes.len().min(MAX_BINARY_CHECK_BYTES)].contains(&0) {
            let insert_at = bytes
                .windows(2)
                .position(|w| w == b"\n%")
                .map(|p| p + 1)
                .unwrap_or(9);
            let comment_with_nul = b"%\x00\x00binary-pdf-comment\n";
            bytes.splice(insert_at..insert_at, comment_with_nul.iter().copied());
        }
        bytes
    }

    fn inject_pdf_title(pdf: &mut Vec<u8>, title: &str) {
        let text = String::from_utf8_lossy(pdf).into_owned();
        let Some(startxref_pos) = text.rfind("startxref") else {
            return;
        };
        let old_xref_offset: usize = text[startxref_pos + "startxref".len()..]
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()))
            .and_then(|l| l.parse().ok())
            .unwrap_or(0);
        if old_xref_offset == 0 {
            return;
        }
        let root_ref = text
            .find("/Root ")
            .and_then(|pos| {
                let after = &text[pos + 6..];
                let end = after.find('R')?;
                Some(after[..=end].trim().to_string())
            })
            .unwrap_or_else(|| "1 0 R".to_string());
        let info_obj_num = 99;
        let obj_offset = pdf.len();
        let info_obj = format!("{info_obj_num} 0 obj\n<< /Title ({title}) >>\nendobj\n");
        pdf.extend_from_slice(info_obj.as_bytes());
        let xref_offset = pdf.len();
        let xref = format!(
            "xref\n{info_obj_num} 1\n{:010} 00000 n \ntrailer\n<< /Size {} /Root {root_ref} /Info {info_obj_num} 0 R /Prev {old_xref_offset} >>\nstartxref\n{xref_offset}\n%%EOF\n",
            obj_offset,
            info_obj_num + 1
        );
        pdf.extend_from_slice(xref.as_bytes());
    }

    #[test]
    fn titles_per_format() {
        let limits = Limits::default();

        // Markdown: front-matter title wins over H1
        let md_fm = b"---\ntitle: \"Front Matter Title\"\n---\n# Ignored H1\nBody text.\n";
        let ft = detect("docs/guide.md", md_fm).unwrap();
        let out = extract("docs/guide.md", ft, md_fm, &limits).unwrap();
        assert_eq!(out.title, "Front Matter Title");

        // Markdown: falls back to first H1
        let md_h1 = b"Intro paragraph\n\n# First Heading\nMore text.\n";
        let ft = detect("docs/h1.md", md_h1).unwrap();
        let out = extract("docs/h1.md", ft, md_h1, &limits).unwrap();
        assert_eq!(out.title, "First Heading");

        // Markdown: Setext H1
        let md_setext = b"Setext Heading Title\n====================\n\nBody paragraph.\n";
        let ft = detect("docs/setext.md", md_setext).unwrap();
        let out = extract("docs/setext.md", ft, md_setext, &limits).unwrap();
        assert_eq!(out.title, "Setext Heading Title");

        // Markdown: ATX closing hashes only stripped when preceded by whitespace (# C# -> C#)
        let md_csharp = b"# C#\n\nC# language notes.\n";
        let out = extract("docs/csharp.md", FileType::Markdown, md_csharp, &limits).unwrap();
        assert_eq!(out.title, "C#");
        let md_csharp_closed = b"# C# ###\n\nC# language notes.\n";
        let out = extract(
            "docs/csharp.md",
            FileType::Markdown,
            md_csharp_closed,
            &limits,
        )
        .unwrap();
        assert_eq!(out.title, "C#");

        // Markdown: YAML front-matter block scalars (| and >) and empty block scalar fallback
        let md_block_lit =
            b"---\ntitle: |\n  Literal Block\n  Scalar Title\n---\n# Ignored H1\nBody.\n";
        let out = extract("docs/block.md", FileType::Markdown, md_block_lit, &limits).unwrap();
        assert_eq!(out.title, "Literal Block Scalar Title");

        let md_block_fold = b"---\ntitle: >-\n  Folded Block\n  Title\n---\n# Ignored H1\nBody.\n";
        let out = extract("docs/fold.md", FileType::Markdown, md_block_fold, &limits).unwrap();
        assert_eq!(out.title, "Folded Block Title");

        let md_block_empty = b"---\ntitle: |\nauthor: Alice\n---\n# Fallback H1\nBody.\n";
        let out = extract(
            "docs/empty_block.md",
            FileType::Markdown,
            md_block_empty,
            &limits,
        )
        .unwrap();
        assert_eq!(out.title, "Fallback H1");

        // HTML: <title> only from <head>, ignoring SVG <title> in body and script/style/noscript
        let html_bytes = br#"<!doctype html>
<html>
  <head>
    <title>  HTML   Page Title </title>
    <style>body { color: red; }</style>
  </head>
  <body>
    <script>console.log("secret script");</script>
    <noscript>noscript fallback</noscript>
    <p>Visible paragraph content.</p>
  </body>
</html>"#;
        let ft = detect("site/index.html", html_bytes).unwrap();
        let out = extract("site/index.html", ft, html_bytes, &limits).unwrap();
        assert_eq!(out.title, "HTML Page Title");
        assert_eq!(out.segments.len(), 1);
        assert!(out.segments[0].text.contains("Visible paragraph content."));
        assert!(!out.segments[0].text.contains("secret script"));
        assert!(!out.segments[0].text.contains("noscript fallback"));
        assert!(!out.segments[0].text.contains("color: red"));

        let html_svg_body_title = br#"<!doctype html>
<html>
  <head></head>
  <body>
    <svg><title>SVG Icon Title</title></svg>
    <p>Body text</p>
  </body>
</html>"#;
        let out = extract(
            "site/icon_page.html",
            FileType::Html,
            html_svg_body_title,
            &limits,
        )
        .unwrap();
        assert_eq!(out.title, "icon_page.html");

        // DOCX: dc:title else first heading
        let docx_meta = build_docx(Some("Docx Meta Title"), Some("Docx H1"), "Hello docx.");
        let ft = detect("notes/spec.docx", &docx_meta).unwrap();
        let out = extract("notes/spec.docx", ft, &docx_meta, &limits).unwrap();
        assert_eq!(out.title, "Docx Meta Title");

        let docx_h1 = build_docx(None, Some("Docx Heading Title"), "Hello docx.");
        let out = extract("notes/spec.docx", FileType::Docx, &docx_h1, &limits).unwrap();
        assert_eq!(out.title, "Docx Heading Title");

        // ODT: dc:title else first heading
        let odt_meta = build_odt(Some("Odt Meta Title"), Some("Odt H1"), "Hello odt.");
        let ft = detect("notes/spec.odt", &odt_meta).unwrap();
        let out = extract("notes/spec.odt", ft, &odt_meta, &limits).unwrap();
        assert_eq!(out.title, "Odt Meta Title");

        let odt_h1 = build_odt(None, Some("Odt Heading Title"), "Hello odt.");
        let out = extract("notes/spec.odt", FileType::Odt, &odt_h1, &limits).unwrap();
        assert_eq!(out.title, "Odt Heading Title");

        // PDF: Title metadata else first text line, and one Segment per page
        let pdf_meta =
            build_pdf_with_lines(&["First Page Line", "Second Line"], Some("PDF Meta Title"));
        let ft = detect("papers/doc.pdf", &pdf_meta).unwrap();
        let out = extract("papers/doc.pdf", ft, &pdf_meta, &limits).unwrap();
        assert_eq!(out.title, "PDF Meta Title");
        assert_eq!(out.segments.len(), 1);
        assert_eq!(out.segments[0].kind, SegmentKind::Page(1));

        let pdf_line = build_pdf_with_lines(&["First Text Line Fallback", "Body line"], None);
        let out = extract("papers/doc.pdf", FileType::Pdf, &pdf_line, &limits).unwrap();
        assert_eq!(out.title, "First Text Line Fallback");

        // Sheets (.xlsx, .ods, .xls), CSV, plain text, and code: file name
        let xlsx = build_xlsx("Budget", "Item", "Cost");
        let ft = detect("finance/q1_budget.xlsx", &xlsx).unwrap();
        let out = extract("finance/q1_budget.xlsx", ft, &xlsx, &limits).unwrap();
        assert_eq!(out.title, "q1_budget.xlsx");

        let ods = build_ods("Sheet1", "Alpha", "Beta");
        let ft = detect("finance/annual.ods", &ods).unwrap();
        let out = extract("finance/annual.ods", ft, &ods, &limits).unwrap();
        assert_eq!(out.title, "annual.ods");
        assert_eq!(out.segments.len(), 1);
        assert!(out.segments[0].text.contains("Alpha\tBeta"));

        let xls = build_xls("Sheet1", "LegacyCell");
        let ft = detect("finance/legacy.xls", &xls).unwrap();
        let out = extract("finance/legacy.xls", ft, &xls, &limits).unwrap();
        assert_eq!(out.title, "legacy.xls");
        assert_eq!(out.segments.len(), 1);
        assert!(out.segments[0].text.contains("LegacyCell"));

        let txt_bytes = b"Plain text notes.\nSecond line.\n";
        let ft = detect("notes/todo.txt", txt_bytes).unwrap();
        let out = extract("notes/todo.txt", ft, txt_bytes, &limits).unwrap();
        assert_eq!(out.title, "todo.txt");

        let csv_bytes = b"name,amount\nalice,42\n";
        let ft = detect("finance/report.csv", csv_bytes).unwrap();
        let out = extract("finance/report.csv", ft, csv_bytes, &limits).unwrap();
        assert_eq!(out.title, "report.csv");

        let code_bytes = b"pub fn answer() -> u32 {\n    42\n}\n";
        let ft = detect("src/lib.rs", code_bytes).unwrap();
        assert_eq!(ft, FileType::Code("rust"));
        let out = extract("src/lib.rs", ft, code_bytes, &limits).unwrap();
        assert_eq!(out.title, "lib.rs");
    }

    #[test]
    fn docx_subtitle_and_outline_level_9_ignored_and_fake_ns_title_ignored() {
        let limits = Limits::default();

        let content_types = br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
        let fake_core = br#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:fake="http://example.com/not-dc">
  <fake:title>Fake Namespace Title</fake:title>
</cp:coreProperties>"#;
        let doc_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:pPr><w:pStyle w:val="Subtitle"/></w:pPr><w:r><w:t>Subtitle Before Heading</w:t></w:r></w:p>
    <w:p><w:pPr><w:outlineLvl w:val="9"/></w:pPr><w:r><w:t>Body With OutlineLvl 9</w:t></w:r></w:p>
    <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Actual Heading 1</w:t></w:r></w:p>
    <w:p><w:r><w:t>Paragraph body.</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        let docx = build_zip(&[
            (
                "[Content_Types].xml",
                content_types,
                CompressionMethod::Deflated,
            ),
            ("docProps/core.xml", fake_core, CompressionMethod::Deflated),
            ("word/document.xml", doc_xml, CompressionMethod::Deflated),
        ]);

        let out = extract("report.docx", FileType::Docx, &docx, &limits).unwrap();
        assert_eq!(out.title, "Actual Heading 1");

        // ODT with fake:title in meta.xml also ignores fake namespace and falls back to heading
        let fake_odt_meta = br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:fake="http://example.com/not-dc">
  <office:meta><fake:title>Fake ODT Title</fake:title></office:meta>
</office:document-meta>"#;
        let odt_content = br#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:text>
    <text:h text:outline-level="1">Real ODT Heading</text:h>
    <text:p>Body</text:p>
  </office:text></office:body>
</office:document-content>"#;
        let odt = build_zip(&[
            (
                "mimetype",
                ODF_ODT_MIME.as_bytes(),
                CompressionMethod::Stored,
            ),
            ("meta.xml", fake_odt_meta, CompressionMethod::Deflated),
            ("content.xml", odt_content, CompressionMethod::Deflated),
        ]);
        let out = extract("notes.odt", FileType::Odt, &odt, &limits).unwrap();
        assert_eq!(out.title, "Real ODT Heading");
    }

    #[test]
    fn office_and_pdf_with_nul_bytes_are_indexed() {
        let limits = Limits::default();

        let docx = build_docx(Some("Contract"), None, "Clause 1: obligations.");
        assert!(docx[..docx.len().min(MAX_BINARY_CHECK_BYTES)].contains(&0));
        let ft = detect("legal/contract.docx", &docx).unwrap();
        let out = extract("legal/contract.docx", ft, &docx, &limits).unwrap();
        assert!(out.segments[0].text.contains("Clause 1: obligations."));

        let xlsx = build_xlsx("Data", "Revenue", "12345");
        assert!(xlsx[..xlsx.len().min(MAX_BINARY_CHECK_BYTES)].contains(&0));
        let ft = detect("legal/numbers.xlsx", &xlsx).unwrap();
        let out = extract("legal/numbers.xlsx", ft, &xlsx, &limits).unwrap();
        assert!(out.segments[0].text.contains("Revenue"));

        let pdf = build_pdf_with_lines(&["PDF with embedded NUL bytes"], None);
        assert!(pdf[..pdf.len().min(MAX_BINARY_CHECK_BYTES)].contains(&0));
        let ft = detect("legal/scan.pdf", &pdf).unwrap();
        let out = extract("legal/scan.pdf", ft, &pdf, &limits).unwrap();
        assert_eq!(out.segments.len(), 1);
        assert!(out.segments[0].text.contains("PDF with embedded NUL bytes"));
    }

    #[test]
    fn magic_mismatch_and_renamed_containers_rejected() {
        let limits = Limits::default();

        assert_eq!(
            detect("fake.docx", b"not a zip archive at all"),
            Err(Skip::Mismatch)
        );
        let wrong_zip = build_zip(&[("hello.txt", b"hello", CompressionMethod::Stored)]);
        assert_eq!(detect("fake.docx", &wrong_zip), Err(Skip::Mismatch));

        // Renamed ODS as .odt -> Mismatch (and vice versa)
        let ods = build_ods("Sheet1", "A", "B");
        assert_eq!(detect("renamed.odt", &ods), Err(Skip::Mismatch));
        assert_eq!(
            extract("renamed.odt", FileType::Odt, &ods, &limits),
            Err(ExtractError::Skipped(Skip::Mismatch))
        );

        let odt = build_odt(Some("Doc"), None, "Body");
        assert_eq!(detect("renamed.ods", &odt), Err(Skip::Mismatch));
        assert_eq!(
            extract("renamed.ods", FileType::Ods, &odt, &limits),
            Err(ExtractError::Skipped(Skip::Mismatch))
        );

        // Renamed XLSX as .docx and DOCX as .xlsx -> Mismatch
        let xlsx = build_xlsx("Sheet1", "1", "2");
        assert_eq!(detect("renamed.docx", &xlsx), Err(Skip::Mismatch));
        assert_eq!(
            extract("renamed.docx", FileType::Docx, &xlsx, &limits),
            Err(ExtractError::Skipped(Skip::Mismatch))
        );

        let docx = build_docx(Some("Title"), None, "Body");
        assert_eq!(detect("renamed.xlsx", &docx), Err(Skip::Mismatch));
        assert_eq!(
            extract("renamed.xlsx", FileType::Xlsx, &docx, &limits),
            Err(ExtractError::Skipped(Skip::Mismatch))
        );

        // OLE2 requires all 8 bytes D0 CF 11 E0 A1 B1 1A E1
        assert_eq!(
            detect("half_magic.xls", &[0xD0, 0xCF, 0x11, 0xE0, 0, 0, 0, 0]),
            Err(Skip::Mismatch)
        );
        assert_eq!(detect("fake.pdf", b"not a pdf"), Err(Skip::Mismatch));

        // .doc renamed .xls (valid 8-byte OLE2 container, but WordDocument stream instead of Workbook)
        // -> Failed (ExtractError::Parse), not abort
        let doc_as_xls = build_ole2_single_stream("WordDocument", b"not an excel biff stream");
        assert_eq!(detect("renamed.xls", &doc_as_xls), Ok(FileType::Xls));
        let err = extract("renamed.xls", FileType::Xls, &doc_as_xls, &limits).unwrap_err();
        assert!(matches!(err, ExtractError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn xlsx_sparse_a1_xfd1048576_and_shared_string_amplification_rejected_without_abort() {
        let mut limits = Limits::default();

        // 1. Sparse A1 + XFD1048576 with declared <dimension ref="A1:XFD1048576"/>
        let sparse_declared = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:XFD1048576"/>
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>start</t></is></c></row>
    <row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>end</t></is></c></row>
  </sheetData>
</worksheet>"#;
        let xlsx_declared = build_xlsx_with_raw_sheet("Sparse", sparse_declared, None);
        let err = extract("sparse1.xlsx", FileType::Xlsx, &xlsx_declared, &limits).unwrap_err();
        assert_eq!(err, ExtractError::TooLarge);

        // 2. Sparse A1 + XFD1048576 with spoofed <dimension ref="A1:A1"/>
        let sparse_spoofed = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <dimension ref="A1:A1"/>
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>start</t></is></c></row>
    <row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>end</t></is></c></row>
  </sheetData>
</worksheet>"#;
        let xlsx_spoofed = build_xlsx_with_raw_sheet("Sparse", sparse_spoofed, None);
        let err = extract("sparse2.xlsx", FileType::Xlsx, &xlsx_spoofed, &limits).unwrap_err();
        assert_eq!(err, ExtractError::TooLarge);

        // 3. Shared-string amplification exceeding max_text_bytes
        limits.max_text_bytes = 32 * 1024;
        let shared_payload = "S".repeat(4096);
        let sst = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
  <si><t>{shared_payload}</t></si>
</sst>"#
        );
        let mut cells_xml = String::new();
        for row in 1..=20 {
            cells_xml.push_str(&format!(
                r#"<row r="{row}"><c r="A{row}" t="s"><v>0</v></c></row>"#
            ));
        }
        let sheet_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>{cells_xml}</sheetData>
</worksheet>"#
        );
        let amp_xlsx = build_xlsx_with_raw_sheet("Amp", &sheet_xml, Some(&sst));
        let err = extract("amp.xlsx", FileType::Xlsx, &amp_xlsx, &limits).unwrap_err();
        assert_eq!(err, ExtractError::TooLarge);
    }

    #[test]
    fn corrupt_xlsx_worksheet_returns_error_not_empty() {
        let limits = Limits::default();
        let corrupt_sheet = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>unclosed"#;
        let xlsx = build_xlsx_with_raw_sheet("Broken", corrupt_sheet, None);
        let err = extract("broken.xlsx", FileType::Xlsx, &xlsx, &limits).unwrap_err();
        assert!(matches!(err, ExtractError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn odt_repeated_space_runs_exceeding_max_text_bytes_rejected() {
        let limits = Limits {
            max_text_bytes: 16 * 1024,
            ..Limits::default()
        };

        let spaces_tags = r#"<text:s text:c="256"/>"#.repeat(100);
        let content_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:text>
    <text:p>start{spaces_tags}end</text:p>
  </office:text></office:body>
</office:document-content>"#
        );
        let odt = build_zip(&[
            (
                "mimetype",
                ODF_ODT_MIME.as_bytes(),
                CompressionMethod::Stored,
            ),
            (
                "content.xml",
                content_xml.as_bytes(),
                CompressionMethod::Deflated,
            ),
        ]);
        let err = extract("spaces.odt", FileType::Odt, &odt, &limits).unwrap_err();
        assert_eq!(err, ExtractError::TooLarge);
    }

    #[test]
    fn html_pruner_not_fooled_by_self_closing_or_stray_close_tags() {
        let limits = Limits::default();
        for unit in ["<div/>", "<div></b>"] {
            let html = format!(
                "<html><body><p>Top</p>{}x</body></html>",
                unit.repeat(80_000)
            );
            let start = Instant::now();
            extract("deep.html", FileType::Html, html.as_bytes(), &limits).unwrap();
            assert!(
                start.elapsed().as_secs() < 5,
                "{unit} x80000 took {:?}",
                start.elapsed()
            );
        }
    }

    #[test]
    fn deeply_nested_80k_html_depth_capped_without_stack_overflow() {
        let limits = Limits::default();
        let depth = 80_000;
        let mut html = String::with_capacity(depth * 11 + 128);
        html.push_str(
            "<!doctype html><html><head><title>Deep</title></head><body><p>TopVisible</p>",
        );
        for _ in 0..depth {
            html.push_str("<div>");
        }
        html.push_str("TooDeepHidden");
        for _ in 0..depth {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");

        let start = Instant::now();
        let out = extract("deep.html", FileType::Html, html.as_bytes(), &limits).unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_secs() < 5,
            "80k-deep HTML took too long: {elapsed:?}"
        );
        assert_eq!(out.title, "Deep");
        assert_eq!(out.segments.len(), 1);
        assert!(out.segments[0].text.contains("TopVisible"));
        assert!(!out.segments[0].text.contains("TooDeepHidden"));
    }

    #[test]
    fn pdf_page_cap_and_fail_closed_binding() {
        let limits = Limits::default();
        assert_eq!(pdf::check_page_count(pdf::MAX_PDF_PAGES), Ok(()));
        assert_eq!(
            pdf::check_page_count(pdf::MAX_PDF_PAGES + 1),
            Err(ExtractError::TooLarge)
        );

        // Generate a minimal valid PDF with 5,001 pages and verify pdf::extract rejects it
        // with ExtractError::TooLarge.
        let page_count = pdf::MAX_PDF_PAGES + 1;
        let mut kids = String::with_capacity(page_count * 6);
        for _ in 0..page_count {
            kids.push_str("3 0 R ");
        }
        let mut pdf_bytes = Vec::new();
        pdf_bytes.extend_from_slice(b"%PDF-1.4\n");
        let off1 = pdf_bytes.len();
        pdf_bytes.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off2 = pdf_bytes.len();
        pdf_bytes.extend_from_slice(
            format!("2 0 obj\n<< /Type /Pages /Kids [ {kids}] /Count {page_count} >>\nendobj\n")
                .as_bytes(),
        );
        let off3 = pdf_bytes.len();
        pdf_bytes.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n",
        );
        let xref_off = pdf_bytes.len();
        pdf_bytes.extend_from_slice(
            format!(
                "xref\n0 4\n0000000000 65535 f \n{off1:010} 00000 n \n{off2:010} 00000 n \n{off3:010} 00000 n \ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref_off}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let err = extract("huge_pages.pdf", FileType::Pdf, &pdf_bytes, &limits).unwrap_err();
        assert_eq!(err, ExtractError::TooLarge);

        // Verify bind_pdfium_at fails closed when given a nonexistent library path
        // (never falling back to bind_to_system_library).
        let err = pdf::bind_pdfium_at("/nonexistent/libpdfium.so").unwrap_err();
        assert!(matches!(err, ExtractError::Parse(_)));
    }

    #[test]
    fn long_line_html_indexed_and_minified_js_skipped() {
        let limits = Limits::default();
        let long_body = "word ".repeat(200);
        let html =
            format!("<html><head><title>Long</title></head><body><p>{long_body}</p></body></html>");
        assert!(html.len() > 800 && html.lines().count() == 1);
        let ft = detect("page.html", html.as_bytes()).unwrap();
        let out = extract("page.html", ft, html.as_bytes(), &limits).unwrap();
        assert_eq!(out.title, "Long");
        assert!(!out.segments.is_empty());

        let minified_js = format!("var a=1;{}", "b=2;".repeat(100));
        assert_eq!(
            detect("bundle.min.js", minified_js.as_bytes()),
            Err(Skip::Minified)
        );
        assert_eq!(
            detect("styles.css", "body{margin:0;}".repeat(30).as_bytes()),
            Err(Skip::Minified)
        );
    }

    #[test]
    fn generated_files_skipped() {
        let gen1 = b"// @generated by tool\npub fn foo() {}\n";
        assert_eq!(detect("src/gen.rs", gen1), Err(Skip::Generated));

        let gen2 = b"// Code generated by protoc-gen-go. DO NOT EDIT.\npackage pb\n";
        assert_eq!(detect("pb/msg.pb.go", gen2), Err(Skip::Generated));

        // Binary plain text with NUL in first 8 KB is skipped as Binary
        let bin_txt = b"hello\x00world\n";
        assert_eq!(detect("notes.txt", bin_txt), Err(Skip::Binary));
    }

    #[test]
    fn zip_bomb_rejected() {
        let limits = Limits::default();
        // A highly compressible payload whose uncompressed size exceeds 100x the ZIP archive size.
        let huge_zeros = vec![b'A'; 500_123];
        let content_types = br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
        let bomb_zip = build_zip(&[
            (
                "[Content_Types].xml",
                content_types,
                CompressionMethod::Deflated,
            ),
            (
                "word/document.xml",
                &huge_zeros,
                CompressionMethod::Deflated,
            ),
        ]);
        // Confirm ratio really is > 100x (caught by declared size check)
        assert!((huge_zeros.len() as u64) > (bomb_zip.len() as u64) * 100);
        let err = extract("bomb.docx", FileType::Docx, &bomb_zip, &limits).unwrap_err();
        assert_eq!(err, ExtractError::ZipBomb);

        // Forge the declared uncompressed size in both local and central directory headers
        // so that declared size is tiny (64 bytes), reaching the `actual_total > budget` branch
        // in `verify_zip_guard`.
        let mut spoofed_zip = bomb_zip.clone();
        let orig_len = (huge_zeros.len() as u32).to_le_bytes();
        let fake_len = 64u32.to_le_bytes();
        for i in 0..spoofed_zip.len().saturating_sub(4) {
            if spoofed_zip[i..i + 4] == orig_len {
                spoofed_zip[i..i + 4].copy_from_slice(&fake_len);
            }
        }
        let err = extract("spoofed.docx", FileType::Docx, &spoofed_zip, &limits).unwrap_err();
        assert_eq!(err, ExtractError::ZipBomb);
    }
}
