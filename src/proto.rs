use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Share {
    pub fstype: String,
    pub source: String,
    pub mount_root: String,
    pub mount_point: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct MountIdentity {
    pub share: Share,
    pub mount_id: u64,
    pub submounts: Vec<String>,
}

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

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
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
        identity_before: Option<MountIdentity>,
        identity_after: Option<MountIdentity>,
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
        /// statx of the file at event time; `None` when gone or not a regular file.
        size: Option<u64>,
        mtime_ns: Option<i64>,
    },
    WatchDegraded {
        reason: String,
    },
    /// Initial watches are in place; ends the coordinator's setup progress timeout.
    WatchReady,
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
        assert_eq!(
            serde_json::to_string(&Message::ScanDone {
                complete: true,
                identity_before: Some(MountIdentity {
                    share: Share {
                        fstype: "nfs".into(),
                        source: "host:/share".into(),
                        mount_root: "/".into(),
                        mount_point: "/mnt/share".into(),
                    },
                    mount_id: 42,
                    submounts: vec!["/mnt/share/sub".into()],
                }),
                identity_after: None,
            })
            .unwrap(),
            r#"{"message":"ScanDone","complete":true,"identity_before":{"share":{"fstype":"nfs","source":"host:/share","mount_root":"/","mount_point":"/mnt/share"},"mount_id":42,"submounts":["/mnt/share/sub"]},"identity_after":null}"#
        );
    }
}
