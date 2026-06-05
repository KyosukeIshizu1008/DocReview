use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::chunking;
use crate::embedding::Embedder;
use crate::llm::{Llm, LlmRequest, RagDocument};
use crate::vectordb::{SearchFilter, VectorStore};

/// レビュー結果。LLM の JSON 応答をパースした構造化結果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewResult {
    /// 元ドキュメントのメタデータ
    #[serde(default)]
    pub doc_title: String,
    #[serde(default)]
    pub doc_source_id: String,
    #[serde(default)]
    pub doc_url: String,
    /// 作成日時 (epoch sec)
    #[serde(default)]
    pub reviewed_at: i64,

    /// 構造化所見
    #[serde(default)]
    pub contradictions: Vec<ContradictionFinding>,
    #[serde(default)]
    pub duplicates: Vec<DuplicateFinding>,
    #[serde(default)]
    pub gaps: Vec<String>,
    #[serde(default)]
    pub terminology: Vec<TerminologyFinding>,

    /// 総合評価 ("A" / "B" / "C" / "要修正")
    #[serde(default)]
    pub overall_grade: String,
    /// 1-2文の総評
    #[serde(default)]
    pub summary: String,

    /// LLM の生レスポンス（JSON パース失敗時の fallback 用）
    #[serde(default)]
    pub raw_response: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContradictionFinding {
    pub new_excerpt: String,
    pub existing_title: String,
    pub existing_url: String,
    pub explanation: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DuplicateFinding {
    pub new_section: String,
    pub existing_title: String,
    pub existing_url: String,
    pub overlap_note: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminologyFinding {
    pub new_term: String,
    pub existing_term: String,
    pub suggestion: String,
}

const SYSTEM: &str = "\
あなたはドキュメント品質レビュアーです。\
新規ドキュメント [NEW] と、関連既存ナレッジ [EXISTING] を比較し、4観点で問題を抽出します:\n\
1. 矛盾 (contradictions): NEW の主張と EXISTING の主張が食い違う\n\
2. 重複 (duplicates): NEW が EXISTING の内容を再記述している\n\
3. 欠落 (gaps): NEW が触れるべきだが抜けているトピック\n\
4. 用語不整合 (terminology): NEW が EXISTING と命名・略称が揃っていない\n\
\n\
回答は必ず JSON のみ。前後にマークダウンや説明文を含めないこと。日本語で記述。\
";

const USER_TEMPLATE: &str = "\
## NEW ドキュメント\n\
タイトル: {TITLE}\n\
本文:\n\
{NEW_BODY}\n\
\n\
## EXISTING 関連ナレッジ\n\
{EXISTING_DOCS}\n\
\n\
以下の JSON スキーマで回答してください:\n\
```json\n\
{\n\
  \"contradictions\": [\n\
    {\"new_excerpt\": \"\", \"existing_title\": \"\", \"existing_url\": \"\", \"explanation\": \"\"}\n\
  ],\n\
  \"duplicates\": [\n\
    {\"new_section\": \"\", \"existing_title\": \"\", \"existing_url\": \"\", \"overlap_note\": \"\"}\n\
  ],\n\
  \"gaps\": [\"\"],\n\
  \"terminology\": [\n\
    {\"new_term\": \"\", \"existing_term\": \"\", \"suggestion\": \"\"}\n\
  ],\n\
  \"overall_grade\": \"A\",\n\
  \"summary\": \"\"\n\
}\n\
```\n\
\n\
該当なしの観点は空配列にしてください。回答は JSON 1個のみ。\
";

/// ドキュメントをレビュー。
/// `text` を embed → 既存ナレッジ top-K を集める → LLM に JSON 構造で出力させる → パース
pub async fn review_document<E, V, L>(
    title: &str,
    source_id: &str,
    url: &str,
    text: &str,
    embedder: &E,
    store: &V,
    llm: &L,
    model: &str,
) -> Result<ReviewResult>
where
    E: Embedder,
    V: VectorStore,
    L: Llm,
{
    // 1. NEW 本文をチャンク化（長文対策）
    let new_chunks = if text.chars().count() > 1200 {
        chunking::split(text)
    } else {
        vec![text.to_owned()]
    };

    // 2. 各チャンクで既存ナレッジを top-K 検索。自分自身（同一 source_id）は除外
    let mut related: Vec<RagDocument> = vec![];
    let mut seen_ids = std::collections::HashSet::<String>::new();
    for chunk_text in &new_chunks {
        let q_vec = embedder
            .embed(&[chunk_text.clone()])
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let hits = store.search(&q_vec, 5, &SearchFilter::default()).await?;
        for h in hits {
            if h.chunk.source_id == source_id {
                continue;
            }
            if seen_ids.insert(h.chunk.id.clone()) {
                related.push(RagDocument {
                    title: format!("{} ({})", h.chunk.title, h.chunk.source_id),
                    url: h.chunk.url,
                    source_type: h.chunk.source_type,
                    text: h.chunk.text,
                });
            }
        }
    }

    // 3. プロンプト組み立て
    let mut existing_block = String::new();
    if related.is_empty() {
        existing_block.push_str("(関連既存ナレッジは見つかりませんでした)\n");
    } else {
        for (i, d) in related.iter().enumerate() {
            existing_block.push_str(&format!(
                "[{}] {} ({})\nURL: {}\n{}\n\n",
                i + 1,
                d.title,
                d.source_type,
                d.url,
                d.text
            ));
        }
    }
    let user_prompt = USER_TEMPLATE
        .replace("{TITLE}", title)
        .replace("{NEW_BODY}", text)
        .replace("{EXISTING_DOCS}", &existing_block);

    // 4. LLM 呼び出し（履歴なし、ドキュメントは user message 内に埋め込み済み）
    let resp = llm
        .complete(LlmRequest {
            system: SYSTEM.to_owned(),
            user: user_prompt,
            history: vec![],
            documents: vec![],
            model: model.to_owned(),
        })
        .await?;

    // 5. JSON 抽出 + パース
    let parsed = parse_review_json(&resp.content).unwrap_or_else(|| ReviewResult {
        raw_response: resp.content.clone(),
        summary: "(JSON 解析失敗 — raw_response を参照)".to_owned(),
        ..Default::default()
    });

    Ok(ReviewResult {
        doc_title: title.to_owned(),
        doc_source_id: source_id.to_owned(),
        doc_url: url.to_owned(),
        reviewed_at: chrono::Utc::now().timestamp(),
        raw_response: if parsed.contradictions.is_empty()
            && parsed.duplicates.is_empty()
            && parsed.gaps.is_empty()
            && parsed.terminology.is_empty()
        {
            resp.content
        } else {
            String::new()
        },
        ..parsed
    })
}

/// LLM 応答から JSON 部分を切り出してパース。
/// マークダウンの ```json ブロックも考慮。
pub fn parse_review_json(s: &str) -> Option<ReviewResult> {
    let candidate = extract_json_block(s)?;
    serde_json::from_str::<ReviewResult>(&candidate).ok()
}

fn extract_json_block(s: &str) -> Option<String> {
    // ```json ... ``` で囲まれていれば抽出
    if let Some(start) = s.find("```json") {
        let after = &s[start + 7..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim().to_owned());
        }
    }
    if let Some(start) = s.find("```") {
        let after = &s[start + 3..];
        if let Some(end) = after.find("```") {
            let inner = after[..end].trim();
            if inner.starts_with('{') {
                return Some(inner.to_owned());
            }
        }
    }
    // フォールバック: 最初の { から最後の } までを切り出して試す
    let first = s.find('{')?;
    let last = s.rfind('}')?;
    if last > first {
        Some(s[first..=last].to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pure_json() {
        let s = r#"{
            "contradictions": [
                {"new_excerpt": "auth=OAuth2", "existing_title": "Spec", "existing_url": "https://x/y", "explanation": "Spec says SAML"}
            ],
            "duplicates": [],
            "gaps": ["error handling"],
            "terminology": [],
            "overall_grade": "B",
            "summary": "minor issues"
        }"#;
        let r = parse_review_json(s).expect("parse");
        assert_eq!(r.contradictions.len(), 1);
        assert_eq!(r.contradictions[0].existing_title, "Spec");
        assert_eq!(r.gaps, vec!["error handling"]);
        assert_eq!(r.overall_grade, "B");
    }

    #[test]
    fn parse_json_in_markdown_block() {
        let s = "Here is the review:\n\n```json\n{\n  \"gaps\": [\"missing error handling\"],\n  \"overall_grade\": \"C\"\n}\n```\n\nDone.";
        let r = parse_review_json(s).expect("parse");
        assert_eq!(r.gaps, vec!["missing error handling"]);
        assert_eq!(r.overall_grade, "C");
    }

    #[test]
    fn parse_json_fallback_first_to_last_brace() {
        // ヘッダ文があり、json が markdown フェンスなしで本文に直接ある
        let s = "Sure, here is the JSON: {\"summary\": \"all good\", \"overall_grade\": \"A\"} let me know.";
        let r = parse_review_json(s).expect("parse");
        assert_eq!(r.summary, "all good");
    }

    #[test]
    fn parse_returns_none_for_non_json() {
        assert!(parse_review_json("this is just text").is_none());
        assert!(parse_review_json("{ broken json").is_none());
    }
}
