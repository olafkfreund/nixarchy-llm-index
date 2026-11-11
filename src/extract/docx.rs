use std::io::BufRead;

use quick_xml::Reader;
use quick_xml::events::Event;

use super::{
    ExtractError, Extracted, FileType, OoxmlKind, Segment, SegmentKind, Skip, TextBudget,
    append_general_ref_budget, file_name, local_name, parse_dc_title, verify_ooxml_content_type,
    verify_zip_guard, with_zip_xml_reader,
};

pub fn extract(
    relpath: &str,
    bytes: &[u8],
    max_text_bytes: usize,
) -> Result<Extracted, ExtractError> {
    verify_zip_guard(bytes)?;
    verify_ooxml_content_type(bytes, OoxmlKind::Docx)?;

    let mut read_so_far = 0;
    let dc_title =
        with_zip_xml_reader(bytes, "docProps/core.xml", &mut read_so_far, parse_dc_title)?
            .flatten();

    let Some((first_heading, segments)) =
        with_zip_xml_reader(bytes, "word/document.xml", &mut read_so_far, |reader| {
            parse_document_xml(reader, max_text_bytes)
        })?
    else {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    };

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

fn parse_document_xml(
    reader: &mut dyn BufRead,
    max_text_bytes: usize,
) -> Result<(Option<String>, Vec<Segment>), ExtractError> {
    let mut xml_reader = Reader::from_reader(reader);
    let mut event_buf = Vec::new();
    let mut budget = TextBudget::new(max_text_bytes);

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
        event_buf.clear();
        match xml_reader.read_event_into(&mut event_buf) {
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
                    if is_heading_outline_lvl(&e) {
                        p_is_heading = true;
                    }
                }
                "t" if in_p => {
                    in_t = true;
                }
                "tab" if in_p => {
                    budget.push_char(&mut p_text, '\t')?;
                }
                "br" | "cr" if in_p => {
                    budget.push_char(&mut p_text, '\n')?;
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
                    if is_heading_outline_lvl(&e) {
                        p_is_heading = true;
                    }
                }
                "tab" if in_p => {
                    budget.push_char(&mut p_text, '\t')?;
                }
                "br" | "cr" if in_p => {
                    budget.push_char(&mut p_text, '\n')?;
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
                    p_text.clear();
                }
                _ => {}
            },
            Ok(Event::Text(e)) if in_t => {
                budget.push_str(&mut p_text, e.as_ref())?;
            }
            Ok(Event::GeneralRef(r)) if in_t => {
                append_general_ref_budget(&r, &mut p_text, &mut budget)?;
            }
            Ok(Event::CData(e)) if in_t => {
                budget.push_str(&mut p_text, e.as_ref())?;
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
            if lower == "title" {
                return true;
            }
            if let Some(rest) = lower.strip_prefix("heading") {
                let rest = rest.trim_start_matches(' ');
                if rest.len() == 1 && matches!(rest.as_bytes()[0], b'1'..=b'9') {
                    return true;
                }
            }
        }
    }
    false
}

fn is_heading_outline_lvl(e: &quick_xml::events::BytesStart<'_>) -> bool {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.as_ref()) == "val"
            && let Ok(n) = attr.value.as_ref().trim().parse::<u8>()
        {
            return n <= 8;
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
