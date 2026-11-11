use std::io::Cursor;

use calamine::{Data, Ods, Reader, Xls, Xlsx};

use super::{
    ExtractError, Extracted, FileType, Segment, SegmentKind, Skip, file_name, verify_zip_guard,
};

const ROWS_PER_SEGMENT: usize = 50;

pub fn extract(
    relpath: &str,
    file_type: FileType,
    bytes: &[u8],
) -> Result<Extracted, ExtractError> {
    let worksheets = match file_type {
        FileType::Xlsx => {
            verify_zip_guard(bytes, "[Content_Types].xml")?;
            let mut wb: Xlsx<_> = Xlsx::new(Cursor::new(bytes))
                .map_err(|e| ExtractError::Parse(format!("xlsx error: {e}")))?;
            wb.worksheets()
        }
        FileType::Ods => {
            verify_zip_guard(bytes, "mimetype")?;
            let mut wb: Ods<_> = Ods::new(Cursor::new(bytes))
                .map_err(|e| ExtractError::Parse(format!("ods error: {e}")))?;
            wb.worksheets()
        }
        FileType::Xls => {
            if !bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
                return Err(ExtractError::Skipped(Skip::Mismatch));
            }
            let mut wb: Xls<_> = Xls::new(Cursor::new(bytes))
                .map_err(|e| ExtractError::Parse(format!("xls error: {e}")))?;
            wb.worksheets()
        }
        _ => return Err(ExtractError::Skipped(Skip::Unsupported)),
    };

    let title = file_name(relpath);
    let mut segments = Vec::new();
    let mut global_line: u32 = 0;

    for (sheet_name, range) in worksheets {
        let mut numbered_rows: Vec<(u32, String)> = Vec::new();
        let row_offset = range.start().map(|(r, _)| r).unwrap_or(0);

        for (idx, row) in range.rows().enumerate() {
            if row.iter().all(|cell| matches!(cell, Data::Empty)) {
                continue;
            }
            let row_num = row_offset + (idx as u32) + 1;
            let text = row
                .iter()
                .map(format_cell)
                .collect::<Vec<_>>()
                .join("\t")
                .trim_end()
                .to_string();
            if !text.is_empty() {
                numbered_rows.push((row_num, text));
            }
        }

        for chunk in numbered_rows.chunks(ROWS_PER_SEGMENT) {
            let first_row = chunk.first().map(|(r, _)| *r).unwrap_or(1);
            let last_row = chunk.last().map(|(r, _)| *r).unwrap_or(first_row);
            let start_line = global_line + 1;
            global_line += chunk.len() as u32;
            let end_line = global_line;
            let text = chunk
                .iter()
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            segments.push(Segment {
                kind: SegmentKind::Sheet {
                    name: sheet_name.clone(),
                    first_row,
                    last_row,
                },
                start_line,
                end_line,
                text,
            });
        }
    }

    Ok(Extracted {
        title,
        file_type,
        segments,
    })
}

fn format_cell(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.replace(['\r', '\n', '\t'], " "),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("{e:?}"),
    }
}
