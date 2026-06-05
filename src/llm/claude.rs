use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{HistoryRole, Llm, LlmRequest, LlmResponse};
use crate::chat::Citation;

const ANTHROPIC_API: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct ClaudeClient {
    api_key: String,
    http: reqwest::Client,
}

impl ClaudeClient {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            http: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: Vec<ContentBlock<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ContentBlock<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "document")]
    Document {
        source: DocSource<'a>,
        title: &'a str,
        citations: CitationsCfg,
    },
}

#[derive(Serialize)]
struct DocSource<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    media_type: &'a str,
    data: &'a str,
}

#[derive(Serialize)]
struct CitationsCfg {
    enabled: bool,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ResponseBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponseBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(default)]
        citations: Vec<ResponseCitation>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ResponseCitation {
    #[serde(default)]
    document_index: Option<usize>,
    #[serde(default)]
    document_title: Option<String>,
}

impl Llm for ClaudeClient {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse> {
        let mut messages: Vec<Message> = Vec::with_capacity(req.history.len() + 1);
        for h in &req.history {
            let role = match h.role {
                HistoryRole::User => "user",
                HistoryRole::Assistant => "assistant",
            };
            messages.push(Message {
                role,
                content: vec![ContentBlock::Text { text: &h.content }],
            });
        }

        let mut current_blocks: Vec<ContentBlock> = req
            .documents
            .iter()
            .map(|d| ContentBlock::Document {
                source: DocSource {
                    ty: "text",
                    media_type: "text/plain",
                    data: &d.text,
                },
                title: &d.title,
                citations: CitationsCfg { enabled: true },
            })
            .collect();
        current_blocks.push(ContentBlock::Text { text: &req.user });
        messages.push(Message {
            role: "user",
            content: current_blocks,
        });

        let body = MessagesRequest {
            model: &req.model,
            max_tokens: 2048,
            system: &req.system,
            messages,
        };

        let resp = self
            .http
            .post(ANTHROPIC_API)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            anyhow::bail!("Claude API {status}: {txt}");
        }
        let parsed: MessagesResponse = resp.json().await?;

        let mut content = String::new();
        let mut citations: Vec<Citation> = vec![];
        for b in parsed.content {
            if let ResponseBlock::Text { text, citations: cs } = b {
                content.push_str(&text);
                for c in cs {
                    let title = c.document_title.unwrap_or_default();
                    let idx = c.document_index.unwrap_or(0);
                    let doc = req.documents.get(idx);
                    citations.push(Citation {
                        title: if title.is_empty() {
                            doc.map(|d| d.title.clone()).unwrap_or_default()
                        } else {
                            title
                        },
                        url: doc.map(|d| d.url.clone()).unwrap_or_default(),
                        source_type: doc.map(|d| d.source_type.clone()).unwrap_or_default(),
                    });
                }
            }
        }
        Ok(LlmResponse { content, citations })
    }
}
