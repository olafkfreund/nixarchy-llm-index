use crate::proto::Chunk;

/// Returns `true` if `c` is a Unicode tag, bidi mark/override/isolate, invisible operator,
/// variation selector, or zero-width character that must be stripped at ingest.
pub fn is_stripped(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{180E}'
            | '\u{0000}'..='\u{0008}'
            | '\u{000B}'..='\u{001F}'
            | '\u{007F}'..='\u{009F}'
            | '\u{00AD}'
            | '\u{034F}'
            | '\u{115F}'..='\u{1160}'
            | '\u{2028}'..='\u{2029}'
            | '\u{206A}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

fn unsafe_char(c: char) -> bool {
    is_stripped(c) || c.is_control()
}

/// `true` if a file name has a stripped or control character, or is not valid UTF-8.
pub fn unsafe_name(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_none_or(|s| s.chars().any(unsafe_char))
}

/// Replaces each unsafe character with its `\u{XXXX}` escape, for text that quotes untrusted input.
pub fn escape_unsafe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if unsafe_char(c) {
                format!("\\u{{{:04x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// `true` if an already-stored path has a stripped or control character.
pub fn unsafe_path(path: &str) -> bool {
    path.chars().any(unsafe_char)
}

/// Strips Unicode tag characters (`U+E0000..=U+E007F`), bidi marks/overrides/isolates
/// (`U+061C`, `U+200E..=U+200F`, `U+202A..=U+202E`, `U+2066..=U+2069`), invisible operators
/// and zero-width characters (`U+180E`, `U+200B..=U+200D`, `U+2060..=U+2064`, `U+FEFF`), and
/// variation selectors (`U+FE00..=U+FE0F`, `U+E0100..=U+E01EF`).
pub fn sanitize(s: &str) -> String {
    if !s.chars().any(is_stripped) {
        return s.to_string();
    }
    s.chars().filter(|&c| !is_stripped(c)).collect()
}

/// Makes an untrusted title or heading a single bounded display line.
pub fn label(s: &str, max: usize) -> String {
    let spaced: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' | '\u{0085}' | '\u{2028}' | '\u{2029}' => ' ',
            other => other,
        })
        .collect();
    sanitize(&spaced)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// Sanitizes chunk text and non-sheet labels, drops empty text, and renumbers `ord`.
/// Sheet names are sanitized once in `extract::sheet`, where they are made unique.
pub fn sanitize_chunks(chunks: &mut Vec<Chunk>) {
    for c in chunks.iter_mut() {
        c.text = sanitize(&c.text);
        if c.kind != "sheet" {
            c.heading_or_symbol = c.heading_or_symbol.as_deref().and_then(|sym| {
                let cleaned = label(sym, 200);
                (!cleaned.is_empty()).then_some(cleaned)
            });
        }
    }
    chunks.retain(|c| !c.text.trim().is_empty());
    for (idx, c) in chunks.iter_mut().enumerate() {
        c.ord = idx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_unsafe_replaces_only_unsafe_chars() {
        assert_eq!(escape_unsafe("a\u{202e}b\nc é"), "a\\u{202e}b\\u{000a}c é");
    }

    #[test]
    fn unsafe_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        for bad in [
            "a\u{202e}b",
            "a\u{e0041}",
            "a\u{200b}",
            "a\nb",
            "a\x1bb",
            "a\x7f",
            "a\u{85}",
        ] {
            assert!(unsafe_name(OsStr::new(bad)), "{bad:?}");
            assert!(unsafe_path(bad), "{bad:?}");
        }
        assert!(unsafe_name(OsStr::from_bytes(b"a\xffb")));
        for ok in ["notes.md", "Ünïcödé.md", "日本語.txt"] {
            assert!(!unsafe_name(OsStr::new(ok)), "{ok}");
            assert!(!unsafe_path(ok), "{ok}");
        }
    }

    #[test]
    fn strips_tag_bidi_invisible_operators_and_variation_selectors() {
        let input = concat!(
            "\u{FEFF}Hello",
            "\u{061C}\u{180E}",
            "\u{200B}\u{200C}\u{200D}\u{200E}\u{200F}",
            "\u{2060}\u{2061}\u{2062}\u{2063}\u{2064}",
            " ",
            "\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}",
            "World",
            "\u{2066}\u{2067}\u{2068}\u{2069}",
            "\u{FE00}\u{FE08}\u{FE0F}",
            "\u{E0000}\u{E0001}\u{E0041}\u{E007F}",
            "\u{E0100}\u{E017F}\u{E01EF}",
            "!"
        );
        assert_eq!(sanitize(input), "Hello World!");
    }

    #[test]
    fn strips_controls_and_invisible_code_points_but_keeps_lines_and_tabs() {
        let removed = [
            '\x1b', '\x00', '\x08', '\x0b', '\x1f', '\x7f', '\u{80}', '\u{85}', '\u{9f}', '\u{ad}',
            '\u{34f}', '\u{115f}', '\u{1160}', '\u{3164}', '\u{ffa0}', '\u{2028}', '\u{2029}',
            '\u{206a}', '\u{206f}', '\u{fff9}', '\u{fffa}', '\u{fffb}',
        ];
        for c in removed {
            assert_eq!(sanitize(&format!("a{c}b")), "ab", "{c:?}");
        }
        assert_eq!(sanitize("a\r\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn labels_are_single_line_and_bounded() {
        let long = format!(
            "  a\r\n\tb\u{85}c\u{2028}d\u{2029}e\x1bf{}",
            "x".repeat(10_000)
        );
        let cleaned = label(&long, 200);
        assert!(cleaned.starts_with("a b c d ef"));
        assert_eq!(cleaned.chars().count(), 200);
        assert!(!cleaned.chars().any(char::is_control));
    }

    #[test]
    fn sanitize_chunks_drops_empty_after_sanitizing_and_renumbers() {
        let mut chunks = vec![
            Chunk {
                ord: 0,
                kind: "section".into(),
                start: 1,
                end: 1,
                decl_start: None,
                decl_end: None,
                heading_or_symbol: Some("\u{2061}\u{FE0F}".into()),
                text: "\u{200E}\u{200F}\u{061C}\u{2061}\u{2064}\u{180E}\u{FE00}\u{E0100}".into(),
            },
            Chunk {
                ord: 1,
                kind: "section".into(),
                start: 2,
                end: 2,
                decl_start: None,
                decl_end: None,
                heading_or_symbol: Some("Head\u{FE0F}ing".into()),
                text: "Kept\u{2062} text\u{E01EF}".into(),
            },
        ];
        sanitize_chunks(&mut chunks);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].ord, 0);
        assert_eq!(chunks[0].heading_or_symbol.as_deref(), Some("Heading"));
        assert_eq!(chunks[0].text, "Kept text");
    }

    #[test]
    fn generated_symbols_are_bounded_and_sheet_names_keep_identity() {
        let mut chunks = vec![
            Chunk {
                ord: 0,
                kind: "code".into(),
                start: 1,
                end: 1,
                decl_start: None,
                decl_end: None,
                heading_or_symbol: Some("x".repeat(100_000)),
                text: "body".into(),
            },
            Chunk {
                ord: 1,
                kind: "sheet".into(),
                start: 1,
                end: 1,
                decl_start: None,
                decl_end: None,
                heading_or_symbol: Some("A\u{2028}B".into()),
                text: "cell".into(),
            },
        ];
        sanitize_chunks(&mut chunks);
        assert_eq!(chunks[0].heading_or_symbol.as_deref().unwrap().len(), 200);
        assert_eq!(chunks[1].heading_or_symbol.as_deref(), Some("A\u{2028}B"));
    }
}
