use super::{Extracted, FileType, Segment, SegmentKind, file_name, line_count};

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

pub fn extract(relpath: &str, lang: &'static str, text: &str) -> Extracted {
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
        file_type: FileType::Code(lang),
        segments,
    }
}
