//! Static-embedding model (model2vec): tokenizer + one embedding row per token.
//! `Query` mode preads only the rows a query needs; `Bulk` mode memory-maps the matrix.

use std::error::Error;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use memmap2::Mmap;
use tokenizers::Tokenizer;

pub const DIM: usize = 512;
const ROW_BYTES: usize = DIM * 4;
const MAX_TOKENS: usize = 512;
const MODEL_REV: &str = "6fc8051fab2a1e0ee76689cf08c853792ac285e7";

type Res<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Mode {
    Query,
    Bulk,
}

enum Rows {
    File(File),
    Map(Mmap),
}

pub struct Embedder {
    tok: Tokenizer,
    rows: Rows,
    data_start: u64,
    vocab: usize,
    unk_id: Option<u32>,
}

/// The one embedding-input format: path, symbol or heading, then the chunk text.
pub fn input(relpath: &str, symbol_or_heading: Option<&str>, text: &str) -> String {
    format!("{relpath}\n{}\n{text}", symbol_or_heading.unwrap_or(""))
}

/// Embed-cache key: BLAKE3(model_rev ‖ full input).
pub fn cache_key(input: &str) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    h.update(MODEL_REV.as_bytes());
    h.update(input.as_bytes());
    h.finalize()
}

/// Validate the safetensors header; returns (absolute data start, vocab). Every size and
/// offset is checked arithmetic and must lie inside the file.
fn parse_header(json: &serde_json::Value, n: u64, file_len: u64) -> Res<(u64, u64)> {
    let bad = || -> Box<dyn Error> { "embeddings must be F32 [vocab, 512] inside the file".into() };
    let t = json.get("embeddings").ok_or("no embeddings tensor")?;
    let shape = t["shape"].as_array().ok_or_else(bad)?;
    if shape.len() != 2 || t["dtype"] != "F32" {
        return Err(bad());
    }
    let vocab = shape[0].as_u64().ok_or_else(bad)?;
    if shape[1].as_u64() != Some(DIM as u64) {
        return Err(bad());
    }
    let offsets = t["data_offsets"].as_array().ok_or_else(bad)?;
    if offsets.len() != 2 {
        return Err(bad());
    }
    let begin = offsets[0].as_u64().ok_or_else(bad)?;
    let end = offsets[1].as_u64().ok_or_else(bad)?;
    let size = end.checked_sub(begin).ok_or_else(bad)?;
    if size != vocab.checked_mul(ROW_BYTES as u64).ok_or_else(bad)? {
        return Err(bad());
    }
    let start = 8u64.checked_add(n).ok_or_else(bad)?;
    if start.checked_add(end).ok_or_else(bad)? != file_len {
        return Err(bad());
    }
    Ok((start + begin, vocab)) // begin <= end, so this is <= file_len
}

impl Embedder {
    pub fn open(model_dir: &Path, mode: Mode) -> Res<Self> {
        let tok_bytes = std::fs::read(model_dir.join("tokenizer.json"))?;
        let tok = Tokenizer::from_bytes(&tok_bytes).map_err(|e| e.to_string())?;
        let unk = serde_json::from_slice::<serde_json::Value>(&tok_bytes)?["model"]["unk_token"]
            .as_str()
            .unwrap_or("[UNK]")
            .to_string();
        let unk_id = tok.token_to_id(&unk);
        let file = File::open(model_dir.join("model.safetensors"))?;
        let mut len = [0u8; 8];
        file.read_exact_at(&mut len, 0)?;
        let n = u64::from_le_bytes(len);
        if n > 1 << 20 {
            return Err("safetensors header too large".into());
        }
        let mut header = vec![0u8; n as usize];
        file.read_exact_at(&mut header, 8)?;
        let json: serde_json::Value = serde_json::from_slice(&header)?;
        let (data_start, vocab) = parse_header(&json, n, file.metadata()?.len())?;
        let rows = match mode {
            Mode::Query => Rows::File(file),
            Mode::Bulk => {
                // SAFETY: the model file is a read-only store path.
                let map = unsafe { Mmap::map(&file)? };
                safetensors::SafeTensors::deserialize(&map)?; // full validation
                Rows::Map(map)
            }
        };
        Ok(Self {
            tok,
            rows,
            data_start,
            vocab: vocab as usize,
            unk_id,
        })
    }

    fn add_row(&self, id: usize, acc: &mut [f32; DIM]) -> Res<()> {
        // id < vocab and vocab * ROW_BYTES was checked against the file length at open.
        let at = self.data_start + id as u64 * ROW_BYTES as u64;
        let mut buf = [0u8; ROW_BYTES];
        let bytes: &[u8] = match &self.rows {
            Rows::File(f) => {
                f.read_exact_at(&mut buf, at)?;
                &buf
            }
            Rows::Map(m) => &m[at as usize..at as usize + ROW_BYTES],
        };
        for (a, c) in acc.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *a += f32::from_le_bytes(*c);
        }
        Ok(())
    }

    /// Mean of the token rows (no `[UNK]`, at most 512 tokens), L2-normalised, ×127,
    /// rounded, clamped to i8. `None` when no usable token remains: never a zero vector.
    pub fn embed(&self, text: &str) -> Res<Option<[i8; DIM]>> {
        let enc = self.tok.encode(text, false).map_err(|e| e.to_string())?;
        let mut acc = [0f32; DIM];
        let mut count = 0usize;
        let usable = enc
            .get_ids()
            .iter()
            .filter(|&&id| Some(id) != self.unk_id && (id as usize) < self.vocab)
            .take(MAX_TOKENS);
        for &id in usable {
            self.add_row(id as usize, &mut acc)?;
            count += 1;
        }
        let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
        if count == 0 || norm == 0.0 || !norm.is_finite() {
            return Ok(None);
        }
        let mut out = [0i8; DIM];
        for (o, a) in out.iter_mut().zip(acc) {
            *o = (a / norm * 127.0).round().clamp(-127.0, 127.0) as i8;
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rss_kb() -> u64 {
        let s = std::fs::read_to_string("/proc/self/status").unwrap();
        let l = s.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
        l.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    #[test]
    fn cache_key_changes_with_path_prefix() {
        let a = cache_key(&input("a/x.md", Some("H"), "text"));
        let b = cache_key(&input("b/x.md", Some("H"), "text"));
        assert_ne!(a, b);
        assert_eq!(input("p", None, "t"), "p\n\nt");
    }

    #[test]
    fn malformed_headers_error_without_panic() {
        let ok =
            json!({"embeddings": {"dtype": "F32", "shape": [2, 512], "data_offsets": [0, 4096]}});
        assert_eq!(parse_header(&ok, 100, 8 + 100 + 4096).unwrap(), (108, 2));
        let cases = [
            json!({"embeddings": {"dtype": "F32", "shape": [], "data_offsets": [0, 0]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [2], "data_offsets": [0, 4096]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [2, 512, 1], "data_offsets": [0, 4096]}}),
            json!({"embeddings": {"dtype": "F16", "shape": [2, 512], "data_offsets": [0, 4096]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [2, 256], "data_offsets": [0, 4096]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [u64::MAX, 512], "data_offsets": [0, 4096]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [2, 512], "data_offsets": [4096, 0]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [2, 512], "data_offsets": [0]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [0, 512], "data_offsets": [u64::MAX, u64::MAX]}}),
            json!({"embeddings": {"dtype": "F32", "shape": [2, 512], "data_offsets": [0, 8192]}}),
            json!({"embeddings": "x"}),
            json!({}),
        ];
        for c in &cases {
            assert!(parse_header(c, 100, 8 + 100 + 4096).is_err(), "{c}");
        }
        assert!(parse_header(&ok, u64::MAX, u64::MAX).is_err()); // 8 + n overflows
    }

    #[test]
    fn zero_usable_tokens_give_no_vector() {
        let dir = crate::model_dir();
        if dir == "unset" {
            println!("skipped: LLM_INDEX_MODEL_DIR unset");
            return;
        }
        let e = Embedder::open(Path::new(dir), Mode::Query).unwrap();
        assert!(e.embed("").unwrap().is_none());
        assert!(e.embed("hello world").unwrap().is_some());
    }

    #[test]
    fn query_mode_does_not_map_and_rss_stays_flat() {
        let dir = crate::model_dir();
        if dir == "unset" {
            println!("skipped: LLM_INDEX_MODEL_DIR unset");
            return;
        }
        let before = rss_kb(); // before the tokenizer and header are loaded
        let e = Embedder::open(Path::new(dir), Mode::Query).unwrap();
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        assert!(!maps.contains("model.safetensors"), "matrix is mapped");
        // 1,000 queries of token ids drawn from across the whole vocabulary
        let mut x = 0x9E3779B97F4A7C15u64;
        for _ in 0..1000 {
            let ids: Vec<u32> = (0..12)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x % e.vocab as u64) as u32
                })
                .collect();
            let text = e.tok.decode(&ids, true).unwrap();
            e.embed(&text).unwrap();
        }
        let grown = rss_kb().saturating_sub(before);
        let matrix_kb = (e.vocab * ROW_BYTES / 1024) as u64;
        assert!(
            grown < matrix_kb / 2,
            "RSS grew {grown} kB, matrix is {matrix_kb} kB"
        );
    }
}
