use super::{
    ExtractError, Extracted, FileType, Segment, SegmentKind, TextBudget, file_name, line_count,
};

pub fn language_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "go" => Some("go"),
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
        "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
        "nix" => Some("nix"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some("cpp"),
        "java" => Some("java"),
        "sh" | "bash" => Some("bash"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "json" | "jsonc" | "map" => Some("json"),
        "css" => Some("css"),
        _ => None,
    }
}

pub fn extract(
    relpath: &str,
    lang: &'static str,
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
        file_type: FileType::Code(lang),
        segments,
    })
}
