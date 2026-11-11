use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Cursor, Read};
use std::path::Path;

use zip::ZipArchive;

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
const OLE2_MAGIC: &[u8; 4] = &[0xD0, 0xCF, 0x11, 0xE0];

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

impl skip_fmt::Display for Skip {
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

use std::fmt as skip_fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtractError {
    Skipped(Skip),
    ZipBomb,
    Parse(String),
}

impl fmt::Display for ExtractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skipped(skip) => write!(f, "skipped: {skip}"),
            Self::ZipBomb => f.write_str("zip decompression limit exceeded"),
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
            if has_zip_member(head, "[Content_Types].xml") {
                return Ok(FileType::Docx);
            }
            return Err(Skip::Mismatch);
        }
        Some("xlsx") => {
            if has_zip_member(head, "[Content_Types].xml") {
                return Ok(FileType::Xlsx);
            }
            return Err(Skip::Mismatch);
        }
        Some("odt") => {
            if has_zip_member(head, "mimetype") {
                return Ok(FileType::Odt);
            }
            return Err(Skip::Mismatch);
        }
        Some("ods") => {
            if has_zip_member(head, "mimetype") {
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
        FileType::Pdf => pdf::extract(relpath, bytes),
        FileType::Docx => docx::extract(relpath, bytes),
        FileType::Odt => odt::extract(relpath, bytes),
        FileType::Xlsx | FileType::Ods | FileType::Xls => sheet::extract(relpath, file_type, bytes),
        FileType::Html => {
            let s = check_utf8_and_nul(bytes, false)?;
            Ok(html::extract(relpath, s))
        }
        FileType::Markdown => {
            let s = check_text_heuristics(relpath, file_type, bytes, false)?;
            Ok(markdown::extract(relpath, s))
        }
        FileType::Text => {
            let s = check_text_heuristics(relpath, file_type, bytes, false)?;
            Ok(text::extract(relpath, s))
        }
        FileType::Csv => {
            check_text_heuristics(relpath, file_type, bytes, false)?;
            csv::extract(relpath, bytes)
        }
        FileType::Code(lang) => {
            let s = check_text_heuristics(relpath, file_type, bytes, false)?;
            Ok(code::extract(relpath, lang, s))
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

fn has_zip_member(head: &[u8], expected: &str) -> bool {
    if !head.starts_with(b"PK\x03\x04") {
        return false;
    }
    if let Ok(mut archive) = ZipArchive::new(Cursor::new(head)) {
        return archive.by_name(expected).is_ok();
    }
    scan_local_zip_headers(head, expected.as_bytes())
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

pub(crate) fn verify_zip_guard(bytes: &[u8], expected_member: &str) -> Result<(), ExtractError> {
    if !bytes.starts_with(b"PK\x03\x04") {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|_| ExtractError::Skipped(Skip::Mismatch))?;
    if archive.by_name(expected_member).is_err() {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }

    let budget = zip_budget(bytes.len());
    let mut declared_total: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| ExtractError::Parse(format!("zip header error: {e}")))?;
        declared_total = declared_total.saturating_add(entry.size());
        if declared_total > budget {
            return Err(ExtractError::ZipBomb);
        }
    }

    let mut actual_total: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| ExtractError::Parse(format!("zip entry error: {e}")))?;
        let remaining = budget.saturating_sub(actual_total);
        let mut limited = entry.take(remaining + 1);
        let copied = io::copy(&mut limited, &mut io::sink()).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("larger than its declared uncompressed size") {
                ExtractError::ZipBomb
            } else {
                ExtractError::Parse(format!("zip read error: {e}"))
            }
        })?;
        actual_total = actual_total.saturating_add(copied);
        if actual_total > budget {
            return Err(ExtractError::ZipBomb);
        }
    }

    Ok(())
}

pub(crate) fn read_zip_entries(
    bytes: &[u8],
    expected_member: &str,
    wanted: &[&str],
) -> Result<HashMap<String, String>, ExtractError> {
    verify_zip_guard(bytes, expected_member)?;
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|_| ExtractError::Skipped(Skip::Mismatch))?;
    let budget = zip_budget(bytes.len());
    let mut read_so_far: u64 = 0;
    let mut out = HashMap::new();

    for &name in wanted {
        let Ok(entry) = archive.by_name(name) else {
            continue;
        };
        let remaining = budget.saturating_sub(read_so_far);
        let mut buf = Vec::new();
        let n = entry
            .take(remaining + 1)
            .read_to_end(&mut buf)
            .map_err(|e| ExtractError::Parse(format!("zip read {name}: {e}")))?
            as u64;
        read_so_far = read_so_far.saturating_add(n);
        if read_so_far > budget {
            return Err(ExtractError::ZipBomb);
        }
        let s = String::from_utf8(buf)
            .map_err(|e| ExtractError::Parse(format!("invalid utf-8 in {name}: {e}")))?;
        out.insert(name.to_string(), s);
    }

    Ok(out)
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
        let mimetype = b"application/vnd.oasis.opendocument.text";
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

    fn build_xlsx(sheet_name: &str, cell_a1: &str, cell_b1: &str) -> Vec<u8> {
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
</Relationships>"#;
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
        build_zip(&[
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
                sheet1.as_bytes(),
                CompressionMethod::Deflated,
            ),
        ])
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
        // Ensure the PDF binary contains NUL bytes in its first 8 KB to test that
        // PDFs bypass the plain-text NUL heuristic.
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
        // Attach a standard /Info dictionary with /Title to the trailer if not present,
        // or rewrite via incremental update so pdfium reads `PdfDocumentMetadataTagType::Title`.
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

        // HTML: <title>, skipping script/style/noscript
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

        // Sheets, CSV, and code: file name
        let xlsx = build_xlsx("Budget", "Item", "Cost");
        let ft = detect("finance/q1_budget.xlsx", &xlsx).unwrap();
        let out = extract("finance/q1_budget.xlsx", ft, &xlsx, &limits).unwrap();
        assert_eq!(out.title, "q1_budget.xlsx");

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
    fn magic_mismatch_rejected() {
        assert_eq!(
            detect("fake.docx", b"not a zip archive at all"),
            Err(Skip::Mismatch)
        );
        // A ZIP without [Content_Types].xml is also a mismatch for .docx
        let wrong_zip = build_zip(&[("hello.txt", b"hello", CompressionMethod::Stored)]);
        assert_eq!(detect("fake.docx", &wrong_zip), Err(Skip::Mismatch));

        // A .xls without OLE2 magic D0 CF 11 E0 -> Mismatch
        assert_eq!(
            detect("fake.xls", b"plain text pretending to be xls"),
            Err(Skip::Mismatch)
        );
        assert_eq!(detect("fake.pdf", b"not a pdf"), Err(Skip::Mismatch));
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
        let content_types = br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#;
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
        // so that declared size is tiny (64 bytes), testing that actual decompressed bytes
        // read is also bounded and stops immediately when exceeded.
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
