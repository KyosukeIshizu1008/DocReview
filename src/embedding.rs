use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 埋め込みベクターの次元。Gemini `text-embedding-004` のネイティブ次元に合わせる。
/// `outputDimensionality` で常にこの次元に固定するため、別モデルに替えても DB スキーマは不変。
pub const EMBED_DIM: usize = 768;

const GEMINI_API: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const DEFAULT_MODEL: &str = "text-embedding-004";
/// レート制限(429)/一時障害(5xx)/通信エラー時のリトライ上限。
const MAX_EMBED_ATTEMPTS: u32 = 5;

/// 指数バックオフ: 0.5s, 1s, 2s, 4s, 8s（上限 8s）。
fn backoff(attempt: u32) -> Duration {
    let ms = 500u64.saturating_mul(1u64 << (attempt - 1).min(5));
    Duration::from_millis(ms.min(8000))
}

/// 429 の Retry-After ヘッダ（秒）を尊重する。
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// 埋め込みベクター生成のインターフェース。
#[allow(async_fn_in_trait)]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Gemini の埋め込み API を使うリモート埋め込み。
/// ローカル ONNX ランタイム/モデルのダウンロードが不要で、社内ネットワークで
/// HuggingFace に到達できない環境でも動く（その代わり実行時に Google への通信が必要）。
pub struct GeminiEmbedder {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl GeminiEmbedder {
    pub fn new(api_key: String, model: String) -> Self {
        let model = if model.trim().is_empty() {
            DEFAULT_MODEL.to_owned()
        } else {
            model
        };
        Self {
            api_key,
            model,
            http: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct BatchEmbedRequest {
    requests: Vec<EmbedReq>,
}

#[derive(Serialize)]
struct EmbedReq {
    model: String,
    content: EmbedContent,
    #[serde(rename = "outputDimensionality")]
    output_dimensionality: usize,
}

#[derive(Serialize)]
struct EmbedContent {
    parts: Vec<EmbedPart>,
}

#[derive(Serialize)]
struct EmbedPart {
    text: String,
}

#[derive(Deserialize)]
struct BatchEmbedResponse {
    embeddings: Vec<EmbeddingValues>,
}

#[derive(Deserialize)]
struct EmbeddingValues {
    values: Vec<f32>,
}

/// L2 正規化（単位ベクトル化）。`outputDimensionality` でネイティブ未満に切り詰めた場合、
/// Gemini の埋め込みは正規化されていないため、コサイン類似度の整合性のためここで正規化する。
fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

impl Embedder for GeminiEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        if self.api_key.is_empty() {
            anyhow::bail!(
                "Gemini 埋め込み用 API キーが未設定です（設定タブの「埋め込み (Gemini)」で入力してください）"
            );
        }
        let model_path = format!("models/{}", self.model);
        let requests = texts
            .iter()
            .map(|t| EmbedReq {
                model: model_path.clone(),
                content: EmbedContent {
                    parts: vec![EmbedPart { text: t.clone() }],
                },
                output_dimensionality: EMBED_DIM,
            })
            .collect();
        let body = BatchEmbedRequest { requests };

        let url = format!("{GEMINI_API}/{}:batchEmbedContents", self.model);
        // 429 / 5xx / 通信エラーは指数バックオフでリトライ（大量チャンク同期が1回の失敗で落ちないように）
        let parsed: BatchEmbedResponse = {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match self
                    .http
                    .post(&url)
                    .header("x-goog-api-key", &self.api_key)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            break resp.json().await.context("gemini embed decode")?;
                        }
                        let retryable = status.as_u16() == 429 || status.is_server_error();
                        if retryable && attempt < MAX_EMBED_ATTEMPTS {
                            let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                            let txt = resp.text().await.unwrap_or_default();
                            tracing::warn!(
                                "Gemini embed {status} (attempt {attempt}/{MAX_EMBED_ATTEMPTS}); \
                                 retrying in {wait:?}: {txt}"
                            );
                            tokio::time::sleep(wait).await;
                            continue;
                        }
                        let txt = resp.text().await.unwrap_or_default();
                        anyhow::bail!("Gemini embed API {status}: {txt}");
                    }
                    Err(e) => {
                        if attempt < MAX_EMBED_ATTEMPTS {
                            let wait = backoff(attempt);
                            tracing::warn!(
                                "Gemini embed request error (attempt {attempt}/{MAX_EMBED_ATTEMPTS}); \
                                 retrying in {wait:?}: {e}"
                            );
                            tokio::time::sleep(wait).await;
                            continue;
                        }
                        return Err(anyhow::Error::new(e).context("gemini embed request"));
                    }
                }
            }
        };
        if parsed.embeddings.len() != texts.len() {
            anyhow::bail!(
                "gemini embed: expected {} embeddings, got {}",
                texts.len(),
                parsed.embeddings.len()
            );
        }
        Ok(parsed
            .embeddings
            .into_iter()
            .map(|e| normalize(e.values))
            .collect())
    }
}
