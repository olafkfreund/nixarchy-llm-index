use std::io::BufRead;

use quick_xml::Reader;
use quick_xml::events::Event;

use super::{
    ExtractError, Extracted, FileType, ODF_ODT_MIME, Segment, SegmentKind, Skip, TextBudget,
    append_general_ref_budget, file_name, local_name, parse_dc_title, verify_odf_mimetype,
    verify_zip_guard, with_zip_xml_reader,
};

pub fn extract(
    relpath: &str,
    bytes: &[u8],
    max_text_bytes: usize,
) -> Result<Extracted, ExtractError> {
    verify_zip_guard(bytes)?;
    verify_odf_mimetype(bytes, ODF_ODT_MIME)?;

    let mut read_so_far = 0;
    let dc_title =
        with_zip_xml_reader(bytes, "meta.xml", &mut read_so_far, parse_dc_title)?.flatten();

    let Some((first_heading, segments)) =
        with_zip_xml_reader(bytes, "content.xml", &mut read_so_far, |reader| {
            parse_content_xml(reader, max_text_bytes)
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
        file_type: FileType::Odt,
        segments,
    })
}

fn parse_content_xml(
    reader: &mut dyn BufRead,
    max_text_bytes: usize,
) -> Result<(Option<String>, Vec<Segment>), ExtractError> {
    let mut xml_reader = Reader::from_reader(reader);
    let mut event_buf = Vec::new();
    let mut budget = TextBudget::new(max_text_bytes);

    let mut block_depth: usize = 0;
    let mut is_heading = false;
    let mut block_text = String::new();

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
                "h" => {
                    if block_depth == 0 {
                        is_heading = true;
                        block_text.clear();
                    }
                    block_depth += 1;
                }
                "p" => {
                    if block_depth == 0 {
                        is_heading = false;
                        block_text.clear();
                    }
                    block_depth += 1;
                }
                "s" if block_depth > 0 => {
                    let count = space_count(&e);
                    budget.push_spaces(&mut block_text, count)?;
                }
                "tab" if block_depth > 0 => {
                    budget.push_char(&mut block_text, '\t')?;
                }
                "line-break" if block_depth > 0 => {
                    budget.push_char(&mut block_text, '\n')?;
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if block_depth > 0 => match local_name(e.name().as_ref()) {
                "s" => {
                    let count = space_count(&e);
                    budget.push_spaces(&mut block_text, count)?;
                }
                "tab" => {
                    budget.push_char(&mut block_text, '\t')?;
                }
                "line-break" => {
                    budget.push_char(&mut block_text, '\n')?;
                }
                _ => {}
            },
            Ok(Event::End(e)) => match local_name(e.name().as_ref()) {
                "h" | "p" if block_depth > 0 => {
                    block_depth -= 1;
                    if block_depth == 0 {
                        let text = block_text.trim();
                        if !text.is_empty() {
                            if is_heading {
                                if first_heading.is_none() {
                                    first_heading = Some(text.to_string());
                                }
                                flush_odt_section(
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
                        block_text.clear();
                    }
                }
                _ => {}
            },
            Ok(Event::Text(e)) if block_depth > 0 => {
                budget.push_str(&mut block_text, e.as_ref())?;
            }
            Ok(Event::GeneralRef(r)) if block_depth > 0 => {
                append_general_ref_budget(&r, &mut block_text, &mut budget)?;
            }
            Ok(Event::CData(e)) if block_depth > 0 => {
                budget.push_str(&mut block_text, e.as_ref())?;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(ExtractError::Parse(format!("odt xml error: {e}"))),
            _ => {}
        }
    }

    flush_odt_section(
        &mut segments,
        cur_heading,
        cur_start_line,
        total_lines,
        &mut cur_lines,
    );

    Ok((first_heading, segments))
}

fn space_count(e: &quick_xml::events::BytesStart<'_>) -> usize {
    for attr in e.attributes().flatten() {
        if local_name(attr.key.as_ref()) == "c"
            && let Ok(n) = attr.value.as_ref().parse::<usize>()
        {
            return n.min(1024);
        }
    }
    1
}

fn flush_odt_section(
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
