use std::sync::OnceLock;

use pdfium_render::prelude::*;

use super::{ExtractError, Extracted, FileType, Segment, SegmentKind, Skip, file_name, line_count};

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

pub(crate) fn pdfium() -> Result<&'static Pdfium, ExtractError> {
    PDFIUM
        .get_or_init(|| {
            let compiled = crate::pdfium_lib();
            let env_var = std::env::var("LLM_INDEX_PDFIUM_LIB").ok();
            let candidate = if compiled != "unset" {
                Some(compiled)
            } else {
                env_var.as_deref()
            };
            let bindings = match candidate {
                Some(path) => {
                    Pdfium::bind_to_library(path).or_else(|_| Pdfium::bind_to_system_library())
                }
                None => Pdfium::bind_to_system_library(),
            }
            .map_err(|e| format!("failed to bind pdfium: {e}"))?;
            Ok(Pdfium::new(bindings))
        })
        .as_ref()
        .map_err(|msg| ExtractError::Parse(msg.clone()))
}

pub fn extract(relpath: &str, bytes: &[u8]) -> Result<Extracted, ExtractError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    }
    let pdfium = pdfium()?;
    let doc = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|e| ExtractError::Parse(format!("pdf load error: {e}")))?;

    let meta_title = doc
        .metadata()
        .get(PdfDocumentMetadataTagType::Title)
        .map(|tag| tag.value().trim().to_string())
        .filter(|s| !s.is_empty());

    let mut first_line: Option<String> = None;
    let mut segments = Vec::new();

    for (idx, page) in doc.pages().iter().enumerate() {
        let page_num = (idx + 1) as u32;
        let raw_text = page
            .text()
            .map_err(|e| ExtractError::Parse(format!("pdf page {page_num} text error: {e}")))?
            .all();
        let text = normalize_page_text(&raw_text);
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
