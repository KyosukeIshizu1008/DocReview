// fastembed の埋め込みモデルをリポジトリ内 models/ に事前ダウンロードする。
// 社内ネットワーク等で HuggingFace に直接アクセスできない環境向けに、
// アクセス可能な環境で `cargo run --example download_model` してから git LFS でコミットする。

use std::path::PathBuf;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

fn main() -> anyhow::Result<()> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let dir = PathBuf::from(manifest).join("models");
    std::fs::create_dir_all(&dir)?;

    println!("Downloading MultilingualE5Small into {}", dir.display());
    let _model = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::MultilingualE5Small)
            .with_show_download_progress(true)
            .with_cache_dir(dir),
    )?;
    println!("Done. Commit ./models via git LFS.");
    Ok(())
}
