use text_splitter::{ChunkConfig, ChunkSizer, MarkdownSplitter, TextSplitter};
use tree_sitter::{Language, Node, Parser};

use crate::extract::{FileType, Segment, SegmentKind};
use crate::proto::Chunk;

pub const TARGET_TOKENS: usize = 512;

/// Approximates token count as whitespace-separated words × 1.3 (with a byte-length floor
/// for long whitespace-free sequences).
pub fn approx_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    let by_words = ((words as f64) * 1.3).ceil() as usize;
    by_words.max(text.len() / 16)
}

#[derive(Clone, Copy)]
struct TokenSizer;

impl ChunkSizer for TokenSizer {
    fn size(&self, chunk: &str) -> usize {
        approx_tokens(chunk).max(1)
    }
}

pub fn chunk<S: AsRef<[Segment]>>(segments: S, file_type: FileType) -> Vec<Chunk> {
    let segments = segments.as_ref();
    let mut out = Vec::new();

    match file_type {
        FileType::Code(lang) => {
            for seg in segments {
                if !seg.text.is_empty() {
                    chunk_code(&seg.text, lang, &mut out);
                }
            }
        }
        FileType::Markdown => {
            for seg in segments {
                let heading = match &seg.kind {
                    SegmentKind::Section { heading } => heading.clone(),
                    _ => None,
                };
                chunk_markdown_segment(seg, heading, &mut out);
            }
        }
        FileType::Pdf => {
            for seg in segments {
                let page = match seg.kind {
                    SegmentKind::Page(p) => p as usize,
                    _ => seg.start_line as usize,
                };
                chunk_pdf_page(&seg.text, page, &mut out);
            }
        }
        FileType::Xlsx | FileType::Ods | FileType::Xls | FileType::Csv => {
            for seg in segments {
                chunk_sheet_segment(seg, &mut out);
            }
        }
        FileType::Text | FileType::Html | FileType::Docx | FileType::Odt => {
            for seg in segments {
                chunk_text_segment(seg, &mut out);
            }
        }
    }

    for (idx, c) in out.iter_mut().enumerate() {
        c.ord = idx;
    }
    out
}

fn tree_sitter_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
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
struct DeclSpan {
    span_start: usize,
    span_end: usize,
    decl_start: usize,
    decl_end: usize,
    symbol: Option<String>,
}

fn chunk_code(source: &str, lang: &str, out: &mut Vec<Chunk>) {
    let line_starts = compute_line_starts(source);
    let total_lines = line_starts.len();

    let tree = parse_code_tree(source, lang);
    let Some(tree) = tree else {
        split_code_range(
            source,
            &line_starts,
            0,
            source.len(),
            1,
            total_lines,
            None,
            out,
        );
        return;
    };

    let mut raw_nodes = Vec::new();
    collect_declaration_nodes(tree.root_node(), &mut raw_nodes);

    // Filter out standalone comments so they attach to adjacent declarations, unless the file
    // is only comments.
    let non_comment_nodes: Vec<Node<'_>> = raw_nodes
        .iter()
        .copied()
        .filter(|n| !is_comment_kind(n.kind()))
        .collect();
    let decl_nodes = if non_comment_nodes.is_empty() {
        raw_nodes
    } else {
        non_comment_nodes
    };

    if decl_nodes.is_empty() {
        split_code_range(
            source,
            &line_starts,
            0,
            source.len(),
            1,
            total_lines,
            None,
            out,
        );
        return;
    }

    let mut decls = Vec::with_capacity(decl_nodes.len());
    for (idx, node) in decl_nodes.iter().copied().enumerate() {
        let start_byte = node.start_byte();
        let end_byte = node.end_byte().max(start_byte);
        let decl_start = line_at_byte(&line_starts, start_byte);
        let last_byte = last_non_ws_byte(source, start_byte, end_byte);
        let decl_end = line_at_byte(&line_starts, last_byte).max(decl_start);
        let symbol = extract_symbol(node, source);

        let span_start = if idx == 0 {
            0
        } else {
            line_start_at_or_before(&line_starts, start_byte)
        };
        decls.push(DeclSpan {
            span_start,
            span_end: end_byte,
            decl_start,
            decl_end,
            symbol,
        });
    }

    // Ensure contiguous, non-overlapping spans that partition `0..source.len()` cleanly.
    for i in 0..decls.len() {
        if i + 1 < decls.len() {
            let next_start = decls[i + 1].span_start.max(decls[i].span_end);
            decls[i].span_end = next_start;
            decls[i + 1].span_start = next_start;
        } else {
            decls[i].span_end = source.len();
        }
    }

    // Pack small sibling declarations up to ~TARGET_TOKENS; split declarations larger than
    // TARGET_TOKENS into parts that all carry the declaration's full decl_start..decl_end.
    let mut pack_start_idx: Option<usize> = None;
    let mut pack_end_idx: usize = 0;

    let flush_pack = |start_idx: usize, end_idx: usize, out: &mut Vec<Chunk>| {
        let first = &decls[start_idx];
        let last = &decls[end_idx];
        let text = &source[first.span_start..last.span_end];
        if text.is_empty() {
            return;
        }
        let start_line = line_at_byte(&line_starts, first.span_start);
        let end_line = line_at_byte(
            &line_starts,
            last_non_newline_byte(source, first.span_start, last.span_end),
        )
        .max(start_line);
        let symbol = decls[start_idx..=end_idx]
            .iter()
            .find_map(|d| d.symbol.clone());
        out.push(Chunk {
            ord: 0,
            kind: "code".into(),
            start: start_line,
            end: end_line,
            decl_start: Some(first.decl_start),
            decl_end: Some(last.decl_end),
            heading_or_symbol: symbol,
            text: text.to_string(),
        });
    };

    for i in 0..decls.len() {
        let d = &decls[i];
        let decl_text = &source[d.span_start..d.span_end];
        let decl_tokens = approx_tokens(decl_text);

        if decl_tokens > TARGET_TOKENS {
            if let Some(s_idx) = pack_start_idx.take() {
                flush_pack(s_idx, pack_end_idx, out);
            }
            split_code_range(
                source,
                &line_starts,
                d.span_start,
                d.span_end,
                d.decl_start,
                d.decl_end,
                d.symbol.clone(),
                out,
            );
            continue;
        }

        if let Some(s_idx) = pack_start_idx {
            let combined_text = &source[decls[s_idx].span_start..d.span_end];
            if approx_tokens(combined_text) <= TARGET_TOKENS {
                pack_end_idx = i;
            } else {
                flush_pack(s_idx, pack_end_idx, out);
                pack_start_idx = Some(i);
                pack_end_idx = i;
            }
        } else {
            pack_start_idx = Some(i);
            pack_end_idx = i;
        }
    }

    if let Some(s_idx) = pack_start_idx.take() {
        flush_pack(s_idx, pack_end_idx, out);
    }
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
    out: &mut Vec<Chunk>,
) {
    if span_start >= span_end {
        return;
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
        return;
    }

    // Collect line byte boundaries within `span_start..span_end`.
    let first_line = line_at_byte(line_starts, span_start);
    let last_line = line_at_byte(line_starts, span_end.saturating_sub(1)).max(first_line);

    let mut part_start = span_start;
    let mut part_end = span_start;
    let mut part_first_line = first_line;
    let mut part_last_line = first_line;

    for line_no in first_line..=last_line {
        let l_start = line_starts[line_no - 1].max(span_start);
        let l_end = if line_no < line_starts.len() {
            line_starts[line_no].min(span_end)
        } else {
            span_end
        };
        if l_start >= l_end {
            continue;
        }

        let candidate = &source[part_start..l_end];
        if part_end > part_start && approx_tokens(candidate) > TARGET_TOKENS {
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
            part_first_line = line_no;
        }

        let single_line = &source[l_start..l_end];
        if part_start == l_start && approx_tokens(single_line) > TARGET_TOKENS {
            // Single line exceeds TARGET_TOKENS: split within the line on token boundaries.
            split_long_line_code(
                single_line,
                line_no,
                decl_start,
                decl_end,
                symbol.clone(),
                out,
            );
            part_start = l_end;
            part_end = l_end;
            part_first_line = line_no + 1;
            part_last_line = line_no + 1;
        } else {
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
}

fn split_long_line_code(
    line: &str,
    line_no: usize,
    decl_start: usize,
    decl_end: usize,
    symbol: Option<String>,
    out: &mut Vec<Chunk>,
) {
    let mut part_start = 0usize;
    let mut last_valid_end = 0usize;
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if last_valid_end > part_start && approx_tokens(&line[part_start..i]) > TARGET_TOKENS {
            out.push(Chunk {
                ord: 0,
                kind: "code".into(),
                start: line_no,
                end: line_no,
                decl_start: Some(decl_start),
                decl_end: Some(decl_end),
                heading_or_symbol: symbol.clone(),
                text: line[part_start..last_valid_end].to_string(),
            });
            part_start = last_valid_end;
        }
        last_valid_end = i;
    }

    if part_start < line.len() {
        out.push(Chunk {
            ord: 0,
            kind: "code".into(),
            start: line_no,
            end: line_no,
            decl_start: Some(decl_start),
            decl_end: Some(decl_end),
            heading_or_symbol: symbol,
            text: line[part_start..].to_string(),
        });
    }
}

fn parse_code_tree(source: &str, lang: &str) -> Option<tree_sitter::Tree> {
    let language = tree_sitter_language(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    if lang == "typescript" && tree.root_node().has_error() {
        let tsx: Language = tree_sitter_typescript::LANGUAGE_TSX.into();
        if parser.set_language(&tsx).is_ok()
            && let Some(tsx_tree) = parser.parse(source, None)
            && !tsx_tree.root_node().has_error()
        {
            return Some(tsx_tree);
        }
    }
    Some(tree)
}

fn collect_declaration_nodes<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    let kind = node.kind();
    if kind == "formals" {
        return;
    }
    if is_wrapper_node(kind) {
        let mut cursor = node.walk();
        let children: Vec<Node<'a>> = node.named_children(&mut cursor).collect();
        if !children.is_empty() {
            for child in children {
                collect_declaration_nodes(child, out);
            }
            return;
        }
    }
    out.push(node);
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

fn extract_symbol(node: Node<'_>, source: &str) -> Option<String> {
    extract_symbol_depth(node, source, 0)
}

fn extract_symbol_depth(node: Node<'_>, source: &str, depth: usize) -> Option<String> {
    if depth > 3 {
        return None;
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
        return None;
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
        for child in node.named_children(&mut cursor) {
            if let Some(sym) = extract_symbol_depth(child, source, depth + 1) {
                return Some(sym);
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

fn chunk_markdown_segment(seg: &Segment, heading: Option<String>, out: &mut Vec<Chunk>) {
    if seg.text.trim().is_empty() {
        return;
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
        return;
    }

    let splitter = MarkdownSplitter::new(ChunkConfig::new(TARGET_TOKENS).with_sizer(TokenSizer));
    let line_starts = compute_line_starts(&seg.text);
    for (byte_offset, sub_text) in splitter.chunk_indices(&seg.text) {
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
}

fn chunk_text_segment(seg: &Segment, out: &mut Vec<Chunk>) {
    if seg.text.trim().is_empty() {
        return;
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
        return;
    }

    let splitter = TextSplitter::new(ChunkConfig::new(TARGET_TOKENS).with_sizer(TokenSizer));
    let line_starts = compute_line_starts(&seg.text);
    for (byte_offset, sub_text) in splitter.chunk_indices(&seg.text) {
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
}

fn chunk_pdf_page(text: &str, page: usize, out: &mut Vec<Chunk>) {
    if text.trim().is_empty() {
        return;
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
        return;
    }

    let splitter = TextSplitter::new(ChunkConfig::new(TARGET_TOKENS).with_sizer(TokenSizer));
    for (_, sub_text) in splitter.chunk_indices(text) {
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
}

fn chunk_sheet_segment(seg: &Segment, out: &mut Vec<Chunk>) {
    if seg.text.trim().is_empty() {
        return;
    }
    let (sheet_name, first_row, last_row) = match &seg.kind {
        SegmentKind::Sheet {
            name,
            first_row,
            last_row,
        } => (Some(name.clone()), *first_row as usize, *last_row as usize),
        _ => (None, seg.start_line as usize, seg.end_line as usize),
    };

    if approx_tokens(&seg.text) <= TARGET_TOKENS {
        out.push(Chunk {
            ord: 0,
            kind: "sheet".into(),
            start: first_row,
            end: last_row.max(first_row),
            decl_start: None,
            decl_end: None,
            heading_or_symbol: sheet_name,
            text: seg.text.clone(),
        });
        return;
    }

    // Split row block into smaller row blocks so each stays within TARGET_TOKENS.
    let lines: Vec<&str> = seg.text.lines().collect();
    let total_lines = lines.len().max(1);
    let row_span = last_row.saturating_sub(first_row);

    let mut cur_lines: Vec<&str> = Vec::new();
    let mut cur_start_idx = 0usize;

    for (idx, &line) in lines.iter().enumerate() {
        let candidate = if cur_lines.is_empty() {
            line.to_string()
        } else {
            format!("{}\n{line}", cur_lines.join("\n"))
        };
        if !cur_lines.is_empty() && approx_tokens(&candidate) > TARGET_TOKENS {
            let block_start = first_row + (cur_start_idx * row_span) / total_lines;
            let block_end = (first_row + ((idx - 1) * row_span) / total_lines).max(block_start);
            out.push(Chunk {
                ord: 0,
                kind: "sheet".into(),
                start: block_start,
                end: block_end,
                decl_start: None,
                decl_end: None,
                heading_or_symbol: sheet_name.clone(),
                text: cur_lines.join("\n"),
            });
            cur_lines.clear();
            cur_start_idx = idx;
        }
        cur_lines.push(line);
    }

    if !cur_lines.is_empty() {
        let block_start = first_row + (cur_start_idx * row_span) / total_lines;
        let block_end = last_row.max(block_start);
        out.push(Chunk {
            ord: 0,
            kind: "sheet".into(),
            start: block_start,
            end: block_end,
            decl_start: None,
            decl_end: None,
            heading_or_symbol: sheet_name,
            text: cur_lines.join("\n"),
        });
    }
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

fn line_start_at_or_before(line_starts: &[usize], byte_idx: usize) -> usize {
    let line = line_at_byte(line_starts, byte_idx);
    line_starts[line - 1]
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
    use crate::config::Limits;
    use crate::extract;

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
        let chunks = chunk(&extracted.segments, extracted.file_type);

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
        let chunks = chunk(&extracted.segments, extracted.file_type);
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
            let chunks = chunk(&[seg], FileType::Code(lang));
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
        let chunks = chunk(&extracted.segments, extracted.file_type);
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
}
