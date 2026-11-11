{
  lib,
  rustPlatform,
  fetchurl,
  linkFarm,
  pdfium,
  pkg-config,
  oniguruma,
  sqlite,
}:
let
  base = "https://huggingface.co/minishlab/potion-retrieval-32M/resolve/6fc8051fab2a1e0ee76689cf08c853792ac285e7";
  model = {
    "model.safetensors" = fetchurl {
      url = "${base}/model.safetensors";
      hash = "sha256-B2CeW9M6rTeQCz/WL07Jb22uyIyk1Gudi5KL+rq/bqA=";
    };
    "tokenizer.json" = fetchurl {
      url = "${base}/tokenizer.json";
      hash = "sha256-fXXLxUMYE4gHxAGw8MlyERfGKLOd6Ojg7bbLF+DufRg=";
    };
    "config.json" = fetchurl {
      url = "${base}/config.json";
      hash = "sha256-Y8ANkIJMgywE7B0CtqmD+5BIm/BJ8p+/8VukgbikMu4=";
    };
  };
  # A store path that exists at build, test and run time (unlike $out during checkPhase).
  modelDir = linkFarm "potion-retrieval-32M" model;
in
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "llm-index";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # tokenizers' onig feature: link nixpkgs' oniguruma (BSD-2) instead of building a vendored copy.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ oniguruma sqlite ];
  RUSTONIG_SYSTEM_LIBONIG = true;

  # Read with option_env! so both paths land in the binary and hence the closure.
  LLM_INDEX_PDFIUM_LIB = "${pdfium}/lib/libpdfium.so";
  LLM_INDEX_MODEL_DIR = "${modelDir}";

  postInstall = ''
    mkdir -p $out/share/llm-index/models
    ln -s ${modelDir} $out/share/llm-index/models/potion-retrieval-32M
  '';

  passthru = { inherit modelDir pdfium; };

  meta = {
    license = lib.licenses.mit;
    mainProgram = "llm-index";
    platforms = [ "x86_64-linux" ];
  };
})
