//! llm-index library root.

pub mod auth;
pub mod config;
pub mod db;
pub mod deny;
pub mod embed;
pub mod rank;

/// Store paths baked in at build time by `nix/package.nix`; "unset" outside Nix.
pub fn pdfium_lib() -> &'static str {
    option_env!("LLM_INDEX_PDFIUM_LIB").unwrap_or("unset")
}

pub fn model_dir() -> &'static str {
    option_env!("LLM_INDEX_MODEL_DIR").unwrap_or("unset")
}
