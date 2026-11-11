use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum Job {
    Scan,
    Extract {
        relpath: String,
        expected_size: u64,
        expected_mtime_ns: i64,
    },
    Validate {
        paths: Vec<String>,
    },
    Watch,
    Shutdown,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Chunk {
    pub ord: usize,
    pub kind: String,
    pub start: usize,
    pub end: usize,
    pub decl_start: Option<usize>,
    pub decl_end: Option<usize>,
    pub heading_or_symbol: Option<String>,
    pub text: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "message")]
pub enum Message {
    RootError {
        state: String,
        reason: String,
    },
    Progress {
        entries: Option<u64>,
        bytes: Option<u64>,
    },
    Entry {
        relpath: String,
        size: u64,
        mtime_ns: i64,
        is_dir: bool,
    },
    Skipped {
        relpath: String,
        reason: String,
    },
    ScanDone {
        complete: bool,
        identity_before: Option<String>,
        identity_after: Option<String>,
    },
    Extracted {
        relpath: String,
        title: String,
        r#type: String,
        blake3: String,
        chunks: Vec<Chunk>,
    },
    Failed {
        relpath: String,
        reason: String,
    },
    Validated {
        path: String,
        result: ValidationResult,
    },
    WatchEvent {
        relpath: String,
        kind: String,
    },
    WatchDegraded {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationResult {
    Ok,
    Eloop,
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_line_is_an_object() {
        assert_eq!(
            serde_json::to_string(&Job::Scan).unwrap(),
            r#"{"type":"Scan"}"#
        );
        assert!(matches!(
            serde_json::from_str::<Job>(r#"{"type":"Shutdown"}"#).unwrap(),
            Job::Shutdown
        ));
    }
}
