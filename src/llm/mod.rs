use anyhow::Result;

use crate::chat::Citation;
use crate::config::{LlmConfig, LlmProviderKind};

pub mod claude;
pub mod gemini;

pub use claude::ClaudeClient;
pub use gemini::GeminiClient;

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub system: String,
    pub user: String,
    /// 過去のターン（古い順）。現在の user は含めない。
    pub history: Vec<HistoryMessage>,
    /// RAG コンテキスト（チャンクのテキスト + メタ）
    pub documents: Vec<RagDocument>,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub role: HistoryRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy)]
pub enum HistoryRole {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct RagDocument {
    pub title: String,
    pub url: String,
    pub source_type: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    pub citations: Vec<Citation>,
}

#[allow(async_fn_in_trait)]
pub trait Llm: Send + Sync {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse>;
}

/// 設定に応じて Claude / Gemini をディスパッチする統合型
pub enum LlmActive {
    Claude(ClaudeClient),
    Gemini(GeminiClient),
}

impl LlmActive {
    pub fn from_config(cfg: &LlmConfig) -> Self {
        match cfg.kind {
            LlmProviderKind::Claude => Self::Claude(ClaudeClient::new(cfg.api_key.clone())),
            LlmProviderKind::Gemini => Self::Gemini(GeminiClient::new(cfg.api_key.clone())),
        }
    }
}

impl Llm for LlmActive {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        match self {
            LlmActive::Claude(c) => c.complete(req).await,
            LlmActive::Gemini(g) => g.complete(req).await,
        }
    }
}
