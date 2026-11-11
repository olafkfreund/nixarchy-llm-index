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

impl Embedder {
    pub fn open(model_dir: &Path, mode: Mode) -> Res<Self> {
        let tok =
            Tokenizer::from_file(model_dir.join("tokenizer.json")).map_err(|e| e.to_string())?;
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
        let t = json.get("embeddings").ok_or("no embeddings tensor")?;
        let shape = t["shape"].as_array().ok_or("bad shape")?;
        let (vocab, dim) = (
            shape[0].as_u64().unwrap_or(0),
            shape[1].as_u64().unwrap_or(0),
        );
        let (begin, end) = (t["data_offsets"][0].as_u64(), t["data_offsets"][1].as_u64());
        if t["dtype"] != "F32" || shape.len() != 2 || dim as usize != DIM {
            return Err("embeddings must be F32 [vocab, 512]".into());
        }
        let (begin, end) = (begin.ok_or("bad offsets")?, end.ok_or("bad offsets")?);
        if end - begin != vocab * ROW_BYTES as u64 || file.metadata()?.len() != 8 + n + end {
            return Err("embeddings size does not match the file".into());
        }
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
            data_start: 8 + n + begin,
            vocab: vocab as usize,
        })
    }

    fn add_row(&self, id: usize, acc: &mut [f32; DIM]) -> Res<()> {
        let at = self.data_start + (id * ROW_BYTES) as u64;
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

    /// Mean of the token rows, L2-normalised, ×127, rounded, clamped to i8.
    pub fn embed(&self, text: &str) -> Res<[i8; DIM]> {
        let enc = self.tok.encode(text, false).map_err(|e| e.to_string())?;
        let mut acc = [0f32; DIM];
        let mut count = 0usize;
        for &id in enc.get_ids() {
            if (id as usize) < self.vocab {
                self.add_row(id as usize, &mut acc)?;
                count += 1;
            }
        }
        let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mut out = [0i8; DIM];
        if count > 0 && norm > 0.0 {
            for (o, a) in out.iter_mut().zip(acc) {
                *o = (a / norm * 127.0).round().clamp(-127.0, 127.0) as i8;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn query_mode_rss_stays_flat() {
        let dir = crate::model_dir();
        if dir == "unset" {
            println!("skipped: LLM_INDEX_MODEL_DIR unset");
            return;
        }
        let e = Embedder::open(Path::new(dir), Mode::Query).unwrap();
        let v = e.embed("warm up the tokenizer").unwrap();
        assert!(v.iter().any(|&x| x != 0));
        let before = rss_kb();
        for i in 0..1000 {
            e.embed(&format!(
                "distinct query {i} about topic {} and {}",
                i * 7919,
                i * 31
            ))
            .unwrap();
        }
        let grown = rss_kb().saturating_sub(before);
        assert!(grown < 10 * 1024, "RSS grew {grown} kB");
    }
}
