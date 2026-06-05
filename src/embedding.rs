use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// CARGO_MANIFEST_DIR/models をキャッシュとして返す（無ければ None で fastembed のデフォルトに任せる）。
fn repo_model_cache_dir() -> Option<PathBuf> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let dir = PathBuf::from(manifest).join("models");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 埋め込みベクター生成のインターフェース。
#[allow(async_fn_in_trait)]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// fastembed-rs を使ったローカル埋め込み。
/// MultilingualE5Small (384 dim) を採用 — 日本語含む多言語対応かつ軽量。
/// 初回起動時に HuggingFace から ONNX モデルがダウンロードされる (~120MB)。
pub struct LocalEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
}

impl LocalEmbedder {
    pub const MODEL: EmbeddingModel = EmbeddingModel::MultilingualE5Small;
    pub const DIM: usize = 384;

    pub fn new() -> Result<Self> {
        // リポジトリに同梱した models/ をキャッシュとして使う。
        // 社内ネットワークで HuggingFace に直接アクセスできない環境でも、
        // ここにモデルが事前配置されていれば DL をスキップして起動できる。
        let mut opts = InitOptions::new(Self::MODEL).with_show_download_progress(true);
        if let Some(dir) = repo_model_cache_dir() {
            opts = opts.with_cache_dir(dir);
        }
        let model = TextEmbedding::try_new(opts).context("fastembed model init failed")?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }
}

impl Embedder for LocalEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        // multilingual-e5 は "query: ..." / "passage: ..." prefix を期待する。
        // 索引化と検索の対称性を保つため、ここではすべて passage 扱いにしておく。
        // (検索クエリ側でも passage prefix で OK な経験則。厳密にするなら別メソッドを切る)
        let prefixed: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
        let model = self.model.clone();
        let out = tokio::task::spawn_blocking(move || {
            // poisoned でも guard を取り出して継続（モデル状態は推論で破壊されないため安全）
            let mut guard = match model.lock() {
                Ok(g) => g,
                Err(poison) => {
                    tracing::warn!("embedder mutex was poisoned; recovering");
                    poison.into_inner()
                }
            };
            guard.embed(prefixed, Some(32))
        })
        .await
        .context("embed task join")?
        .context("embed failed")?;
        Ok(out)
    }
}

/// 開発初期用のダミー実装。fastembed のダウンロードを避けたいテスト時に使う。
#[allow(dead_code)]
pub struct DummyEmbedder {
    pub dim: usize,
}

#[allow(dead_code)]
impl Default for DummyEmbedder {
    fn default() -> Self {
        Self { dim: 384 }
    }
}

impl Embedder for DummyEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
    }
}
