use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{HistoryRole, Llm, LlmRequest, LlmResponse};
use crate::chat::Citation;

const GEMINI_API: &str = "https://generativelanguage.googleapis.com/v1beta/models";

pub struct GeminiClient {
    api_key: String,
    http: reqwest::Client,
}

impl GeminiClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            http: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    #[serde(rename = "systemInstruction")]
    system_instruction: SystemInstruction<'a>,
    contents: Vec<Content<'a>>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Serialize)]
struct SystemInstruction<'a> {
    parts: Vec<TextPart<'a>>,
}

#[derive(Serialize)]
struct Content<'a> {
    role: &'a str,
    parts: Vec<TextPart<'a>>,
}

#[derive(Serialize)]
struct TextPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    temperature: f32,
}

#[derive(Deserialize)]
struct GenerateResponse {
    candidates: Option<Vec<Candidate>>,
}

#[derive(Deserialize)]
struct Candidate {
    content: Option<CandidateContent>,
}

#[derive(Deserialize)]
struct CandidateContent {
    parts: Option<Vec<RespPart>>,
}

#[derive(Deserialize)]
struct RespPart {
    text: Option<String>,
}

impl Llm for GeminiClient {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        // Gemini には Claude のような構造化 Citations API がないため、
        // ドキュメントを user message に詰めて、プロンプトで [n] 記法を強制する
        let mut user_text = String::new();
        if !req.documents.is_empty() {
            user_text.push_str("## 参考ドキュメント\n\n");
            for (i, d) in req.documents.iter().enumerate() {
                user_text.push_str(&format!(
                    "[{}] {} ({})\n{}\n\n",
                    i + 1,
                    d.title,
                    d.source_type,
                    d.text
                ));
            }
        }
        user_text.push_str("## 質問\n\n");
        user_text.push_str(&req.user);
        if !req.documents.is_empty() {
            user_text.push_str(
                "\n\n回答時は根拠となる参照を `[n]` 形式で本文中に明示してください。",
            );
        }

        let mut contents: Vec<Content> = Vec::with_capacity(req.history.len() + 1);
        for h in &req.history {
            let role = match h.role {
                HistoryRole::User => "user",
                HistoryRole::Assistant => "model",
            };
            contents.push(Content {
                role,
                parts: vec![TextPart { text: &h.content }],
            });
        }
        contents.push(Content {
            role: "user",
            parts: vec![TextPart { text: &user_text }],
        });

        let body = GenerateRequest {
            system_instruction: SystemInstruction {
                parts: vec![TextPart { text: &req.system }],
            },
            contents,
            generation_config: Some(GenerationConfig {
                max_output_tokens: 2048,
                temperature: 0.2,
            }),
        };

        let url = format!("{GEMINI_API}/{}:generateContent", req.model);
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API {status}: {txt}");
        }
        let parsed: GenerateResponse = resp.json().await?;

        let mut content = String::new();
        if let Some(cands) = parsed.candidates {
            if let Some(cand) = cands.into_iter().next() {
                if let Some(cc) = cand.content {
                    if let Some(parts) = cc.parts {
                        for p in parts {
                            if let Some(t) = p.text {
                                content.push_str(&t);
                            }
                        }
                    }
                }
            }
        }

        // 回答本文中の [n] を解析して citations を組み立てる
        let citations = extract_citations(&content, &req.documents);

        Ok(LlmResponse { content, citations })
    }
}

fn extract_citations(text: &str, docs: &[super::RagDocument]) -> Vec<Citation> {
    let mut seen = std::collections::BTreeSet::<usize>::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < bytes.len() && bytes[j] == b']' {
                if let Ok(n) = text[i + 1..j].parse::<usize>() {
                    if n >= 1 && n <= docs.len() {
                        seen.insert(n - 1);
                    }
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    seen.into_iter()
        .filter_map(|idx| {
            docs.get(idx).map(|d| Citation {
                title: d.title.clone(),
                url: d.url.clone(),
                source_type: d.source_type.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::RagDocument;

    fn doc(i: usize) -> RagDocument {
        RagDocument {
            title: format!("doc-{i}"),
            url: format!("https://example.com/{i}"),
            source_type: "jira".to_owned(),
            text: String::new(),
        }
    }

    #[test]
    fn extract_simple_citations() {
        let docs = vec![doc(1), doc(2), doc(3)];
        let text = "this is [1] and also [3].";
        let cites = extract_citations(text, &docs);
        assert_eq!(cites.len(), 2);
        let titles: Vec<&str> = cites.iter().map(|c| c.title.as_str()).collect();
        assert!(titles.contains(&"doc-1"));
        assert!(titles.contains(&"doc-3"));
    }

    #[test]
    fn extract_dedupes_citations() {
        let docs = vec![doc(1), doc(2)];
        let text = "[1][1] [2] [1]";
        let cites = extract_citations(text, &docs);
        assert_eq!(cites.len(), 2);
    }

    #[test]
    fn extract_ignores_out_of_range() {
        let docs = vec![doc(1)];
        let text = "[1] [5] [99]";
        let cites = extract_citations(text, &docs);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].title, "doc-1");
    }

    #[test]
    fn extract_ignores_non_digit_brackets() {
        let docs = vec![doc(1)];
        let text = "[foo] [bar] [1abc] [1]";
        let cites = extract_citations(text, &docs);
        assert_eq!(cites.len(), 1);
    }

    #[test]
    fn extract_handles_empty_docs() {
        let cites = extract_citations("[1][2]", &[]);
        assert_eq!(cites.len(), 0);
    }
}
