use crate::proto::Chunk;

/// Returns `true` if `c` is a Unicode tag, bidi mark/override/isolate, invisible operator,
/// variation selector, or zero-width character that must be stripped at ingest.
pub fn is_stripped(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{180E}'
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

/// Sanitizes each chunk's `text` and `heading_or_symbol`, drops chunks whose `text` is empty
/// after sanitizing, and renumbers `ord` contiguously from `0`.
pub fn sanitize_chunks(chunks: &mut Vec<Chunk>) {
    for c in chunks.iter_mut() {
        c.text = sanitize(&c.text);
        if let Some(sym) = &c.heading_or_symbol {
            let cleaned = sanitize(sym);
            c.heading_or_symbol = if cleaned.trim().is_empty() {
                None
            } else {
                Some(cleaned)
            };
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
}
