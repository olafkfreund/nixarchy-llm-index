use std::io::Cursor;

use calamine::{Data, DataRef, Ods, Range, Reader, Xls, Xlsx};

use crate::config::Limits;

use super::{
    ExtractError, Extracted, FileType, ODF_ODS_MIME, OLE2_MAGIC, OoxmlKind, Segment, SegmentKind,
    Skip, TextBudget, file_name, verify_odf_mimetype, verify_ooxml_content_type, verify_zip_guard,
};

const ROWS_PER_SEGMENT: usize = 50;
const MAX_LEGACY_SHEET_BYTES: usize = 20 * 1024 * 1024;

/// Extracts spreadsheet segments for `.xlsx`, `.ods`, and `.xls`.
///
/// For `.xlsx`, sheet dimensions and actual cell coordinates/shared-string sizes
/// are inspected via `worksheet_cells_reader` before materialising each sheet with
/// `worksheet_range(name)`, rejecting workbooks exceeding `limits.max_cells` or
/// `limits.max_text_bytes`.
///
/// For `.ods` and `.xls`, `calamine` loads every sheet when the workbook opens, so the
/// dimensions are only known afterwards: each range is added to a workbook total
/// (`add_range_cells`) against `limits.max_cells` before its rows are read. Input file size is
/// capped at 20 MiB and `limits.max_text_bytes` is enforced while iterating rows; any dense-range
/// allocation abort inside `calamine` is caught by the worker process boundary,
/// where the coordinator marks the offending file `Failed`.
pub fn extract(
    relpath: &str,
    file_type: FileType,
    bytes: &[u8],
    limits: &Limits,
) -> Result<Extracted, ExtractError> {
    let title = file_name(relpath);
    let mut budget = TextBudget::new(limits.max_text_bytes);
    let mut segments = Vec::new();
    let mut global_line: u32 = 0;

    match file_type {
        FileType::Xlsx => {
            verify_zip_guard(bytes)?;
            verify_ooxml_content_type(bytes, OoxmlKind::Xlsx)?;
            let mut wb: Xlsx<_> = Xlsx::new(Cursor::new(bytes))
                .map_err(|e| ExtractError::Parse(format!("xlsx error: {e}")))?;
            let mut total_workbook_cells: u64 = 0;

            for sheet_name in wb.sheet_names() {
                let sheet_cells = precheck_xlsx_sheet(
                    &mut wb,
                    &sheet_name,
                    total_workbook_cells,
                    limits.max_cells,
                    budget.used(),
                    limits.max_text_bytes,
                )?;
                total_workbook_cells = total_workbook_cells.saturating_add(sheet_cells);

                let range = wb.worksheet_range(&sheet_name).map_err(|e| {
                    ExtractError::Parse(format!("xlsx sheet '{sheet_name}' error: {e}"))
                })?;
                append_range_segments(
                    &sheet_name,
                    &range,
                    &mut budget,
                    &mut global_line,
                    &mut segments,
                )?;
            }
        }
        FileType::Ods => {
            if bytes.len() > MAX_LEGACY_SHEET_BYTES {
                return Err(ExtractError::TooLarge);
            }
            verify_zip_guard(bytes)?;
            verify_odf_mimetype(bytes, ODF_ODS_MIME)?;
            let mut wb: Ods<_> = Ods::new(Cursor::new(bytes))
                .map_err(|e| ExtractError::Parse(format!("ods error: {e}")))?;
            let mut total_cells: u64 = 0;
            for sheet_name in wb.sheet_names() {
                let range = wb.worksheet_range(&sheet_name).map_err(|e| {
                    ExtractError::Parse(format!("ods sheet '{sheet_name}' error: {e}"))
                })?;
                add_range_cells(&mut total_cells, &range, limits.max_cells)?;
                append_range_segments(
                    &sheet_name,
                    &range,
                    &mut budget,
                    &mut global_line,
                    &mut segments,
                )?;
            }
        }
        FileType::Xls => {
            if !bytes.starts_with(OLE2_MAGIC) {
                return Err(ExtractError::Skipped(Skip::Mismatch));
            }
            if bytes.len() > MAX_LEGACY_SHEET_BYTES {
                return Err(ExtractError::TooLarge);
            }
            let mut wb: Xls<_> = Xls::new(Cursor::new(bytes))
                .map_err(|e| ExtractError::Parse(format!("xls error: {e}")))?;
            let mut total_cells: u64 = 0;
            for sheet_name in wb.sheet_names() {
                let range = wb.worksheet_range(&sheet_name).map_err(|e| {
                    ExtractError::Parse(format!("xls sheet '{sheet_name}' error: {e}"))
                })?;
                add_range_cells(&mut total_cells, &range, limits.max_cells)?;
                append_range_segments(
                    &sheet_name,
                    &range,
                    &mut budget,
                    &mut global_line,
                    &mut segments,
                )?;
            }
        }
        _ => return Err(ExtractError::Skipped(Skip::Unsupported)),
    }

    Ok(Extracted {
        title,
        file_type,
        segments,
    })
}

/// Adds the full rectangle of `range` to the workbook total; `TooLarge` once it passes `max`.
/// ODS and XLS ranges are loaded when the workbook opens, so this bounds the work after that.
fn add_range_cells(total: &mut u64, range: &Range<Data>, max: u64) -> Result<(), ExtractError> {
    *total = total.saturating_add((range.height() as u64).saturating_mul(range.width() as u64));
    if *total > max {
        return Err(ExtractError::TooLarge);
    }
    Ok(())
}

fn precheck_xlsx_sheet(
    wb: &mut Xlsx<Cursor<&[u8]>>,
    sheet_name: &str,
    total_workbook_cells: u64,
    max_cells: u64,
    used_text_bytes: usize,
    max_text_bytes: usize,
) -> Result<u64, ExtractError> {
    let mut reader = wb
        .worksheet_cells_reader(sheet_name)
        .map_err(|e| ExtractError::Parse(format!("xlsx sheet '{sheet_name}' error: {e}")))?;

    let dims = reader.dimensions();
    let rows = (dims.end.0.saturating_sub(dims.start.0) as u64).saturating_add(1);
    let cols = (dims.end.1.saturating_sub(dims.start.1) as u64).saturating_add(1);
    let dim_cells = rows.saturating_mul(cols);
    if dim_cells > max_cells || total_workbook_cells.saturating_add(dim_cells) > max_cells {
        return Err(ExtractError::TooLarge);
    }

    let mut min_row = u32::MAX;
    let mut min_col = u32::MAX;
    let mut max_row = 0u32;
    let mut max_col = 0u32;
    let mut has_cells = false;
    let mut approx_text_bytes = used_text_bytes;

    while let Some(cell) = reader
        .next_cell()
        .map_err(|e| ExtractError::Parse(format!("xlsx sheet '{sheet_name}' error: {e}")))?
    {
        if matches!(cell.get_value(), DataRef::Empty) {
            continue;
        }
        let (r, c) = cell.get_position();
        min_row = min_row.min(r);
        min_col = min_col.min(c);
        max_row = max_row.max(r);
        max_col = max_col.max(c);
        has_cells = true;

        let span_rows = (max_row.saturating_sub(min_row) as u64).saturating_add(1);
        let span_cols = (max_col.saturating_sub(min_col) as u64).saturating_add(1);
        let span_cells = span_rows.saturating_mul(span_cols);
        if span_cells > max_cells || total_workbook_cells.saturating_add(span_cells) > max_cells {
            return Err(ExtractError::TooLarge);
        }

        let cell_bytes = match cell.get_value() {
            DataRef::SharedString(s) => s.len(),
            DataRef::String(s) | DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s.len(),
            DataRef::Int(_) | DataRef::Float(_) | DataRef::Bool(_) | DataRef::DateTime(_) => 1,
            DataRef::Error(_) | DataRef::Empty => 0,
        };
        approx_text_bytes = approx_text_bytes.saturating_add(cell_bytes);
        if max_text_bytes > 0 && approx_text_bytes > max_text_bytes {
            return Err(ExtractError::TooLarge);
        }
    }

    let actual_cells = if has_cells {
        let span_rows = (max_row.saturating_sub(min_row) as u64).saturating_add(1);
        let span_cols = (max_col.saturating_sub(min_col) as u64).saturating_add(1);
        span_rows.saturating_mul(span_cols)
    } else {
        0
    };

    Ok(dim_cells.max(actual_cells))
}

fn append_range_segments(
    sheet_name: &str,
    range: &Range<Data>,
    budget: &mut TextBudget,
    global_line: &mut u32,
    segments: &mut Vec<Segment>,
) -> Result<(), ExtractError> {
    let row_offset = range.start().map(|(r, _)| r).unwrap_or(0);
    let mut chunk_first_row: u32 = 0;
    let mut chunk_last_row: u32 = 0;
    let mut chunk_rows: u32 = 0;
    let mut chunk_row_numbers: Vec<u32> = Vec::with_capacity(ROWS_PER_SEGMENT);
    let mut chunk_text = String::new();
    let mut row_buf = String::new();

    for (idx, row) in range.rows().enumerate() {
        if row.iter().all(|cell| matches!(cell, Data::Empty)) {
            continue;
        }
        row_buf.clear();
        for (col_idx, cell) in row.iter().enumerate() {
            if col_idx > 0 {
                row_buf.push('\t');
            }
            format_cell_into(cell, &mut row_buf, budget.remaining())?;
        }
        let trimmed_len = row_buf.trim_end().len();
        row_buf.truncate(trimmed_len);
        if row_buf.is_empty() {
            continue;
        }

        let row_num = row_offset + (idx as u32) + 1;
        if chunk_rows == 0 {
            chunk_first_row = row_num;
        } else {
            budget.push_char(&mut chunk_text, '\n')?;
        }
        budget.push_str(&mut chunk_text, &row_buf)?;
        chunk_last_row = row_num;
        chunk_row_numbers.push(row_num);
        chunk_rows += 1;

        if chunk_rows as usize == ROWS_PER_SEGMENT {
            let start_line = *global_line + 1;
            *global_line += chunk_rows;
            segments.push(Segment {
                kind: SegmentKind::Sheet {
                    name: sheet_name.to_string(),
                    first_row: chunk_first_row,
                    last_row: chunk_last_row,
                    row_numbers: std::mem::take(&mut chunk_row_numbers),
                },
                start_line,
                end_line: *global_line,
                text: std::mem::take(&mut chunk_text),
            });
            chunk_rows = 0;
        }
    }

    if chunk_rows > 0 {
        let start_line = *global_line + 1;
        *global_line += chunk_rows;
        segments.push(Segment {
            kind: SegmentKind::Sheet {
                name: sheet_name.to_string(),
                first_row: chunk_first_row,
                last_row: chunk_last_row,
                row_numbers: chunk_row_numbers,
            },
            start_line,
            end_line: *global_line,
            text: chunk_text,
        });
    }

    Ok(())
}

fn format_cell_into(
    cell: &Data,
    out: &mut String,
    remaining_budget: usize,
) -> Result<(), ExtractError> {
    match cell {
        Data::Empty => {}
        Data::String(s) => {
            if out.len().saturating_add(s.len()) > remaining_budget {
                return Err(ExtractError::TooLarge);
            }
            for ch in s.chars() {
                out.push(match ch {
                    '\r' | '\n' | '\t' => ' ',
                    other => other,
                });
            }
        }
        Data::Float(f) => {
            let formatted = if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            };
            if out.len().saturating_add(formatted.len()) > remaining_budget {
                return Err(ExtractError::TooLarge);
            }
            out.push_str(&formatted);
        }
        Data::Int(i) => {
            let formatted = i.to_string();
            if out.len().saturating_add(formatted.len()) > remaining_budget {
                return Err(ExtractError::TooLarge);
            }
            out.push_str(&formatted);
        }
        Data::Bool(b) => {
            let formatted = b.to_string();
            if out.len().saturating_add(formatted.len()) > remaining_budget {
                return Err(ExtractError::TooLarge);
            }
            out.push_str(&formatted);
        }
        Data::DateTime(dt) => {
            let formatted = dt.to_string();
            if out.len().saturating_add(formatted.len()) > remaining_budget {
                return Err(ExtractError::TooLarge);
            }
            out.push_str(&formatted);
        }
        Data::DateTimeIso(s) | Data::DurationIso(s) => {
            if out.len().saturating_add(s.len()) > remaining_budget {
                return Err(ExtractError::TooLarge);
            }
            out.push_str(s);
        }
        Data::Error(e) => {
            let formatted = format!("{e:?}");
            if out.len().saturating_add(formatted.len()) > remaining_budget {
                return Err(ExtractError::TooLarge);
            }
            out.push_str(&formatted);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;

    fn limits(max_cells: u64) -> Limits {
        Limits {
            max_cells,
            ..Limits::default()
        }
    }

    #[test]
    fn add_range_cells_counts_the_rectangle_and_the_sum() {
        let r = |h: u32, w: u32| Range::<Data>::new((0, 0), (h - 1, w - 1));
        let mut total = 0;
        assert!(add_range_cells(&mut total, &r(10, 10), 100).is_ok());
        assert_eq!(total, 100);
        assert!(matches!(
            add_range_cells(&mut total, &r(1, 1), 100),
            Err(ExtractError::TooLarge)
        ));
        let mut total = 0;
        assert!(matches!(
            add_range_cells(&mut total, &r(11, 10), 100),
            Err(ExtractError::TooLarge)
        ));
    }

    fn ods(sheets: &[(u32, u32)]) -> Vec<u8> {
        let mut tables = String::new();
        for (n, &(rows, cols)) in sheets.iter().enumerate() {
            tables += &format!(r#"<table:table table:name="S{n}">"#);
            for _ in 0..rows {
                tables += "<table:table-row>";
                for _ in 0..cols {
                    tables += r#"<table:table-cell office:value-type="string"><text:p>x</text:p></table:table-cell>"#;
                }
                tables += "</table:table-row>";
            }
            tables += "</table:table>";
        }
        let content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet>{tables}</office:spreadsheet></office:body></office:document-content>"#
        );
        let mut zw = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        zw.start_file("mimetype", stored).unwrap();
        zw.write_all(ODF_ODS_MIME.as_bytes()).unwrap();
        zw.start_file("META-INF/manifest.xml", SimpleFileOptions::default())
            .unwrap();
        zw.write_all(br#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"/>"#)
            .unwrap();
        zw.start_file("content.xml", SimpleFileOptions::default())
            .unwrap();
        zw.write_all(content.as_bytes()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    fn run(ft: FileType, bytes: &[u8], max: u64) -> Result<Extracted, ExtractError> {
        extract("w", ft, bytes, &limits(max))
    }

    #[test]
    fn ods_cell_budget() {
        let one = ods(&[(100, 10)]);
        let r = run(FileType::Ods, &one, 900);
        assert!(matches!(r, Err(ExtractError::TooLarge)), "{r:?}");
        assert!(run(FileType::Ods, &one, 1000).is_ok());
        let two = ods(&[(30, 20), (30, 20)]);
        assert!(matches!(
            run(FileType::Ods, &two, 900),
            Err(ExtractError::TooLarge)
        ));
        assert!(run(FileType::Ods, &two, 1200).is_ok());
    }

    #[test]
    fn xls_cell_budget() {
        let big = include_bytes!("../../tests/fixtures/sheets/big.xls");
        let two = include_bytes!("../../tests/fixtures/sheets/two.xls");
        let small = include_bytes!("../../tests/fixtures/sheets/small.xls");
        assert!(matches!(
            run(FileType::Xls, big, 900),
            Err(ExtractError::TooLarge)
        ));
        assert!(matches!(
            run(FileType::Xls, two, 900),
            Err(ExtractError::TooLarge)
        ));
        assert!(run(FileType::Xls, two, 1200).is_ok());
        assert!(run(FileType::Xls, small, 900).is_ok());
        assert!(run(FileType::Xls, big, 1000).is_ok());
    }
}
