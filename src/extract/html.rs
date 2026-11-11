use std::borrow::Cow;

use scraper::{ElementRef, Html, Node};

use super::{
    ExtractError, Extracted, FileType, Segment, SegmentKind, TextBudget, file_name, line_count,
};

const MAX_HTML_INPUT_BYTES: usize = 20 * 1024 * 1024;
const MAX_HTML_DEPTH: usize = 256;

pub fn extract(
    relpath: &str,
    html: &str,
    max_text_bytes: usize,
) -> Result<Extracted, ExtractError> {
    if html.len() > MAX_HTML_INPUT_BYTES {
        return Err(ExtractError::TooLarge);
    }

    let bounded_html = prune_deep_html(html, MAX_HTML_DEPTH);
    let doc = Html::parse_document(&bounded_html);
    let doc_root = doc.root_element();

    let title = extract_head_title(doc_root)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| file_name(relpath));

    let body_root = find_direct_child_element(doc_root, "body").unwrap_or(doc_root);

    let mut budget = TextBudget::new(max_text_bytes);
    let mut raw = String::new();
    walk_element_iterative(body_root, &mut budget, &mut raw)?;

    let cleaned = raw
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    let segments = if cleaned.is_empty() {
        Vec::new()
    } else {
        vec![Segment {
            kind: SegmentKind::Whole,
            start_line: 1,
            end_line: line_count(&cleaned),
            text: cleaned,
        }]
    };

    Ok(Extracted {
        title,
        file_type: FileType::Html,
        segments,
    })
}

fn find_direct_child_element<'a>(parent: ElementRef<'a>, tag: &str) -> Option<ElementRef<'a>> {
    for child in parent.children() {
        if let Some(el) = ElementRef::wrap(child)
            && el.value().name() == tag
        {
            return Some(el);
        }
    }
    None
}

fn extract_head_title(doc_root: ElementRef<'_>) -> Option<String> {
    let head = find_direct_child_element(doc_root, "head")?;
    let mut stack: Vec<(ElementRef<'_>, usize)> = vec![(head, 1)];

    while let Some((el, depth)) = stack.pop() {
        if depth > MAX_HTML_DEPTH {
            continue;
        }
        let tag = el.value().name();
        if matches!(tag, "script" | "style" | "noscript" | "template" | "svg") {
            continue;
        }
        if tag == "title" {
            let mut title_buf = String::new();
            for child in el.children() {
                if let Node::Text(t) = child.value() {
                    title_buf.push_str(t);
                    title_buf.push(' ');
                }
            }
            let normalized = title_buf.split_whitespace().collect::<Vec<_>>().join(" ");
            if !normalized.is_empty() {
                return Some(normalized);
            }
            continue;
        }
        if depth < MAX_HTML_DEPTH {
            for child in el.children().rev() {
                if let Some(child_el) = ElementRef::wrap(child) {
                    stack.push((child_el, depth + 1));
                }
            }
        }
    }
    None
}

enum WalkStep<'a> {
    Enter(ElementRef<'a>, usize),
    Text(&'a str),
    ExitBlock,
}

fn walk_element_iterative(
    root: ElementRef<'_>,
    budget: &mut TextBudget,
    out: &mut String,
) -> Result<(), ExtractError> {
    let mut stack: Vec<WalkStep<'_>> = vec![WalkStep::Enter(root, 1)];

    while let Some(step) = stack.pop() {
        match step {
            WalkStep::Enter(el, depth) => {
                if depth > MAX_HTML_DEPTH {
                    continue;
                }
                let tag = el.value().name();
                if matches!(
                    tag,
                    "script" | "style" | "noscript" | "head" | "template" | "svg"
                ) {
                    continue;
                }
                let block = is_block(tag);
                if block && !out.ends_with('\n') {
                    budget.push_char(out, '\n')?;
                }
                if block {
                    stack.push(WalkStep::ExitBlock);
                }
                for child in el.children().rev() {
                    match child.value() {
                        Node::Text(text) => {
                            if !text.trim().is_empty() {
                                stack.push(WalkStep::Text(text));
                            }
                        }
                        Node::Element(_) => {
                            if depth < MAX_HTML_DEPTH
                                && let Some(child_el) = ElementRef::wrap(child)
                            {
                                stack.push(WalkStep::Enter(child_el, depth + 1));
                            }
                        }
                        _ => {}
                    }
                }
            }
            WalkStep::Text(text) => {
                if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                    budget.push_char(out, ' ')?;
                }
                budget.push_str(out, text)?;
            }
            WalkStep::ExitBlock => {
                if !out.ends_with('\n') {
                    budget.push_char(out, '\n')?;
                }
            }
        }
    }

    Ok(())
}

/// Linear pre-pass that returns `Cow::Borrowed(html)` untouched when element nesting
/// depth stays within `max_depth`, or strips subtrees deeper than `max_depth` before
/// handing the document to `html5ever` (whose open-element scope checks are quadratic
/// in open block depth).
fn prune_deep_html(html: &str, max_depth: usize) -> Cow<'_, str> {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    let mut depth = 0usize;
    let mut keep_start = 0usize;
    let mut out: Option<String> = None;

    while pos < len {
        let Some(rel_lt) = bytes[pos..].iter().position(|&b| b == b'<') else {
            break;
        };
        let lt = pos + rel_lt;
        if lt + 1 >= len {
            break;
        }

        if bytes[lt..].starts_with(b"<!--") {
            if let Some(rel_end) = html[lt + 4..].find("-->") {
                pos = lt + 4 + rel_end + 3;
            } else {
                break;
            }
            continue;
        }

        if bytes[lt..].starts_with(b"<![CDATA[") {
            if let Some(rel_end) = html[lt + 9..].find("]]>") {
                pos = lt + 9 + rel_end + 3;
            } else {
                break;
            }
            continue;
        }

        let second = bytes[lt + 1];
        if second == b'!' || second == b'?' {
            if let Some(rel_gt) = bytes[lt + 2..].iter().position(|&b| b == b'>') {
                pos = lt + 2 + rel_gt + 1;
            } else {
                break;
            }
            continue;
        }

        let is_close = second == b'/';
        let name_start = if is_close { lt + 2 } else { lt + 1 };
        if name_start >= len || !bytes[name_start].is_ascii_alphabetic() {
            pos = lt + 1;
            continue;
        }

        let mut name_end = name_start + 1;
        while name_end < len
            && (bytes[name_end].is_ascii_alphanumeric()
                || bytes[name_end] == b'-'
                || bytes[name_end] == b':')
        {
            name_end += 1;
        }
        let tag_name = &html[name_start..name_end];

        let Some(gt) = find_tag_close(bytes, name_end) else {
            break;
        };
        let self_closing = !is_close && bytes[gt.saturating_sub(1)] == b'/';
        let next_pos = gt + 1;

        if is_close {
            if !is_void_element(tag_name) && depth > 0 {
                if depth == max_depth + 1 {
                    keep_start = next_pos;
                }
                depth -= 1;
            }
            pos = next_pos;
            continue;
        }

        if !self_closing && !is_void_element(tag_name) {
            if tag_name.eq_ignore_ascii_case("script") || tag_name.eq_ignore_ascii_case("style") {
                pos = skip_rawtext_element(html, next_pos, tag_name);
                continue;
            }
            depth += 1;
            if depth == max_depth + 1 {
                let buf = out.get_or_insert_with(|| String::with_capacity(len.min(64 * 1024)));
                buf.push_str(&html[keep_start..lt]);
            }
        }

        pos = next_pos;
    }

    match out {
        None => Cow::Borrowed(html),
        Some(mut buf) => {
            if depth <= max_depth && keep_start < len {
                buf.push_str(&html[keep_start..]);
            }
            Cow::Owned(buf)
        }
    }
}

fn find_tag_close(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some(i),
            None => {}
        }
        i += 1;
    }
    None
}

fn skip_rawtext_element(html: &str, mut pos: usize, tag_name: &str) -> usize {
    let bytes = html.as_bytes();
    while let Some(rel) = bytes[pos..].iter().position(|&b| b == b'<') {
        let lt = pos + rel;
        if lt + 2 + tag_name.len() <= bytes.len()
            && bytes[lt + 1] == b'/'
            && html[lt + 2..lt + 2 + tag_name.len()].eq_ignore_ascii_case(tag_name)
            && let Some(gt) = find_tag_close(bytes, lt + 2 + tag_name.len())
        {
            return gt + 1;
        }
        pos = lt + 1;
    }
    bytes.len()
}

fn is_void_element(tag: &str) -> bool {
    tag.eq_ignore_ascii_case("area")
        || tag.eq_ignore_ascii_case("base")
        || tag.eq_ignore_ascii_case("br")
        || tag.eq_ignore_ascii_case("col")
        || tag.eq_ignore_ascii_case("embed")
        || tag.eq_ignore_ascii_case("hr")
        || tag.eq_ignore_ascii_case("img")
        || tag.eq_ignore_ascii_case("input")
        || tag.eq_ignore_ascii_case("keygen")
        || tag.eq_ignore_ascii_case("link")
        || tag.eq_ignore_ascii_case("meta")
        || tag.eq_ignore_ascii_case("param")
        || tag.eq_ignore_ascii_case("source")
        || tag.eq_ignore_ascii_case("track")
        || tag.eq_ignore_ascii_case("wbr")
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "footer"
            | "nav"
            | "aside"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "ul"
            | "ol"
            | "tr"
            | "table"
            | "br"
            | "hr"
            | "pre"
            | "blockquote"
            | "dt"
            | "dd"
    )
}
