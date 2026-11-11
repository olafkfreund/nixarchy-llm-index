use super::{
    ExtractError, Extracted, FileType, Segment, SegmentKind, TextBudget, file_name, line_count,
};

pub fn extract(
    relpath: &str,
    text: &str,
    max_text_bytes: usize,
) -> Result<Extracted, ExtractError> {
    let title = file_name(relpath);
    let mut budget = TextBudget::new(max_text_bytes);
    let segments = if text.is_empty() {
        Vec::new()
    } else {
        let mut out = String::new();
        budget.push_str(&mut out, text)?;
        vec![Segment {
            kind: SegmentKind::Whole,
            start_line: 1,
            end_line: line_count(&out),
            text: out,
        }]
    };
    Ok(Extracted {
        title,
        file_type: FileType::Text,
        segments,
    })
}
