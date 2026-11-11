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
/// For `.ods` and `.xls`, `calamine` does not expose dimensions up front before
/// materialising a worksheet range. Input file size is capped at 20 MiB and
/// `limits.max_text_bytes` is enforced while iterating rows; any dense-range
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
            for sheet_name in wb.sheet_names() {
                let range = wb.worksheet_range(&sheet_name).map_err(|e| {
                    ExtractError::Parse(format!("ods sheet '{sheet_name}' error: {e}"))
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
        FileType::Xls => {
            if !bytes.starts_with(OLE2_MAGIC) {
                return Err(ExtractError::Skipped(Skip::Mismatch));
            }
            if bytes.len() > MAX_LEGACY_SHEET_BYTES {
                return Err(ExtractError::TooLarge);
            }
            let mut wb: Xls<_> = Xls::new(Cursor::new(bytes))
                .map_err(|e| ExtractError::Parse(format!("xls error: {e}")))?;
            for sheet_name in wb.sheet_names() {
                let range = wb.worksheet_range(&sheet_name).map_err(|e| {
                    ExtractError::Parse(format!("xls sheet '{sheet_name}' error: {e}"))
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
        _ => return Err(ExtractError::Skipped(Skip::Unsupported)),
    }

    Ok(Extracted {
        title,
        file_type,
        segments,
    })
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
        chunk_rows += 1;

        if chunk_rows as usize == ROWS_PER_SEGMENT {
            let start_line = *global_line + 1;
            *global_line += chunk_rows;
            segments.push(Segment {
                kind: SegmentKind::Sheet {
                    name: sheet_name.to_string(),
                    first_row: chunk_first_row,
                    last_row: chunk_last_row,
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
