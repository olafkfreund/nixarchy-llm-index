use std::sync::OnceLock;

use pdfium_render::prelude::*;

use super::{
    ExtractError, Extracted, FileType, Segment, SegmentKind, Skip, TextBudget, file_name,
    line_count,
};

pub const MAX_PDF_PAGES: usize = 5_000;

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

pub(crate) fn bind_pdfium_at(path: &str) -> Result<Pdfium, ExtractError> {
    let bindings = Pdfium::bind_to_library(path)
        .map_err(|e| ExtractError::Parse(format!("failed to bind pdfium ({path}): {e}")))?;
    Ok(Pdfium::new(bindings))
}

pub(crate) fn pdfium() -> Result<&'static Pdfium, ExtractError> {
    PDFIUM
        .get_or_init(|| bind_pdfium_at(crate::pdfium_lib()).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|msg| ExtractError::Parse(msg.clone()))
}

pub(crate) fn check_page_count(page_count: usize) -> Result<(), ExtractError> {
    if page_count > MAX_PDF_PAGES {
        return Err(ExtractError::TooLarge);
    }
    Ok(())
}

pub fn extract(
    relpath: &str,
    bytes: &[u8],
    max_text_bytes: usize,
) -> Result<Extracted, ExtractError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    let pdfium = pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| ExtractError::Parse(format!("pdf load error: {e}")))?;

    check_page_count(doc.pages().len() as usize)?;

    let meta_title = doc
        .metadata()
        .get(PdfDocumentMetadataTagType::Title)
        .map(|tag| tag.value().trim().to_string())
        .filter(|s| !s.is_empty());

    let mut budget = TextBudget::new(max_text_bytes);
    let mut first_line: Option<String> = None;
    let mut segments = Vec::new();

    for (idx, page) in doc.pages().iter().enumerate() {
        check_page_count(idx + 1)?;
        let page_num = (idx + 1) as u32;
        let raw_text = page
            .text()
            .map_err(|e| ExtractError::Parse(format!("pdf page {page_num} text error: {e}")))?
            .all();
        let normalized = normalize_page_text(&raw_text);
        let mut text = String::new();
        budget.push_str(&mut text, &normalized)?;
        if first_line.is_none() {
            first_line = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(ToOwned::to_owned);
        }
        segments.push(Segment {
            kind: SegmentKind::Page(page_num),
            start_line: 1,
            end_line: line_count(&text),
            text,
        });
    }

    let title = meta_title
        .or(first_line)
        .unwrap_or_else(|| file_name(relpath));

    Ok(Extracted {
        title,
        file_type: FileType::Pdf,
        segments,
    })
}

fn normalize_page_text(raw: &str) -> String {
    raw.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}
