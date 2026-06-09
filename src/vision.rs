use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};

const GEMINI_API: &str = "https://generativelanguage.googleapis.com/v1beta/models";

/// 画像を Gemini Vision で説明文化する。
/// OCR と図の構造説明を同時に行うプロンプトを使用。
pub struct VisionClient {
    api_key: String,
    model: String,
    http: reqwest::Client,
}

impl VisionClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            api_key,
            model,
            http: reqwest::Client::new(),
        }
    }

    pub async fn describe_image(&self, bytes: &[u8], mime_type: &str) -> Result<String> {
        if self.api_key.is_empty() {
            anyhow::bail!("Gemini API key is empty");
        }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let body = GenerateRequest {
            contents: vec![Content {
                role: "user",
                parts: vec![
                    Part::InlineData {
                        inline_data: InlineData {
                            mime_type,
                            data: &data,
                        },
                    },
                    Part::Text {
                        text: VISION_PROMPT,
                    },
                ],
            }],
            generation_config: Some(GenerationConfig {
                max_output_tokens: 1024,
                temperature: 0.1,
            }),
        };

        let url = format!("{GEMINI_API}/{}:generateContent", self.model);
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
            anyhow::bail!("Gemini Vision {status}: {txt}");
        }
        let parsed: GenerateResponse = resp.json().await?;
        let text = parsed
            .candidates
            .and_then(|mut c| c.pop())
            .and_then(|c| c.content)
            .and_then(|c| c.parts)
            .map(|parts| parts.into_iter().filter_map(|p| p.text).collect::<String>())
            .unwrap_or_default();
        Ok(text)
    }
}

const VISION_PROMPT: &str = "\
あなたは技術ドキュメント用の画像解析アシスタントです。\
以下の画像について、社内ナレッジ検索でヒットしやすいように構造化された日本語の説明を生成してください:\n\
\n\
1. 【種別】 スクリーンショット / アーキ図 / フローチャート / グラフ / その他、のどれか\n\
2. 【テキスト】 画像内に含まれる全ての可読テキスト（OCR相当）\n\
3. 【内容】 画像が何を表しているかの要約（2-3文）\n\
4. 【構造】 図の場合は登場要素と関係性をリスト化\n\
\n\
専門用語はそのまま残してください。情報がない項目は省略可。\
";

#[derive(Serialize)]
struct GenerateRequest<'a> {
    contents: Vec<Content<'a>>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Serialize)]
struct Content<'a> {
    role: &'a str,
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Part<'a> {
    Text {
        text: &'a str,
    },
    InlineData {
        #[serde(rename = "inline_data")]
        inline_data: InlineData<'a>,
    },
}

#[derive(Serialize)]
struct InlineData<'a> {
    mime_type: &'a str,
    data: &'a str,
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
