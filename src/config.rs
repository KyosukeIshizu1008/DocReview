use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const APP_QUALIFIER: &str = "co.jp";
const APP_ORG: &str = "oracleberry";
const APP_NAME: &str = "JiraAI";
const KEYRING_SERVICE: &str = "jira_ai";

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub atlassian: AtlassianConfig,
    pub llm: LlmConfig,
    pub sync_state: SyncState,
    #[serde(default)]
    pub ingest: IngestOptions,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Gemini 埋め込みモデル名（例: text-embedding-004）
    pub model: String,
    /// Gemini API キー。空なら LLM が Gemini のときそのキーを流用する。
    /// メモリ上のみで保持し、永続化は keyring 経由。
    #[serde(skip)]
    pub api_key: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "text-embedding-004".to_owned(),
            api_key: String::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct IngestOptions {
    /// 添付ファイル取得を有効化
    pub fetch_attachments: bool,
    /// draw.io XML を取得・解析
    pub parse_drawio: bool,
    /// 画像を Gemini Vision で説明文化（API コストあり）
    pub enable_vision: bool,
    /// 1ページ/issue あたりの最大画像処理数
    pub max_images_per_doc: u32,
    /// Vision 用モデル
    pub vision_model: String,
    /// 新規ドキュメントを同期後に自動レビュー（LLM コストあり）
    #[serde(default)]
    pub auto_review_new_pages: bool,
    /// 1回の同期で auto-review する最大件数
    #[serde(default = "default_max_reviews")]
    pub max_auto_reviews: u32,
}

fn default_max_reviews() -> u32 {
    10
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            fetch_attachments: true,
            parse_drawio: true,
            enable_vision: false,
            max_images_per_doc: 5,
            vision_model: "gemini-2.5-flash".to_owned(),
            auto_review_new_pages: false,
            max_auto_reviews: 10,
        }
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct AtlassianConfig {
    /// 例: https://yourorg.atlassian.net
    pub site_url: String,
    pub email: String,
    /// メモリ上のみで保持。永続化は keyring 経由
    #[serde(skip)]
    pub api_token: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub kind: LlmProviderKind,
    pub model: String,
    #[serde(skip)]
    pub api_key: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            kind: LlmProviderKind::Gemini,
            model: "gemini-2.5-flash".to_owned(),
            api_key: String::new(),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum LlmProviderKind {
    Claude,
    Gemini,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct SyncState {
    pub jira_last_synced: Option<String>,
    pub confluence_last_synced: Option<String>,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        let mut cfg: AppConfig = serde_json::from_str(&raw).context("parse config json")?;

        // secrets: keyring から復元
        if let Ok(token) = get_secret("atlassian_token") {
            cfg.atlassian.api_token = token;
        }
        if let Ok(key) = get_secret("llm_api_key") {
            cfg.llm.api_key = key;
        }
        if let Ok(key) = get_secret("embedding_api_key") {
            cfg.embedding.api_key = key;
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, raw).with_context(|| format!("write config {}", path.display()))?;

        // secrets: keyring に書く
        set_secret("atlassian_token", &self.atlassian.api_token)?;
        set_secret("llm_api_key", &self.llm.api_key)?;
        set_secret("embedding_api_key", &self.embedding.api_key)?;
        Ok(())
    }
}

fn config_path() -> Result<PathBuf> {
    let pd = directories::ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME)
        .context("ProjectDirs unavailable")?;
    Ok(pd.config_dir().join("config.json"))
}

pub fn data_dir() -> Result<PathBuf> {
    let pd = directories::ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME)
        .context("ProjectDirs unavailable")?;
    Ok(pd.data_dir().to_path_buf())
}

fn get_secret(name: &str) -> Result<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, name)?;
    Ok(entry.get_password()?)
}

fn set_secret(name: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, name)?;
    if value.is_empty() {
        let _ = entry.delete_credential();
        return Ok(());
    }
    entry.set_password(value)?;
    Ok(())
}
