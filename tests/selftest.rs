use std::fs;
use std::process::Command;

#[test]
fn index_self_test_passes_on_this_host() {
    let base = std::env::temp_dir().join(format!("llm-index-selftest-it-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let config = base.join("config.toml");
    fs::write(
        &config,
        format!("state_dir = \"{}\"\n", base.join("state").display()),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_llm-index"))
        .arg("--config")
        .arg(&config)
        .args(["index", "--self-test"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    fs::remove_dir_all(&base).unwrap();
    assert!(out.status.success(), "{err}");
}
