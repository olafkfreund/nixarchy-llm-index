use super::{Extracted, FileType, Segment, SegmentKind, file_name};

pub fn extract(relpath: &str, text: &str) -> Extracted {
    let lines: Vec<&str> = text.lines().collect();
    let (fm_title, body_start_idx) = parse_front_matter(&lines);

    let mut first_h1: Option<String> = None;
    let mut segments: Vec<Segment> = Vec::new();

    let mut cur_heading: Option<String> = None;
    let mut cur_start_line: u32 = (body_start_idx + 1) as u32;
    let mut cur_lines: Vec<&str> = Vec::new();
    let mut in_fence: Option<char> = None;

    for (idx, &raw_line) in lines.iter().enumerate().skip(body_start_idx) {
        let line_no = (idx + 1) as u32;
        let trimmed = raw_line.trim_start();

        if let Some(fence_ch) = fence_marker(trimmed) {
            match in_fence {
                None => in_fence = Some(fence_ch),
                Some(open_ch) if open_ch == fence_ch => in_fence = None,
                Some(_) => {}
            }
            cur_lines.push(raw_line);
            continue;
        }

        if in_fence.is_none()
            && let Some((level, heading_text)) = parse_atx_heading(raw_line)
        {
            if level == 1 && first_h1.is_none() && !heading_text.is_empty() {
                first_h1 = Some(heading_text.clone());
            }
            flush_section(
                &mut segments,
                cur_heading.take(),
                cur_start_line,
                line_no.saturating_sub(1),
                &mut cur_lines,
            );
            cur_heading = Some(heading_text);
            cur_start_line = line_no;
            cur_lines.push(raw_line);
            continue;
        }

        cur_lines.push(raw_line);
    }

    let last_line = (lines.len() as u32).max(cur_start_line);
    flush_section(
        &mut segments,
        cur_heading,
        cur_start_line,
        last_line,
        &mut cur_lines,
    );

    let title = fm_title
        .or(first_h1)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| file_name(relpath));

    Extracted {
        title,
        file_type: FileType::Markdown,
        segments,
    }
}

fn flush_section(
    segments: &mut Vec<Segment>,
    heading: Option<String>,
    start_line: u32,
    end_line: u32,
    lines: &mut Vec<&str>,
) {
    let body = lines.join("\n");
    lines.clear();
    if body.trim().is_empty() {
        return;
    }
    segments.push(Segment {
        kind: SegmentKind::Section { heading },
        start_line,
        end_line: end_line.max(start_line),
        text: body,
    });
}

fn fence_marker(trimmed: &str) -> Option<char> {
    if trimmed.starts_with("```") {
        Some('`')
    } else if trimmed.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }
    let rest = &line[leading_spaces..];
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !after.is_empty() && !after.starts_with(' ') && !after.starts_with('\t') {
        return None;
    }
    let trimmed = after.trim().trim_end_matches('#').trim();
    Some((level, trimmed.to_string()))
}

fn parse_front_matter(lines: &[&str]) -> (Option<String>, usize) {
    let Some(&first) = lines.first() else {
        return (None, 0);
    };
    let delimiter = match first.trim() {
        "---" => "---",
        "+++" => "+++",
        _ => return (None, 0),
    };
    let mut end_idx = None;
    for (idx, &line) in lines.iter().enumerate().skip(1) {
        let t = line.trim();
        if t == delimiter || (delimiter == "---" && t == "...") {
            end_idx = Some(idx);
            break;
        }
    }
    let Some(end) = end_idx else {
        return (None, 0);
    };
    let mut title = None;
    for &line in &lines[1..end] {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let rest = if delimiter == "---" {
            line.strip_prefix("title:")
        } else {
            line.strip_prefix("title")
                .map(str::trim_start)
                .and_then(|s| s.strip_prefix('='))
        };
        if let Some(val) = rest {
            let unquoted = strip_quotes(val.trim());
            if !unquoted.is_empty() {
                title = Some(unquoted.to_string());
                break;
            }
        }
    }
    (title, end + 1)
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}
