use quick_xml::Reader;
use quick_xml::events::Event;

use super::{
    ExtractError, Extracted, FileType, Segment, SegmentKind, Skip, append_general_ref, file_name,
    local_name, read_zip_entries,
};

pub fn extract(relpath: &str, bytes: &[u8]) -> Result<Extracted, ExtractError> {
    let entries = read_zip_entries(
        bytes,
        "[Content_Types].xml",
        &["docProps/core.xml", "word/document.xml"],
    )?;
    let Some(doc_xml) = entries.get("word/document.xml") else {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    };

    let dc_title = entries
        .get("docProps/core.xml")
        .and_then(|xml| parse_core_title(xml));

    let (first_heading, segments) = parse_document_xml(doc_xml)?;
    let title = dc_title
        .or(first_heading)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| file_name(relpath));

    Ok(Extracted {
        title,
        file_type: FileType::Docx,
        segments,
    })
}

fn parse_core_title(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    let mut in_title = false;
    let mut buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == "title" {
                    in_title = true;
                    buf.clear();
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "title" && in_title {
                    let trimmed = buf.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                    in_title = false;
                }
            }
            Ok(Event::Text(e)) if in_title => {
                buf.push_str(e.as_ref());
            }
            Ok(Event::GeneralRef(r)) if in_title => {
                append_general_ref(&r, &mut buf);
            }
            Ok(Event::CData(e)) if in_title => {
                buf.push_str(e.as_ref());
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    None
}

fn parse_document_xml(xml: &str) -> Result<(Option<String>, Vec<Segment>), ExtractError> {
    let mut reader = Reader::from_str(xml);
    let mut in_p = false;
    let mut in_ppr = false;
    let mut in_t = false;
    let mut p_is_heading = false;
    let mut p_text = String::new();

    let mut first_heading: Option<String> = None;
    let mut segments: Vec<Segment> = Vec::new();
    let mut cur_heading: Option<String> = None;
    let mut cur_lines: Vec<String> = Vec::new();
    let mut cur_start_line: u32 = 1;
    let mut total_lines: u32 = 0;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                "p" => {
                    in_p = true;
                    p_is_heading = false;
                    p_text.clear();
                }
                "pPr" if in_p => {
                    in_ppr = true;
                }
                "pStyle" if in_ppr => {
                    if is_heading_style(&e) {
                        p_is_heading = true;
                    }
                }
                "outlineLvl" if in_ppr => {
                    p_is_heading = true;
                }
                "t" if in_p => {
                    in_t = true;
                }
                "tab" if in_p => {
                    p_text.push('\t');
                }
                "br" | "cr" if in_p => {
                    p_text.push('\n');
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => match local_name(e.name().as_ref()) {
                "pStyle" if in_ppr => {
                    if is_heading_style(&e) {
                        p_is_heading = true;
                    }
                }
                "outlineLvl" if in_ppr => {
                    p_is_heading = true;
                }
                "tab" if in_p => {
                    p_text.push('\t');
                }
                "br" | "cr" if in_p => {
                    p_text.push('\n');
                }
                _ => {}
            },
            Ok(Event::End(e)) => match local_name(e.name().as_ref()) {
                "t" => {
                    in_t = false;
                }
                "pPr" => {
                    in_ppr = false;
                }
                "p" if in_p => {
                    in_p = false;
                    let text = p_text.trim();
                    if !text.is_empty() {
                        if p_is_heading {
                            if first_heading.is_none() {
                                first_heading = Some(text.to_string());
                            }
                            flush_docx_section(
                                &mut segments,
                                cur_heading.take(),
                                cur_start_line,
                                total_lines,
                                &mut cur_lines,
                            );
                            cur_heading = Some(text.to_string());
                            cur_start_line = total_lines + 1;
                        }
                        for line in text.lines() {
                            total_lines += 1;
                            cur_lines.push(line.to_string());
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Text(e)) if in_t => {
                p_text.push_str(e.as_ref());
            }
            Ok(Event::GeneralRef(r)) if in_t => {
                append_general_ref(&r, &mut p_text);
            }
            Ok(Event::CData(e)) if in_t => {
                p_text.push_str(e.as_ref());
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ExtractError::Parse(format!("docx xml error: {e}"))),
            _ => {}
        }
    }

    flush_docx_section(
        &mut segments,
        cur_heading,
        cur_start_line,
        total_lines,
        &mut cur_lines,
    );

    Ok((first_heading, segments))
}

fn is_heading_style(e: &quick_xml::events::BytesStart<'_>) -> bool {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.as_ref()) == "val" {
            let lower = attr.value.as_ref().to_ascii_lowercase();
            if lower.starts_with("heading") || lower == "title" || lower == "subtitle" {
                return true;
            }
        }
    }
    false
}

fn flush_docx_section(
    segments: &mut Vec<Segment>,
    heading: Option<String>,
    start_line: u32,
    end_line: u32,
    lines: &mut Vec<String>,
) {
    if lines.is_empty() {
        return;
    }
    let text = lines.join("\n");
    lines.clear();
    segments.push(Segment {
        kind: SegmentKind::Section { heading },
        start_line,
        end_line: end_line.max(start_line),
        text,
    });
}
