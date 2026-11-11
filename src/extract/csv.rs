use super::{ExtractError, Extracted, FileType, Segment, SegmentKind, file_name};

const ROWS_PER_SEGMENT: usize = 50;

pub fn extract(relpath: &str, bytes: &[u8]) -> Result<Extracted, ExtractError> {
    let title = file_name(relpath);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);

    let mut rows: Vec<(u32, String)> = Vec::new();
    for (idx, record) in rdr.records().enumerate() {
        let rec = record.map_err(|e| ExtractError::Parse(format!("csv error: {e}")))?;
        let line = rec
            .iter()
            .map(|field| field.replace(['\r', '\n', '\t'], " "))
            .collect::<Vec<_>>()
            .join("\t");
        if !line.trim().is_empty() {
            rows.push(((idx + 1) as u32, line));
        }
    }

    let mut segments = Vec::new();
    let mut global_line: u32 = 0;
    for chunk in rows.chunks(ROWS_PER_SEGMENT) {
        let first_row = chunk.first().map(|(r, _)| *r).unwrap_or(1);
        let last_row = chunk.last().map(|(r, _)| *r).unwrap_or(first_row);
        let start_line = global_line + 1;
        global_line += chunk.len() as u32;
        let end_line = global_line;
        let text = chunk
            .iter()
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        segments.push(Segment {
            kind: SegmentKind::Sheet {
                name: title.clone(),
                first_row,
                last_row,
            },
            start_line,
            end_line,
            text,
        });
    }

    Ok(Extracted {
        title,
        file_type: FileType::Csv,
        segments,
    })
}
