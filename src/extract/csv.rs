use super::{ExtractError, Extracted, FileType, Segment, SegmentKind, TextBudget, file_name};

const ROWS_PER_SEGMENT: usize = 50;

pub fn extract(
    relpath: &str,
    bytes: &[u8],
    max_text_bytes: usize,
) -> Result<Extracted, ExtractError> {
    let title = file_name(relpath);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);

    let mut budget = TextBudget::new(max_text_bytes);
    let mut segments = Vec::new();
    let mut global_line: u32 = 0;
    let mut chunk_first_row: u32 = 0;
    let mut chunk_last_row: u32 = 0;
    let mut chunk_rows: u32 = 0;
    let mut chunk_row_numbers: Vec<u32> = Vec::with_capacity(ROWS_PER_SEGMENT);
    let mut chunk_text = String::new();
    let mut row_buf = String::new();

    for (idx, record) in rdr.records().enumerate() {
        let rec = record.map_err(|e| ExtractError::Parse(format!("csv error: {e}")))?;
        row_buf.clear();
        for (col_idx, field) in rec.iter().enumerate() {
            if col_idx > 0 {
                row_buf.push('\t');
            }
            if row_buf.len().saturating_add(field.len()) > budget.remaining() {
                return Err(ExtractError::TooLarge);
            }
            for ch in field.chars() {
                row_buf.push(match ch {
                    '\r' | '\n' | '\t' => ' ',
                    other => other,
                });
            }
        }
        if row_buf.trim().is_empty() {
            continue;
        }
        let row_no = (idx + 1) as u32;
        if chunk_rows == 0 {
            chunk_first_row = row_no;
        } else {
            budget.push_char(&mut chunk_text, '\n')?;
        }
        budget.push_str(&mut chunk_text, &row_buf)?;
        chunk_last_row = row_no;
        chunk_row_numbers.push(row_no);
        chunk_rows += 1;

        if chunk_rows as usize == ROWS_PER_SEGMENT {
            let start_line = global_line + 1;
            global_line += chunk_rows;
            segments.push(Segment {
                kind: SegmentKind::Sheet {
                    name: crate::sanitize::sanitize(&title),
                    first_row: chunk_first_row,
                    last_row: chunk_last_row,
                    row_numbers: std::mem::take(&mut chunk_row_numbers),
                },
                start_line,
                end_line: global_line,
                text: std::mem::take(&mut chunk_text),
            });
            chunk_rows = 0;
        }
    }

    if chunk_rows > 0 {
        let start_line = global_line + 1;
        global_line += chunk_rows;
        segments.push(Segment {
            kind: SegmentKind::Sheet {
                name: crate::sanitize::sanitize(&title),
                first_row: chunk_first_row,
                last_row: chunk_last_row,
                row_numbers: chunk_row_numbers,
            },
            start_line,
            end_line: global_line,
            text: chunk_text,
        });
    }

    Ok(Extracted {
        title,
        file_type: FileType::Csv,
        segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_sheet_name_is_sanitized() {
        let extracted = extract("a\u{200b}b.csv", b"value\n", 100).unwrap();
        assert!(
            matches!(&extracted.segments[0].kind, SegmentKind::Sheet { name, .. } if name == "ab.csv")
        );
    }
}
