use anyhow::Result;

use crate::embedding::Embedder;
use crate::llm::{HistoryMessage, HistoryRole, Llm, LlmRequest, RagDocument};
use crate::vectordb::{SearchFilter, VectorStore};

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub citations: Vec<Citation>,
}

#[derive(Debug, Clone, Copy)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct Citation {
    pub title: String,
    pub url: String,
    pub source_type: String,
}

pub const SYSTEM_PROMPT: &str = "\
あなたは社内ナレッジ検索アシスタントです。\
ユーザーの質問に対し、提供された Jira / Confluence の抜粋に基づいて日本語で回答してください。\
方針:\n\
- 抜粋に該当する根拠がある場合: その内容を優先し、引用元を明示する\n\
- 抜粋に該当情報がない、または抜粋が空の場合: 一般的な範囲で答えつつ、\"ナレッジ内に該当情報は見つかりませんでした\" と一言添える\n\
- 推測で具体的な値や手順を断定しない。曖昧な場合は曖昧と伝える\n\
- ドキュメント間で食い違いを見つけた場合は ⚠️ を付けて明示する\n\
- 過去の会話履歴も踏まえて自然に応答する\
";

/// 履歴として LLM に渡す最大ターン数（ユーザー＋アシスタントで1ターン換算ではなくメッセージ数）
const HISTORY_LIMIT: usize = 8;

/// RAG 1 ターン: embed query → vector search → LLM 呼び出し
/// `history` は古い順、現在の質問は含めない。
pub async fn ask_rag<E, V, L>(
    embedder: &E,
    store: &V,
    llm: &L,
    model: &str,
    top_k: usize,
    question: &str,
    history: &[ChatMessage],
) -> Result<(String, Vec<Citation>)>
where
    E: Embedder,
    V: VectorStore,
    L: Llm,
{
    let q_vecs = embedder.embed(&[question.to_owned()]).await?;
    let q_vec = q_vecs.into_iter().next().unwrap_or_default();
    // 空ストアでもチャットが続けられるよう、検索失敗を致命傷にしない
    let hits = match store.search(&q_vec, top_k, &SearchFilter::default()).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("vector search failed (continuing without docs): {e:?}");
            vec![]
        }
    };

    let documents: Vec<RagDocument> = hits
        .iter()
        .map(|h| RagDocument {
            title: format!("{} ({})", h.chunk.title, h.chunk.source_id),
            url: h.chunk.url.clone(),
            source_type: h.chunk.source_type.clone(),
            text: h.chunk.text.clone(),
        })
        .collect();

    // 履歴を末尾から HISTORY_LIMIT 件に切り詰めて、古い順に並べる
    let trimmed: Vec<HistoryMessage> = history
        .iter()
        .rev()
        .take(HISTORY_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| HistoryMessage {
            role: match m.role {
                Role::User => HistoryRole::User,
                Role::Assistant => HistoryRole::Assistant,
            },
            content: m.content.clone(),
        })
        .collect();

    let resp = llm
        .complete(LlmRequest {
            system: SYSTEM_PROMPT.to_owned(),
            user: question.to_owned(),
            history: trimmed,
            documents,
            model: model.to_owned(),
        })
        .await?;
    Ok((resp.content, resp.citations))
}
