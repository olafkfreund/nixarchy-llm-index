use super::{ExtractError, Extracted, FileType, Segment, SegmentKind, TextBudget, file_name};

pub fn extract(
    relpath: &str,
    text: &str,
    max_text_bytes: usize,
) -> Result<Extracted, ExtractError> {
    let lines: Vec<&str> = text.lines().collect();
    let (fm_title, body_start_idx) = parse_front_matter(&lines);

    let mut budget = TextBudget::new(max_text_bytes);
    let mut first_h1: Option<String> = None;
    let mut segments: Vec<Segment> = Vec::new();

    let mut cur_heading: Option<String> = None;
    let mut cur_start_line: u32 = (body_start_idx + 1) as u32;
    let mut cur_body = String::new();
    let mut cur_has_lines = false;
    let mut in_fence: Option<char> = None;

    let mut idx = body_start_idx;
    while idx < lines.len() {
        let raw_line = lines[idx];
        let line_no = (idx + 1) as u32;
        let trimmed = raw_line.trim_start();

        if let Some(fence_ch) = fence_marker(trimmed) {
            match in_fence {
                None => in_fence = Some(fence_ch),
                Some(open_ch) if open_ch == fence_ch => in_fence = None,
                Some(_) => {}
            }
            push_line(&mut budget, &mut cur_body, &mut cur_has_lines, raw_line)?;
            idx += 1;
            continue;
        }

        if in_fence.is_none() {
            if let Some((level, heading_text)) = parse_atx_heading(raw_line) {
                if level == 1 && first_h1.is_none() && !heading_text.is_empty() {
                    first_h1 = Some(heading_text.clone());
                }
                flush_section(
                    &mut segments,
                    cur_heading.take(),
                    cur_start_line,
                    line_no.saturating_sub(1),
                    &mut cur_body,
                    &mut cur_has_lines,
                );
                cur_heading = Some(heading_text);
                cur_start_line = line_no;
                push_line(&mut budget, &mut cur_body, &mut cur_has_lines, raw_line)?;
                idx += 1;
                continue;
            }

            if idx + 1 < lines.len()
                && is_setext_heading_candidate(raw_line)
                && let Some(level) = parse_setext_underline(lines[idx + 1])
            {
                let heading_text = raw_line.trim().to_string();
                if level == 1 && first_h1.is_none() && !heading_text.is_empty() {
                    first_h1 = Some(heading_text.clone());
                }
                flush_section(
                    &mut segments,
                    cur_heading.take(),
                    cur_start_line,
                    line_no.saturating_sub(1),
                    &mut cur_body,
                    &mut cur_has_lines,
                );
                cur_heading = Some(heading_text);
                cur_start_line = line_no;
                push_line(&mut budget, &mut cur_body, &mut cur_has_lines, raw_line)?;
                push_line(
                    &mut budget,
                    &mut cur_body,
                    &mut cur_has_lines,
                    lines[idx + 1],
                )?;
                idx += 2;
                continue;
            }
        }

        push_line(&mut budget, &mut cur_body, &mut cur_has_lines, raw_line)?;
        idx += 1;
    }

    let last_line = (lines.len() as u32).max(cur_start_line);
    flush_section(
        &mut segments,
        cur_heading,
        cur_start_line,
        last_line,
        &mut cur_body,
        &mut cur_has_lines,
    );

    let title = fm_title
        .or(first_h1)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| file_name(relpath));

    Ok(Extracted {
        title,
        file_type: FileType::Markdown,
        segments,
    })
}

fn push_line(
    budget: &mut TextBudget,
    cur_body: &mut String,
    cur_has_lines: &mut bool,
    line: &str,
) -> Result<(), ExtractError> {
    if *cur_has_lines {
        budget.push_char(cur_body, '\n')?;
    }
    budget.push_str(cur_body, line)?;
    *cur_has_lines = true;
    Ok(())
}

fn flush_section(
    segments: &mut Vec<Segment>,
    heading: Option<String>,
    start_line: u32,
    end_line: u32,
    body: &mut String,
    has_lines: &mut bool,
) {
    let text = std::mem::take(body);
    *has_lines = false;
    if text.trim().is_empty() {
        return;
    }
    segments.push(Segment {
        kind: SegmentKind::Section { heading },
        start_line,
        end_line: end_line.max(start_line),
        text,
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
    let trimmed = strip_atx_closing_hashes(after.trim());
    Some((level, trimmed.to_string()))
}

fn strip_atx_closing_hashes(s: &str) -> &str {
    let without_hashes = s.trim_end_matches('#');
    if without_hashes.len() == s.len() {
        return s;
    }
    if without_hashes.is_empty() {
        return "";
    }
    if without_hashes.ends_with(' ') || without_hashes.ends_with('\t') {
        without_hashes.trim_end()
    } else {
        s
    }
}

fn is_setext_heading_candidate(line: &str) -> bool {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return false;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || parse_setext_underline(line).is_some() {
        return false;
    }
    !trimmed.starts_with('#') && !trimmed.starts_with('>') && fence_marker(trimmed).is_none()
}

fn parse_setext_underline(line: &str) -> Option<usize> {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }
    let trimmed = line[leading_spaces..].trim_end();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.bytes().all(|b| b == b'=') {
        Some(1)
    } else if trimmed.len() >= 2 && trimmed.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
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
    let fm_lines = &lines[1..end];
    for (rel_idx, &line) in fm_lines.iter().enumerate() {
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
            let trimmed_val = val.trim();
            if delimiter == "---" && is_yaml_block_scalar_header(trimmed_val) {
                let block = collect_yaml_block_scalar(&fm_lines[rel_idx + 1..]);
                if !block.is_empty() {
                    title = Some(block);
                    break;
                }
            } else {
                let unquoted = strip_quotes(trimmed_val);
                if !unquoted.is_empty() {
                    title = Some(unquoted.to_string());
                    break;
                }
            }
        }
    }
    (title, end + 1)
}

fn is_yaml_block_scalar_header(val: &str) -> bool {
    let without_comment = match val.find('#') {
        Some(idx) => val[..idx].trim_end(),
        None => val,
    };
    let Some(first) = without_comment.chars().next() else {
        return false;
    };
    if first != '|' && first != '>' {
        return false;
    }
    without_comment[first.len_utf8()..]
        .chars()
        .all(|c| c == '-' || c == '+' || c.is_ascii_digit())
}

fn collect_yaml_block_scalar(following: &[&str]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut min_indent: Option<usize> = None;

    for &line in following {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .count();
        if indent == 0 {
            break;
        }
        if let Some(base) = min_indent {
            if indent < base {
                break;
            }
        } else {
            min_indent = Some(indent);
        }
        parts.push(line.trim());
    }

    parts.join(" ")
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
