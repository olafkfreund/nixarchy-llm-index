use scraper::{ElementRef, Html, Node, Selector};

use super::{Extracted, FileType, Segment, SegmentKind, file_name, line_count};

pub fn extract(relpath: &str, html: &str) -> Extracted {
    let doc = Html::parse_document(html);

    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| doc.select(&sel).next())
        .map(|el| {
            el.text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| file_name(relpath));

    let root = Selector::parse("body")
        .ok()
        .and_then(|sel| doc.select(&sel).next())
        .unwrap_or_else(|| doc.root_element());

    let mut raw = String::new();
    walk_element(root, &mut raw);

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

    Extracted {
        title,
        file_type: FileType::Html,
        segments,
    }
}

fn walk_element(el: ElementRef<'_>, out: &mut String) {
    let tag = el.value().name();
    if matches!(
        tag,
        "script" | "style" | "noscript" | "head" | "template" | "svg"
    ) {
        return;
    }
    let block = is_block(tag);
    if block && !out.ends_with('\n') {
        out.push('\n');
    }
    for child in el.children() {
        match child.value() {
            Node::Text(text) => {
                if !text.trim().is_empty() {
                    if !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                        out.push(' ');
                    }
                    out.push_str(text);
                }
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    walk_element(child_el, out);
                }
            }
            _ => {}
        }
    }
    if block && !out.ends_with('\n') {
        out.push('\n');
    }
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
