use quick_xml::Reader;
use quick_xml::events::Event;

use super::{
    ExtractError, Extracted, FileType, Segment, SegmentKind, Skip, append_general_ref, file_name,
    local_name, read_zip_entries,
};

pub fn extract(relpath: &str, bytes: &[u8]) -> Result<Extracted, ExtractError> {
    let entries = read_zip_entries(bytes, "mimetype", &["meta.xml", "content.xml"])?;
    let Some(content_xml) = entries.get("content.xml") else {
        return Err(ExtractError::Skipped(Skip::Mismatch));
    };

    let dc_title = entries
        .get("meta.xml")
        .and_then(|xml| parse_meta_title(xml));

    let (first_heading, segments) = parse_content_xml(content_xml)?;
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

fn parse_meta_title(xml: &str) -> Option<String> {
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

fn parse_content_xml(xml: &str) -> Result<(Option<String>, Vec<Segment>), ExtractError> {
    let mut reader = Reader::from_str(xml);
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
        match reader.read_event() {
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
                    for _ in 0..count {
                        block_text.push(' ');
                    }
                }
                "tab" if block_depth > 0 => {
                    block_text.push('\t');
                }
                "line-break" if block_depth > 0 => {
                    block_text.push('\n');
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if block_depth > 0 => match local_name(e.name().as_ref()) {
                "s" => {
                    let count = space_count(&e);
                    for _ in 0..count {
                        block_text.push(' ');
                    }
                }
                "tab" => {
                    block_text.push('\t');
                }
                "line-break" => {
                    block_text.push('\n');
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
                block_text.push_str(e.as_ref());
            }
            Ok(Event::GeneralRef(r)) if block_depth > 0 => {
                append_general_ref(&r, &mut block_text);
            }
            Ok(Event::CData(e)) if block_depth > 0 => {
                block_text.push_str(e.as_ref());
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
            return n.min(256);
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
