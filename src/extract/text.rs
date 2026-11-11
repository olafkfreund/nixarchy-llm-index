use super::{Extracted, FileType, Segment, SegmentKind, file_name, line_count};

pub fn extract(relpath: &str, text: &str) -> Extracted {
    let title = file_name(relpath);
    let segments = if text.is_empty() {
        Vec::new()
    } else {
        vec![Segment {
            kind: SegmentKind::Whole,
            start_line: 1,
            end_line: line_count(text),
            text: text.to_string(),
        }]
    };
    Extracted {
        title,
        file_type: FileType::Text,
        segments,
    }
}
