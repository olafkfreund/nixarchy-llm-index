/// Returns `true` if `c` is a Unicode tag, bidi override/isolate, or zero-width character
/// that must be stripped at ingest.
pub fn is_stripped(c: char) -> bool {
    matches!(
        c,
        '\u{E0000}'..='\u{E007F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200B}'..='\u{200D}'
            | '\u{FEFF}'
            | '\u{2060}'
    )
}

/// Strips Unicode tag characters (`U+E0000..=U+E007F`), bidi overrides/isolates
/// (`U+202A..=U+202E`, `U+2066..=U+2069`) and zero-width characters
/// (`U+200B..=U+200D`, `U+FEFF`, `U+2060`).
pub fn sanitize(s: &str) -> String {
    if !s.chars().any(is_stripped) {
        return s.to_string();
    }
    s.chars().filter(|&c| !is_stripped(c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tag_bidi_and_zero_width_characters() {
        let input = concat!(
            "\u{FEFF}Hello",
            "\u{200B}\u{200C}\u{200D}\u{2060}",
            " ",
            "\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}",
            "World",
            "\u{2066}\u{2067}\u{2068}\u{2069}",
            "\u{E0000}\u{E0001}\u{E0041}\u{E007F}",
            "!"
        );
        assert_eq!(sanitize(input), "Hello World!");
    }
}
