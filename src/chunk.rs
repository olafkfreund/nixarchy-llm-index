use std::ops::ControlFlow;
use std::time::Instant;

use text_splitter::{ChunkConfig, ChunkSizer, MarkdownSplitter, TextSplitter};
use tree_sitter::{Language, Node, ParseOptions, ParseState, Parser};

use crate::extract::{ExtractError, FileType, Segment, SegmentKind};
use crate::proto::Chunk;

pub const TARGET_TOKENS: usize = 512;
const MAX_CHUNK_BYTES: usize = TARGET_TOKENS * 16;

/// Approximates token count as whitespace-separated words × 1.3 (with a byte-length floor
/// for long whitespace-free sequences).
pub fn approx_tokens(text: &str) -> usize {
    let words = count_words(text);
    tokens_from_counts(words, text.len())
}

#[inline]
fn count_words(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut count = 0usize;
    let mut in_word = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            let ws = b.is_ascii_whitespace();
            if !ws && !in_word {
                count += 1;
            }
            in_word = !ws;
            i += 1;
        } else {
            return s.split_whitespace().count();
        }
    }
    count
}

#[inline]
fn tokens_from_counts(words: usize, bytes: usize) -> usize {
    let by_words = ((words as f64) * 1.3).ceil() as usize;
    by_words.max(bytes / 16)
}

#[inline]
fn combine_words_at_boundary(
    source: &str,
    a: usize,
    b: usize,
    c: usize,
    words_a: usize,
    words_b: usize,
) -> usize {
    if a < b && b < c {
        let bytes = source.as_bytes();
        let prev_b = bytes[b - 1];
        let next_b = bytes[b];
        if prev_b.is_ascii() && next_b.is_ascii() {
            if !prev_b.is_ascii_whitespace() && !next_b.is_ascii_whitespace() {
                return (words_a + words_b).saturating_sub(1);
            }
            return words_a + words_b;
        }
        let prev_non_ws = source[a..b]
            .chars()
            .next_back()
            .is_some_and(|ch| !ch.is_whitespace());
        let next_non_ws = source[b..c]
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace());
        if prev_non_ws && next_non_ws {
            return (words_a + words_b).saturating_sub(1);
        }
    }
    words_a + words_b
}

#[derive(Clone, Copy)]
struct TokenSizer;

impl ChunkSizer for TokenSizer {
    fn size(&self, chunk: &str) -> usize {
        approx_tokens(chunk).max(1)
    }
}

#[inline]
fn check_deadline(deadline: Instant) -> Result<(), ExtractError> {
    if Instant::now() >= deadline {
        return Err(ExtractError::Timeout);
    }
    Ok(())
}

pub fn chunk<S: AsRef<[Segment]>>(
    segments: S,
    file_type: FileType,
    deadline: Instant,
) -> Result<Vec<Chunk>, ExtractError> {
    check_deadline(deadline)?;
    let segments = segments.as_ref();
    let mut out = Vec::new();

    match file_type {
        FileType::Code(lang) => {
            for seg in segments {
                check_deadline(deadline)?;
                if !seg.text.is_empty() {
                    chunk_code(&seg.text, lang, deadline, &mut out)?;
                }
            }
        }
        FileType::Markdown => {
            for seg in segments {
                check_deadline(deadline)?;
                let heading = match &seg.kind {
                    SegmentKind::Section { heading } => heading.clone(),
                    _ => None,
                };
                chunk_markdown_segment(seg, heading, deadline, &mut out)?;
            }
        }
        FileType::Pdf => {
            for seg in segments {
                check_deadline(deadline)?;
                let page = match seg.kind {
                    SegmentKind::Page(p) => p as usize,
                    _ => seg.start_line as usize,
                };
                chunk_pdf_page(&seg.text, page, deadline, &mut out)?;
            }
        }
        FileType::Xlsx | FileType::Ods | FileType::Xls | FileType::Csv => {
            for seg in segments {
                check_deadline(deadline)?;
                chunk_sheet_segment(seg, deadline, &mut out)?;
            }
        }
        FileType::Text | FileType::Html | FileType::Docx | FileType::Odt => {
            for seg in segments {
                check_deadline(deadline)?;
                chunk_text_segment(seg, deadline, &mut out)?;
            }
        }
    }

    check_deadline(deadline)?;
    for (idx, c) in out.iter_mut().enumerate() {
        c.ord = idx;
    }
    Ok(out)
}

fn tree_sitter_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "nix" => Some(tree_sitter_nix::LANGUAGE.into()),
        "c" => Some(tree_sitter_c::LANGUAGE.into()),
        "cpp" => Some(tree_sitter_cpp::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
        "toml" => Some(tree_sitter_toml_ng::LANGUAGE.into()),
        "yaml" => Some(tree_sitter_yaml::LANGUAGE.into()),
        "json" => Some(tree_sitter_json::LANGUAGE.into()),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct PendingDecl {
    span_start: usize,
    end_byte: usize,
    decl_start: usize,
    decl_end: usize,
    symbol: Option<String>,
}

struct PackState {
    span_start: usize,
    span_end: usize,
    decl_start: usize,
    decl_end: usize,
    words: usize,
    symbol: Option<String>,
}

impl PackState {
    fn flush(self, source: &str, line_starts: &[usize], out: &mut Vec<Chunk>) {
        let text = &source[self.span_start..self.span_end];
        if text.is_empty() {
            return;
        }
        let start_line = line_at_byte(line_starts, self.span_start);
        let end_line = line_at_byte(
            line_starts,
            last_non_newline_byte(source, self.span_start, self.span_end),
        )
        .max(start_line);
        out.push(Chunk {
            ord: 0,
            kind: "code".into(),
            start: start_line,
            end: end_line,
            decl_start: Some(self.decl_start),
            decl_end: Some(self.decl_end),
            heading_or_symbol: self.symbol,
            text: text.to_string(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn feed_decl(
    d: PendingDecl,
    span_end: usize,
    source: &str,
    line_starts: &[usize],
    deadline: Instant,
    pack: &mut Option<PackState>,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    let decl_text = &source[d.span_start..span_end];
    let decl_words = count_words(decl_text);
    let decl_tokens = tokens_from_counts(decl_words, decl_text.len());

    if decl_tokens > TARGET_TOKENS {
        if let Some(p) = pack.take() {
            p.flush(source, line_starts, out);
        }
        split_code_range(
            source,
            line_starts,
            d.span_start,
            span_end,
            d.decl_start,
            d.decl_end,
            d.symbol,
            deadline,
            out,
        )?;
        return Ok(());
    }

    if let Some(p) = pack.as_mut() {
        let combined_words = combine_words_at_boundary(
            source,
            p.span_start,
            d.span_start,
            span_end,
            p.words,
            decl_words,
        );
        let combined_bytes = span_end - p.span_start;
        if tokens_from_counts(combined_words, combined_bytes) <= TARGET_TOKENS {
            p.span_end = span_end;
            p.decl_end = d.decl_end;
            p.words = combined_words;
            if p.symbol.is_none() {
                p.symbol = d.symbol;
            }
            return Ok(());
        }
        let finished = pack.take().unwrap();
        finished.flush(source, line_starts, out);
    }

    *pack = Some(PackState {
        span_start: d.span_start,
        span_end,
        decl_start: d.decl_start,
        decl_end: d.decl_end,
        words: decl_words,
        symbol: d.symbol,
    });
    Ok(())
}

fn chunk_code(
    source: &str,
    lang: &str,
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    let line_starts = compute_line_starts(source);
    let total_lines = line_starts.len();

    let tree = parse_code_tree(source, lang, deadline)?;
    let Some(tree) = tree else {
        split_code_range(
            source,
            &line_starts,
            0,
            source.len(),
            1,
            total_lines,
            None,
            deadline,
            out,
        )?;
        return Ok(());
    };

    let any_emitted =
        collect_declaration_nodes(tree.root_node(), source, &line_starts, deadline, out)?;
    drop(tree);

    if !any_emitted {
        split_code_range(
            source,
            &line_starts,
            0,
            source.len(),
            1,
            total_lines,
            None,
            deadline,
            out,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn split_code_range(
    source: &str,
    line_starts: &[usize],
    span_start: usize,
    span_end: usize,
    decl_start: usize,
    decl_end: usize,
    symbol: Option<String>,
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    if span_start >= span_end {
        return Ok(());
    }
    let slice = &source[span_start..span_end];
    if approx_tokens(slice) <= TARGET_TOKENS {
        let start_line = line_at_byte(line_starts, span_start);
        let end_line = line_at_byte(
            line_starts,
            last_non_newline_byte(source, span_start, span_end),
        )
        .max(start_line);
        out.push(Chunk {
            ord: 0,
            kind: "code".into(),
            start: start_line,
            end: end_line,
            decl_start: Some(decl_start),
            decl_end: Some(decl_end),
            heading_or_symbol: symbol,
            text: slice.to_string(),
        });
        return Ok(());
    }

    let first_line = line_at_byte(line_starts, span_start);
    let last_line = line_at_byte(line_starts, span_end.saturating_sub(1)).max(first_line);

    let mut part_start = span_start;
    let mut part_end = span_start;
    let mut part_words = 0usize;
    let mut part_first_line = first_line;
    let mut part_last_line = first_line;

    for (step, line_no) in (first_line..=last_line).enumerate() {
        if step & 0x3FF == 0 {
            check_deadline(deadline)?;
        }
        let l_start = line_starts[line_no - 1].max(span_start);
        let l_end = if line_no < line_starts.len() {
            line_starts[line_no].min(span_end)
        } else {
            span_end
        };
        if l_start >= l_end {
            continue;
        }

        let single_line = &source[l_start..l_end];
        let line_words = count_words(single_line);
        let line_tokens = tokens_from_counts(line_words, single_line.len());

        if part_end > part_start {
            let cand_words = combine_words_at_boundary(
                source, part_start, l_start, l_end, part_words, line_words,
            );
            let cand_bytes = l_end - part_start;
            if tokens_from_counts(cand_words, cand_bytes) > TARGET_TOKENS {
                out.push(Chunk {
                    ord: 0,
                    kind: "code".into(),
                    start: part_first_line,
                    end: part_last_line,
                    decl_start: Some(decl_start),
                    decl_end: Some(decl_end),
                    heading_or_symbol: symbol.clone(),
                    text: source[part_start..part_end].to_string(),
                });
                part_start = l_start;
                part_words = 0;
                part_first_line = line_no;
            }
        }

        if part_start == l_start && line_tokens > TARGET_TOKENS {
            split_long_line(
                single_line,
                "code",
                line_no,
                Some(decl_start),
                Some(decl_end),
                symbol.clone(),
                deadline,
                out,
            )?;
            part_start = l_end;
            part_end = l_end;
            part_words = 0;
            part_first_line = line_no + 1;
            part_last_line = line_no + 1;
        } else {
            part_words = combine_words_at_boundary(
                source, part_start, l_start, l_end, part_words, line_words,
            );
            part_end = l_end;
            part_last_line = line_no;
        }
    }

    if part_end > part_start {
        out.push(Chunk {
            ord: 0,
            kind: "code".into(),
            start: part_first_line,
            end: part_last_line.max(part_first_line),
            decl_start: Some(decl_start),
            decl_end: Some(decl_end),
            heading_or_symbol: symbol,
            text: source[part_start..part_end].to_string(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn split_long_line(
    line: &str,
    kind: &str,
    line_no: usize,
    decl_start: Option<usize>,
    decl_end: Option<usize>,
    symbol: Option<String>,
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    let mut part_start = 0usize;
    let mut part_end = 0usize;
    let mut part_words = 0usize;
    let mut i = 0usize;
    let mut step = 0u32;

    while i < line.len() {
        step = step.wrapping_add(1);
        if step & 0x3FF == 0 {
            check_deadline(deadline)?;
        }

        let ws1 = advance_while(line, i, char::is_whitespace);
        let word_end = advance_while(line, ws1, |c| !c.is_whitespace());
        let next_i = advance_while(line, word_end, |c| c == ' ' || c == '\t');
        let next_i = if next_i == i {
            // Trailing newline characters at the end of `line`.
            line.len()
        } else {
            next_i
        };

        let seg_words = usize::from(word_end > ws1);
        let seg_bytes = next_i - i;

        if tokens_from_counts(seg_words, seg_bytes) > TARGET_TOKENS {
            // Flush any pending words before this oversized segment.
            if part_end > part_start {
                out.push(Chunk {
                    ord: 0,
                    kind: kind.into(),
                    start: line_no,
                    end: line_no,
                    decl_start,
                    decl_end,
                    heading_or_symbol: symbol.clone(),
                    text: line[part_start..part_end].to_string(),
                });
            }

            // Split the oversized segment at UTF-8 character boundaries so no chunk exceeds
            // TARGET_TOKENS.
            let mut cur = i;
            while next_i - cur > MAX_CHUNK_BYTES {
                check_deadline(deadline)?;
                let cut = floor_char_boundary(line, cur + MAX_CHUNK_BYTES)
                    .max(cur + line[cur..].chars().next().map_or(1, char::len_utf8));
                out.push(Chunk {
                    ord: 0,
                    kind: kind.into(),
                    start: line_no,
                    end: line_no,
                    decl_start,
                    decl_end,
                    heading_or_symbol: symbol.clone(),
                    text: line[cur..cut].to_string(),
                });
                cur = cut;
            }

            part_start = cur;
            part_end = next_i;
            part_words = count_words(&line[cur..next_i]);
            i = next_i;
            continue;
        }

        if part_end > part_start {
            let cand_words =
                combine_words_at_boundary(line, part_start, i, next_i, part_words, seg_words);
            let cand_bytes = next_i - part_start;
            if tokens_from_counts(cand_words, cand_bytes) > TARGET_TOKENS {
                out.push(Chunk {
                    ord: 0,
                    kind: kind.into(),
                    start: line_no,
                    end: line_no,
                    decl_start,
                    decl_end,
                    heading_or_symbol: symbol.clone(),
                    text: line[part_start..part_end].to_string(),
                });
                part_start = i;
                part_end = next_i;
                part_words = seg_words;
            } else {
                part_end = next_i;
                part_words = cand_words;
            }
        } else {
            part_start = i;
            part_end = next_i;
            part_words = seg_words;
        }

        i = next_i;
    }

    if part_end > part_start {
        out.push(Chunk {
            ord: 0,
            kind: kind.into(),
            start: line_no,
            end: line_no,
            decl_start,
            decl_end,
            heading_or_symbol: symbol,
            text: line[part_start..part_end].to_string(),
        });
    }
    Ok(())
}

#[inline]
fn advance_while<F: Fn(char) -> bool>(s: &str, start: usize, pred: F) -> usize {
    let mut idx = start;
    for ch in s[start..].chars() {
        if pred(ch) {
            idx += ch.len_utf8();
        } else {
            break;
        }
    }
    idx
}

#[inline]
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn parse_code_tree(
    source: &str,
    lang: &str,
    deadline: Instant,
) -> Result<Option<tree_sitter::Tree>, ExtractError> {
    check_deadline(deadline)?;
    let Some(language) = tree_sitter_language(lang) else {
        return Ok(None);
    };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Ok(None);
    }

    let mut timed_out = false;
    let mut cb_ticks = 0u32;
    let mut progress_cb = |_: &ParseState| {
        cb_ticks = cb_ticks.wrapping_add(1);
        if cb_ticks & 0x3FF == 0 && Instant::now() >= deadline {
            timed_out = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = ParseOptions::new().progress_callback(&mut progress_cb);
    let bytes = source.as_bytes();
    let len = bytes.len();
    let tree = parser.parse_with_options(
        &mut |i, _| {
            if i < len { &bytes[i..] } else { &[] }
        },
        None,
        Some(options),
    );
    if timed_out || Instant::now() >= deadline {
        return Err(ExtractError::Timeout);
    }
    Ok(tree)
}

fn collect_declaration_nodes(
    root: Node<'_>,
    source: &str,
    line_starts: &[usize],
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<bool, ExtractError> {
    let total_lines = line_starts.len();
    let mut cursor = root.walk();
    let mut ticks = 0u32;
    let mut prev: Option<PendingDecl> = None;
    let mut pack: Option<PackState> = None;
    let mut any_emitted = false;
    let mut line_idx = 0usize;

    loop {
        ticks = ticks.wrapping_add(1);
        if ticks & 0xFFF == 0 {
            check_deadline(deadline)?;
        }

        let node = cursor.node();
        let mut descended = false;
        let mut emit_node = false;
        let is_leaf = node.child_count() == 0;

        if is_leaf {
            if !(node.is_extra() && is_comment_kind(node.kind())) {
                emit_node = true;
            }
        } else {
            let kind = node.kind();
            if kind != "formals" {
                if is_wrapper_node(kind) && cursor.goto_first_child() {
                    while !cursor.node().is_named() {
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                    if cursor.node().is_named() {
                        descended = true;
                    } else {
                        cursor.goto_parent();
                    }
                }
                if !descended && !(node.is_extra() && is_comment_kind(kind)) {
                    emit_node = true;
                }
            }
        }

        if emit_node {
            let start_byte = node.start_byte();
            let end_byte = node.end_byte().max(start_byte);
            if line_idx > 0 && start_byte < line_starts[line_idx] {
                line_idx = line_at_byte(line_starts, start_byte) - 1;
            } else {
                while line_idx + 1 < total_lines && start_byte >= line_starts[line_idx + 1] {
                    line_idx += 1;
                }
            }
            let decl_start = line_idx + 1;
            let decl_end = if line_idx + 1 >= total_lines || end_byte <= line_starts[line_idx + 1] {
                decl_start
            } else {
                let last_byte = last_non_ws_byte(source, start_byte, end_byte);
                let end_line = line_at_byte(line_starts, last_byte).max(decl_start);
                line_idx = end_line - 1;
                end_line
            };
            let symbol = if is_leaf {
                None
            } else {
                extract_symbol(node, source)
            };

            let span_start = if let Some(prev_decl) = prev.take() {
                let boundary = line_starts[decl_start - 1].max(prev_decl.end_byte);
                feed_decl(
                    prev_decl,
                    boundary,
                    source,
                    line_starts,
                    deadline,
                    &mut pack,
                    out,
                )?;
                boundary
            } else {
                0
            };

            prev = Some(PendingDecl {
                span_start,
                end_byte,
                decl_start,
                decl_end,
                symbol,
            });
            any_emitted = true;
        }

        if descended {
            continue;
        }

        loop {
            if cursor.goto_next_sibling() {
                if cursor.node().is_named() {
                    break;
                }
                continue;
            }
            if !cursor.goto_parent() {
                if let Some(last_decl) = prev.take() {
                    feed_decl(
                        last_decl,
                        source.len(),
                        source,
                        line_starts,
                        deadline,
                        &mut pack,
                        out,
                    )?;
                }
                if let Some(p) = pack.take() {
                    p.flush(source, line_starts, out);
                }
                return Ok(any_emitted);
            }
        }
    }
}

fn is_wrapper_node(kind: &str) -> bool {
    matches!(
        kind,
        "source_file"
            | "source_code"
            | "module"
            | "program"
            | "translation_unit"
            | "document"
            | "stream"
            | "source_expression"
            | "function_expression"
            | "attrset_expression"
            | "rec_attrset_expression"
            | "let_expression"
            | "binding_set"
            | "block_node"
            | "block_mapping"
            | "block_sequence"
            | "object"
            | "array"
    )
}

fn is_comment_kind(kind: &str) -> bool {
    matches!(kind, "comment" | "line_comment" | "block_comment")
}

fn extract_symbol(root: Node<'_>, source: &str) -> Option<String> {
    if root.named_child_count() == 0 {
        return None;
    }

    let mut stack: Vec<(Node<'_>, usize)> = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth > 3 || node.named_child_count() == 0 {
            continue;
        }
        let kind = node.kind();
        if matches!(
            kind,
            "use_declaration"
                | "import_statement"
                | "import_from_statement"
                | "import_declaration"
                | "preproc_include"
                | "package_clause"
                | "package_declaration"
                | "comment"
                | "line_comment"
                | "block_comment"
        ) {
            continue;
        }

        if let Some(name_node) = node.child_by_field_name("name")
            && let Some(sym) = clean_symbol_text(name_node, source)
        {
            return Some(sym);
        }

        if kind == "impl_item"
            && let Some(type_node) = node.child_by_field_name("type")
            && let Some(sym) = clean_symbol_text(type_node, source)
        {
            return Some(sym);
        }

        if let Some(decl_node) = node.child_by_field_name("declarator")
            && let Some(sym) = extract_declarator_symbol(decl_node, source)
        {
            return Some(sym);
        }

        if let Some(attrpath) = node.child_by_field_name("attrpath")
            && let Some(sym) = clean_symbol_text(attrpath, source)
        {
            return Some(sym);
        }

        if let Some(key_node) = node.child_by_field_name("key")
            && let Some(sym) = clean_symbol_text(key_node, source)
        {
            return Some(sym);
        }

        if matches!(
            kind,
            "table" | "table_array_element" | "pair" | "binding" | "block_mapping_pair"
        ) {
            let mut cursor = node.walk();
            if let Some(first_child) = node.named_children(&mut cursor).next()
                && let Some(sym) = clean_symbol_text(first_child, source)
            {
                return Some(sym);
            }
        }

        if matches!(
            kind,
            "export_statement"
                | "decorated_definition"
                | "type_declaration"
                | "lexical_declaration"
                | "variable_declaration"
                | "const_declaration"
                | "var_declaration"
                | "declaration"
                | "template_declaration"
        ) {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }

    None
}

fn extract_declarator_symbol(node: Node<'_>, source: &str) -> Option<String> {
    let mut cur = node;
    for _ in 0..8 {
        if matches!(
            cur.kind(),
            "identifier"
                | "field_identifier"
                | "type_identifier"
                | "qualified_identifier"
                | "destructor_name"
                | "operator_name"
        ) {
            return clean_symbol_text(cur, source);
        }
        if let Some(next) = cur.child_by_field_name("declarator") {
            cur = next;
            continue;
        }
        if let Some(next) = cur.child_by_field_name("name") {
            return clean_symbol_text(next, source);
        }
        let mut cursor = cur.walk();
        if let Some(id) = cur.named_children(&mut cursor).find(|c| {
            matches!(
                c.kind(),
                "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier"
            )
        }) {
            return clean_symbol_text(id, source);
        }
        break;
    }
    None
}

fn clean_symbol_text(node: Node<'_>, source: &str) -> Option<String> {
    let raw = source.get(node.start_byte()..node.end_byte())?.trim();
    let unquoted = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(raw)
        .trim();
    if unquoted.is_empty() || unquoted.contains('\n') {
        None
    } else {
        Some(unquoted.to_string())
    }
}

fn chunk_markdown_segment(
    seg: &Segment,
    heading: Option<String>,
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    if seg.text.trim().is_empty() {
        return Ok(());
    }
    if approx_tokens(&seg.text) <= TARGET_TOKENS {
        out.push(Chunk {
            ord: 0,
            kind: "section".into(),
            start: seg.start_line as usize,
            end: seg.end_line.max(seg.start_line) as usize,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: heading,
            text: seg.text.clone(),
        });
        return Ok(());
    }

    let splitter = MarkdownSplitter::new(ChunkConfig::new(TARGET_TOKENS).with_sizer(TokenSizer));
    let line_starts = compute_line_starts(&seg.text);
    for (byte_offset, sub_text) in splitter.chunk_indices(&seg.text) {
        check_deadline(deadline)?;
        if sub_text.trim().is_empty() {
            continue;
        }
        let rel_start = line_at_byte(&line_starts, byte_offset);
        let rel_end = line_at_byte(
            &line_starts,
            last_non_newline_byte(&seg.text, byte_offset, byte_offset + sub_text.len()),
        )
        .max(rel_start);
        let start = (seg.start_line as usize) + rel_start - 1;
        let end = ((seg.start_line as usize) + rel_end - 1).max(start);
        out.push(Chunk {
            ord: 0,
            kind: "section".into(),
            start,
            end,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: heading.clone(),
            text: sub_text.to_string(),
        });
    }
    Ok(())
}

fn chunk_text_segment(
    seg: &Segment,
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    if seg.text.trim().is_empty() {
        return Ok(());
    }
    let (kind, heading) = match &seg.kind {
        SegmentKind::Section { heading } => ("section", heading.clone()),
        _ => ("text", None),
    };
    if approx_tokens(&seg.text) <= TARGET_TOKENS {
        out.push(Chunk {
            ord: 0,
            kind: kind.into(),
            start: seg.start_line as usize,
            end: seg.end_line.max(seg.start_line) as usize,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: heading,
            text: seg.text.clone(),
        });
        return Ok(());
    }

    let splitter = TextSplitter::new(ChunkConfig::new(TARGET_TOKENS).with_sizer(TokenSizer));
    let line_starts = compute_line_starts(&seg.text);
    for (byte_offset, sub_text) in splitter.chunk_indices(&seg.text) {
        check_deadline(deadline)?;
        if sub_text.trim().is_empty() {
            continue;
        }
        let rel_start = line_at_byte(&line_starts, byte_offset);
        let rel_end = line_at_byte(
            &line_starts,
            last_non_newline_byte(&seg.text, byte_offset, byte_offset + sub_text.len()),
        )
        .max(rel_start);
        let start = (seg.start_line as usize) + rel_start - 1;
        let end = ((seg.start_line as usize) + rel_end - 1).max(start);
        out.push(Chunk {
            ord: 0,
            kind: kind.into(),
            start,
            end,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: heading.clone(),
            text: sub_text.to_string(),
        });
    }
    Ok(())
}

fn chunk_pdf_page(
    text: &str,
    page: usize,
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    if text.trim().is_empty() {
        return Ok(());
    }
    if approx_tokens(text) <= TARGET_TOKENS {
        out.push(Chunk {
            ord: 0,
            kind: "page".into(),
            start: page,
            end: page,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: None,
            text: text.to_string(),
        });
        return Ok(());
    }

    let splitter = TextSplitter::new(ChunkConfig::new(TARGET_TOKENS).with_sizer(TokenSizer));
    for (_, sub_text) in splitter.chunk_indices(text) {
        check_deadline(deadline)?;
        if sub_text.trim().is_empty() {
            continue;
        }
        out.push(Chunk {
            ord: 0,
            kind: "page".into(),
            start: page,
            end: page,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: None,
            text: sub_text.to_string(),
        });
    }
    Ok(())
}

fn chunk_sheet_segment(
    seg: &Segment,
    deadline: Instant,
    out: &mut Vec<Chunk>,
) -> Result<(), ExtractError> {
    if seg.text.trim().is_empty() {
        return Ok(());
    }
    let (sheet_name, first_row, last_row, row_numbers) = match &seg.kind {
        SegmentKind::Sheet {
            name,
            first_row,
            last_row,
            row_numbers,
        } => (
            Some(name.clone()),
            *first_row as usize,
            *last_row as usize,
            row_numbers.as_slice(),
        ),
        _ => (
            None,
            seg.start_line as usize,
            seg.end_line as usize,
            &[][..],
        ),
    };

    let row_for_line = |line_idx: usize| -> usize {
        row_numbers
            .get(line_idx)
            .copied()
            .map(|r| r as usize)
            .unwrap_or_else(|| (first_row + line_idx).min(last_row.max(first_row)))
    };

    if approx_tokens(&seg.text) <= TARGET_TOKENS {
        let start = if row_numbers.is_empty() {
            first_row
        } else {
            row_for_line(0)
        };
        let end = if row_numbers.is_empty() {
            last_row.max(first_row)
        } else {
            row_for_line(row_numbers.len() - 1).max(start)
        };
        out.push(Chunk {
            ord: 0,
            kind: "sheet".into(),
            start,
            end,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: sheet_name,
            text: seg.text.clone(),
        });
        return Ok(());
    }

    // Linearly split row block into smaller row blocks using byte slices and a running word count.
    let text = &seg.text;
    let bytes = text.as_bytes();
    let mut part_byte_start: Option<usize> = None;
    let mut part_byte_end: usize = 0;
    let mut part_words: usize = 0;
    let mut part_first_idx: usize = 0;
    let mut part_last_idx: usize = 0;

    let mut pos = 0usize;
    let mut line_idx = 0usize;
    while pos < bytes.len() {
        if line_idx & 0x3FF == 0 {
            check_deadline(deadline)?;
        }
        let l_start = pos;
        while pos < bytes.len() && bytes[pos] != b'\n' {
            pos += 1;
        }
        let mut l_end = pos;
        if l_end > l_start && bytes[l_end - 1] == b'\r' {
            l_end -= 1;
        }
        if pos < bytes.len() && bytes[pos] == b'\n' {
            pos += 1;
        }

        let line = &text[l_start..l_end];
        let line_words = count_words(line);
        let line_tokens = tokens_from_counts(line_words, line.len());

        if let Some(b_start) = part_byte_start {
            let cand_words = part_words + line_words;
            let cand_bytes = l_end - b_start;
            if tokens_from_counts(cand_words, cand_bytes) > TARGET_TOKENS {
                let block_start = row_for_line(part_first_idx);
                let block_end = row_for_line(part_last_idx).max(block_start);
                out.push(Chunk {
                    ord: 0,
                    kind: "sheet".into(),
                    start: block_start,
                    end: block_end,
                    decl_start: None,
                    decl_end: None,
                    heading_or_symbol: sheet_name.clone(),
                    text: text[b_start..part_byte_end].to_string(),
                });
                part_byte_start = None;
                part_words = 0;
            }
        }

        if part_byte_start.is_none() && line_tokens > TARGET_TOKENS {
            let row_no = row_for_line(line_idx);
            split_long_line(
                line,
                "sheet",
                row_no,
                None,
                None,
                sheet_name.clone(),
                deadline,
                out,
            )?;
        } else if let Some(b_start) = part_byte_start {
            let _ = b_start;
            part_byte_end = l_end;
            part_words += line_words;
            part_last_idx = line_idx;
        } else {
            part_byte_start = Some(l_start);
            part_byte_end = l_end;
            part_words = line_words;
            part_first_idx = line_idx;
            part_last_idx = line_idx;
        }

        line_idx += 1;
    }

    if let Some(b_start) = part_byte_start {
        let block_start = row_for_line(part_first_idx);
        let block_end = row_for_line(part_last_idx).max(block_start);
        out.push(Chunk {
            ord: 0,
            kind: "sheet".into(),
            start: block_start,
            end: block_end,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: sheet_name,
            text: text[b_start..part_byte_end].to_string(),
        });
    }

    Ok(())
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' && idx + 1 < text.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

/// Returns the 1-based line number containing `byte_idx`.
fn line_at_byte(line_starts: &[usize], byte_idx: usize) -> usize {
    match line_starts.binary_search(&byte_idx) {
        Ok(idx) => idx + 1,
        Err(idx) => idx.max(1),
    }
}

fn last_non_ws_byte(text: &str, start: usize, end: usize) -> usize {
    let end = end.min(text.len());
    let start = start.min(end);
    let bytes = text.as_bytes();
    let mut i = end;
    while i > start && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i > start { i - 1 } else { start }
}

fn last_non_newline_byte(text: &str, start: usize, end: usize) -> usize {
    let end = end.min(text.len());
    let start = start.min(end);
    let bytes = text.as_bytes();
    let mut i = end;
    while i > start && (bytes[i - 1] == b'\n' || bytes[i - 1] == b'\r') {
        i -= 1;
    }
    if i > start { i - 1 } else { start }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::config::Limits;
    use crate::extract;

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn large_function_splits_with_full_decl_range_and_exact_source_bytes() {
        let mut source = String::from("pub fn large_computation(input: i64) -> i64 {\n");
        for i in 0..120 {
            source.push_str(&format!(
                "    let step_value_{i} = input.wrapping_add({i}).wrapping_mul(3);\n"
            ));
        }
        source.push_str("    input\n}\n");

        let total_lines = source.lines().count();
        assert!(approx_tokens(&source) > TARGET_TOKENS);

        let extracted = extract::extract(
            "src/calc.rs",
            FileType::Code("rust"),
            source.as_bytes(),
            &Limits::default(),
        )
        .unwrap();
        let chunks = chunk(&extracted.segments, extracted.file_type, far_deadline()).unwrap();

        assert!(
            chunks.len() >= 2,
            "expected large function to split into multiple chunks, got {}",
            chunks.len()
        );
        for (idx, c) in chunks.iter().enumerate() {
            assert_eq!(c.ord, idx);
            assert_eq!(c.kind, "code");
            assert_eq!(c.decl_start, Some(1));
            assert_eq!(c.decl_end, Some(total_lines));
            assert_eq!(c.heading_or_symbol.as_deref(), Some("large_computation"));
            assert!(source.contains(&c.text));
        }
        let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(reconstructed, source);
    }

    #[test]
    fn small_sibling_declarations_packed_and_stored_text_equals_source() {
        let source = "pub fn one() -> i32 {\n    1\n}\n\npub fn two() -> i32 {\n    2\n}\n";
        let extracted = extract::extract(
            "src/small.rs",
            FileType::Code("rust"),
            source.as_bytes(),
            &Limits::default(),
        )
        .unwrap();
        let chunks = chunk(&extracted.segments, extracted.file_type, far_deadline()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, source);
        assert_eq!(chunks[0].decl_start, Some(1));
        assert_eq!(chunks[0].decl_end, Some(7));
        assert_eq!(chunks[0].heading_or_symbol.as_deref(), Some("one"));
    }

    #[test]
    fn all_13_grammars_parse_and_extract_declarations() {
        let samples: &[(&str, &str, &str)] = &[
            ("rust", "fn alpha() {}\n", "alpha"),
            ("python", "def beta():\n    return 1\n", "beta"),
            ("go", "package p\nfunc gamma() {}\n", "gamma"),
            ("javascript", "function delta() { return 1; }\n", "delta"),
            (
                "typescript",
                "export function epsilon(x: number): number { return x; }\n",
                "epsilon",
            ),
            ("nix", "{ zeta = 42; }\n", "zeta"),
            ("c", "int eta(void) { return 0; }\n", "eta"),
            ("cpp", "int theta() { return 0; }\n", "theta"),
            ("java", "class Iota { void run() {} }\n", "Iota"),
            ("bash", "kappa() {\n  echo hi\n}\n", "kappa"),
            ("toml", "[lambda]\nkey = \"val\"\n", "lambda"),
            ("yaml", "mu:\n  nested: true\n", "mu"),
            ("json", "{\n  \"nu\": 123\n}\n", "nu"),
        ];

        for &(lang, code, expected_sym) in samples {
            let seg = Segment {
                kind: SegmentKind::Whole,
                start_line: 1,
                end_line: code.lines().count() as u32,
                text: code.to_string(),
            };
            let chunks = chunk(&[seg], FileType::Code(lang), far_deadline()).unwrap();
            assert!(!chunks.is_empty(), "empty chunks for {lang}");
            assert_eq!(
                chunks[0].heading_or_symbol.as_deref(),
                Some(expected_sym),
                "symbol mismatch for {lang}"
            );
            assert_eq!(chunks[0].text, code, "source mismatch for {lang}");
        }
    }

    #[test]
    fn markdown_sections_by_heading() {
        let md = b"# First Section\nIntro text here.\n\n## Second Section\nMore details here.\n";
        let extracted =
            extract::extract("doc.md", FileType::Markdown, md, &Limits::default()).unwrap();
        let chunks = chunk(&extracted.segments, extracted.file_type, far_deadline()).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, "section");
        assert_eq!(
            chunks[0].heading_or_symbol.as_deref(),
            Some("First Section")
        );
        assert!(chunks[0].text.contains("Intro text here."));
        assert_eq!(chunks[1].kind, "section");
        assert_eq!(
            chunks[1].heading_or_symbol.as_deref(),
            Some("Second Section")
        );
        assert!(chunks[1].text.contains("More details here."));
    }

    #[test]
    fn deep_json_60000_levels_chunks_without_stack_overflow() {
        let depth = 60_000;
        let mut json = String::with_capacity(depth * 4 + 4);
        for _ in 0..depth {
            json.push_str("[\n");
        }
        json.push_str("1\n");
        for _ in 0..depth {
            json.push_str("]\n");
        }
        let seg = Segment {
            kind: SegmentKind::Whole,
            start_line: 1,
            end_line: (depth * 2 + 1) as u32,
            text: json.clone(),
        };
        let chunks = chunk(&[seg], FileType::Code("json"), far_deadline()).unwrap();
        assert!(!chunks.is_empty());
        let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(reconstructed, json);
    }

    #[test]
    fn deadline_fires_during_chunking_returns_timeout() {
        let mut source = String::new();
        for i in 0..20_000 {
            source.push_str(&format!("pub fn func_{i}(x: i32) -> i32 {{ x + {i} }}\n"));
        }
        let seg = Segment {
            kind: SegmentKind::Whole,
            start_line: 1,
            end_line: 20_000,
            text: source,
        };
        let tiny_deadline = Instant::now() + Duration::from_micros(50);
        let err = chunk(&[seg], FileType::Code("rust"), tiny_deadline).unwrap_err();
        assert_eq!(err, ExtractError::Timeout);
    }

    #[test]
    fn oversized_whitespace_free_run_splits_at_utf8_boundaries_under_token_target() {
        // 20 KB string literal containing multi-byte UTF-8 characters ('é' = 2 bytes, '🦀' = 4 bytes)
        let repeated = "é🦀a".repeat(3500); // 3500 * 7 = 24,500 bytes (> 20 KB), no whitespace
        let source = format!("pub fn huge_literal() -> &'static str {{\n    \"{repeated}\"\n}}\n");
        let total_lines = source.lines().count();
        let seg = Segment {
            kind: SegmentKind::Whole,
            start_line: 1,
            end_line: total_lines as u32,
            text: source.clone(),
        };
        let chunks = chunk(&[seg], FileType::Code("rust"), far_deadline()).unwrap();
        assert!(
            chunks.len() >= 3,
            "expected oversized literal to split into >= 3 chunks, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                approx_tokens(&c.text) <= TARGET_TOKENS,
                "chunk exceeded TARGET_TOKENS: {} > {}",
                approx_tokens(&c.text),
                TARGET_TOKENS
            );
            assert_eq!(c.decl_start, Some(1));
            assert_eq!(c.decl_end, Some(total_lines));
            assert_eq!(c.heading_or_symbol.as_deref(), Some("huge_literal"));
        }
        let reconstructed: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(reconstructed, source);
    }

    #[test]
    fn sparse_sheet_split_reports_exact_row_numbers() {
        // Sparse CSV where only rows 1, 2, 100, 250, 500, 1000 are non-empty, and each row is ~200 tokens
        // so the segment splits into multiple chunks.
        let mut csv_data = String::new();
        let target_rows = [1usize, 2, 100, 250, 500, 1000];
        let row_payload: String = (0..160)
            .map(|i| format!("word_{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            approx_tokens(&row_payload) > 200 && approx_tokens(&row_payload) < 250,
            "unexpected row tokens: {}",
            approx_tokens(&row_payload)
        );

        let mut current_row = 1usize;
        for &r in &target_rows {
            while current_row < r {
                csv_data.push_str(",,\n");
                current_row += 1;
            }
            csv_data.push_str(&format!("row_{r},{row_payload},end_{r}\n"));
            current_row += 1;
        }

        let extracted = extract::extract(
            "sparse.csv",
            FileType::Csv,
            csv_data.as_bytes(),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(extracted.segments.len(), 1);

        let chunks = chunk(&extracted.segments, extracted.file_type, far_deadline()).unwrap();
        // Each row is ~211 tokens; 2 rows fit in 512 tokens (~422 tokens), 3 rows exceed 512 (~633 tokens).
        // So the 6 non-empty rows [1, 2, 100, 250, 500, 1000] split into 3 pairs:
        // Pair 0: rows 1..=2
        // Pair 1: rows 100..=250
        // Pair 2: rows 500..=1000
        assert_eq!(chunks.len(), 3);
        assert_eq!((chunks[0].start, chunks[0].end), (1, 2));
        assert!(chunks[0].text.contains("row_1") && chunks[0].text.contains("row_2"));
        assert_eq!((chunks[1].start, chunks[1].end), (100, 250));
        assert!(chunks[1].text.contains("row_100") && chunks[1].text.contains("row_250"));
        assert_eq!((chunks[2].start, chunks[2].end), (500, 1000));
        assert!(chunks[2].text.contains("row_500") && chunks[2].text.contains("row_1000"));
    }
}
