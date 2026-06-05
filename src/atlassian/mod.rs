use anyhow::Result;
use base64::Engine;
use reqwest::header;
use serde::de::DeserializeOwned;

use crate::config::AtlassianConfig;

pub mod confluence;
pub mod jira;

pub use confluence::{ConfluenceAttachment, ConfluencePage};
pub use jira::{JiraAttachment, JiraIssue};

#[derive(Clone)]
pub struct AtlassianClient {
    pub cfg: AtlassianConfig,
    http: reqwest::Client,
}

impl AtlassianClient {
    pub fn new(cfg: AtlassianConfig) -> Self {
        let mut headers = header::HeaderMap::new();
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", cfg.email, cfg.api_token));
        if let Ok(v) = header::HeaderValue::from_str(&format!("Basic {basic}")) {
            headers.insert(header::AUTHORIZATION, v);
        }
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent("jira_ai/0.1")
            .build()
            .expect("reqwest client");
        Self { cfg, http }
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("HTTP {status} {url}: {body}");
        }
        Ok(resp.json::<T>().await?)
    }

    /// 同じ Basic 認証でバイナリをダウンロード。相対 URL なら site_url を頭に付ける。
    pub async fn download_bytes(&self, url_or_path: &str) -> Result<(Vec<u8>, Option<String>)> {
        let full_url = if url_or_path.starts_with("http") {
            url_or_path.to_owned()
        } else if url_or_path.starts_with('/') {
            format!("{}{}", self.site(), url_or_path)
        } else {
            format!("{}/{}", self.site(), url_or_path)
        };
        let resp = self.http.get(&full_url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("HTTP {status} {full_url}: {body}");
        }
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_owned());
        let bytes = resp.bytes().await?.to_vec();
        Ok((bytes, ct))
    }

    pub fn site(&self) -> &str {
        self.cfg.site_url.trim_end_matches('/')
    }
}
