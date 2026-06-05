use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::atlassian::{AtlassianClient, ConfluenceAttachment, ConfluencePage, JiraIssue};
use crate::chunking;
use crate::config::IngestOptions;
use crate::diagrams::{drawio, mermaid};
use crate::embedding::Embedder;
use crate::vectordb::{Chunk, LanceStore, VectorStore};
use crate::vision::VisionClient;

const EMBED_BATCH: usize = 32;

/// since: 増分同期の境目 ISO8601。None なら全件
pub async fn run_sync<E, V>(
    client: &AtlassianClient,
    embedder: &E,
    store: &V,
    vision: Option<&VisionClient>,
    options: &IngestOptions,
    tx: UnboundedSender<crate::app::SyncEvent>,
    since: Option<&str>,
) -> Result<()>
where
    E: Embedder,
    V: VectorStore,
{
    // ====== Jira ======
    let _ = tx.send(crate::app::SyncEvent::Log("Jira 検索を実行".into()));
    let jql = match since {
        Some(ts) => format!("updated >= '{}' ORDER BY updated DESC", ts),
        None => "ORDER BY updated DESC".to_owned(),
    };
    let issues = client.jira_search(&jql, 5000).await?;
    let _ = tx.send(crate::app::SyncEvent::Log(format!(
        "Jira: {} 件取得",
        issues.len()
    )));
    let mut jira_chunks = jira_to_chunks(client, &issues);
    if options.fetch_attachments && options.enable_vision && vision.is_some() {
        let extras = jira_image_chunks(client, &issues, vision.unwrap(), options, &tx).await;
        jira_chunks.extend(extras);
    }
    embed_and_upsert(embedder, store, jira_chunks, &tx, "jira").await?;
    let _ = tx.send(crate::app::SyncEvent::JiraDone(now_iso()));

    // ====== Confluence ======
    let _ = tx.send(crate::app::SyncEvent::Log("Confluence pages を取得".into()));
    let pages = client.confluence_pages(5000).await?;
    let _ = tx.send(crate::app::SyncEvent::Log(format!(
        "Confluence: {} 件取得",
        pages.len()
    )));
    let mut confluence_chunks = confluence_to_chunks(client, &pages);
    if options.fetch_attachments {
        let extras = confluence_extras(client, &pages, vision, options, &tx).await;
        confluence_chunks.extend(extras);
    }
    embed_and_upsert(embedder, store, confluence_chunks, &tx, "confluence").await?;

    // 新規ページ（version 1）を検出して通知。Auto-review の入力
    let new_pages = detect_new_pages(client, &pages);
    if !new_pages.is_empty() {
        let _ = tx.send(crate::app::SyncEvent::Log(format!(
            "新規 Confluence ページを {} 件検出",
            new_pages.len()
        )));
        let _ = tx.send(crate::app::SyncEvent::NewPagesDetected(new_pages));
    }

    let _ = tx.send(crate::app::SyncEvent::ConfluenceDone(now_iso()));

    Ok(())
}

fn detect_new_pages(
    client: &AtlassianClient,
    pages: &[ConfluencePage],
) -> Vec<crate::app::NewPageInfo> {
    pages
        .iter()
        .filter(|p| p.version.as_ref().map(|v| v.number == 1).unwrap_or(false))
        .filter_map(|p| {
            let storage = p
                .body
                .as_ref()
                .and_then(|b| b.storage.as_ref())
                .map(|s| s.value.as_str())
                .unwrap_or("");
            if storage.is_empty() {
                return None;
            }
            let parsed = chunking::parse_confluence_storage(storage);
            if parsed.text.trim().is_empty() {
                return None;
            }
            Some(crate::app::NewPageInfo {
                id: p.id.clone(),
                title: p.title.clone(),
                url: client.page_url(p),
                text: format!("{}\n\n{}", p.title, parsed.text),
            })
        })
        .collect()
}

pub async fn ensure_index(store: &LanceStore) -> Result<()> {
    store.ensure_vector_index(256).await
}

struct PreChunk {
    id: String,
    source_type: String,
    source_id: String,
    title: String,
    url: String,
    space_or_project: String,
    author: String,
    created_at: i64,
    updated_at: i64,
    labels: Vec<String>,
    text: String,
}

fn jira_to_chunks(client: &AtlassianClient, issues: &[JiraIssue]) -> Vec<PreChunk> {
    let mut out = vec![];
    for issue in issues {
        let title = issue.fields.summary.clone().unwrap_or_default();
        let body = issue
            .fields
            .description
            .as_ref()
            .map(chunking::adf_to_text)
            .unwrap_or_default();
        let combined = if body.is_empty() {
            title.clone()
        } else {
            format!("{title}\n\n{body}")
        };
        if combined.trim().is_empty() {
            continue;
        }
        let parts = chunking::split(&combined);
        for (i, part) in parts.into_iter().enumerate() {
            out.push(PreChunk {
                id: format!("jira:{}:{i}", issue.key),
                source_type: "jira".to_owned(),
                source_id: issue.key.clone(),
                title: title.clone(),
                url: client.issue_url(&issue.key),
                space_or_project: issue
                    .fields
                    .project
                    .as_ref()
                    .map(|p| p.key.clone())
                    .unwrap_or_default(),
                author: String::new(),
                created_at: chunking::parse_iso(issue.fields.created.as_deref()),
                updated_at: chunking::parse_iso(issue.fields.updated.as_deref()),
                labels: issue
                    .fields
                    .status
                    .as_ref()
                    .map(|s| vec![s.name.clone()])
                    .unwrap_or_default(),
                text: part,
            });
        }
    }
    out
}

fn confluence_to_chunks(client: &AtlassianClient, pages: &[ConfluencePage]) -> Vec<PreChunk> {
    let mut out = vec![];
    for page in pages {
        let title = page.title.clone();
        let storage = page
            .body
            .as_ref()
            .and_then(|b| b.storage.as_ref())
            .map(|s| s.value.as_str())
            .unwrap_or("");
        let parsed = if storage.is_empty() {
            chunking::ConfluencePageContent::default()
        } else {
            chunking::parse_confluence_storage(storage)
        };

        // 本文
        let mut combined = title.clone();
        if !parsed.text.is_empty() {
            combined.push_str("\n\n");
            combined.push_str(&parsed.text);
        }
        // Mermaid はテキスト本文に同居させる（small chunk なので分ける必要なし）
        for (i, m) in parsed.mermaid_blocks.iter().enumerate() {
            combined.push_str("\n\n");
            combined.push_str(&mermaid::extract_mermaid_text(
                m,
                Some(&format!("page {} #{}", page.id, i + 1)),
            ));
        }
        if combined.trim().is_empty() {
            continue;
        }

        let url = client.page_url(page);
        let created_at = page
            .version
            .as_ref()
            .and_then(|v| v.created_at.as_deref())
            .map(|s| chunking::parse_iso(Some(s)))
            .unwrap_or(0);

        let parts = chunking::split(&combined);
        for (i, part) in parts.into_iter().enumerate() {
            out.push(PreChunk {
                id: format!("confluence:{}:{i}", page.id),
                source_type: "confluence".to_owned(),
                source_id: page.id.clone(),
                title: title.clone(),
                url: url.clone(),
                space_or_project: page.space_id.clone().unwrap_or_default(),
                author: String::new(),
                created_at,
                updated_at: created_at,
                labels: vec![],
                text: part,
            });
        }
    }
    out
}

/// Confluence の draw.io 図と画像添付を個別チャンクとして生成
async fn confluence_extras(
    client: &AtlassianClient,
    pages: &[ConfluencePage],
    vision: Option<&VisionClient>,
    options: &IngestOptions,
    tx: &UnboundedSender<crate::app::SyncEvent>,
) -> Vec<PreChunk> {
    let mut out = vec![];
    for page in pages {
        let storage = page
            .body
            .as_ref()
            .and_then(|b| b.storage.as_ref())
            .map(|s| s.value.as_str())
            .unwrap_or("");
        if storage.is_empty() {
            continue;
        }
        let parsed = chunking::parse_confluence_storage(storage);
        if parsed.drawio_refs.is_empty() && parsed.image_refs.is_empty() {
            continue;
        }
        // 添付ファイル一覧
        let attachments = match client.confluence_attachments(&page.id).await {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(crate::app::SyncEvent::Log(format!(
                    "添付取得失敗 {}: {e}",
                    page.id
                )));
                continue;
            }
        };

        let url = client.page_url(page);
        let created_at = page
            .version
            .as_ref()
            .and_then(|v| v.created_at.as_deref())
            .map(|s| chunking::parse_iso(Some(s)))
            .unwrap_or(0);

        // draw.io 図
        if options.parse_drawio {
            for (di, drawio_name) in parsed.drawio_refs.iter().enumerate() {
                let candidate = find_drawio_attachment(&attachments, drawio_name);
                let Some(att) = candidate else { continue };
                let Some(dl) = attachment_download_url(&att) else { continue };
                let bytes = match client.download_bytes(&dl).await {
                    Ok((b, _)) => b,
                    Err(e) => {
                        tracing::warn!("drawio download: {e}");
                        continue;
                    }
                };
                let diagram = match drawio::parse_drawio(&bytes) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("drawio parse: {e}");
                        continue;
                    }
                };
                let text = drawio::to_text(&diagram, Some(drawio_name));
                let parts = chunking::split(&text);
                for (pi, part) in parts.into_iter().enumerate() {
                    out.push(PreChunk {
                        id: format!("confluence:{}:drawio:{di}:{pi}", page.id),
                        source_type: "confluence".to_owned(),
                        source_id: page.id.clone(),
                        title: format!("{} (draw.io: {})", page.title, drawio_name),
                        url: url.clone(),
                        space_or_project: page.space_id.clone().unwrap_or_default(),
                        author: String::new(),
                        created_at,
                        updated_at: created_at,
                        labels: vec!["drawio".to_owned()],
                        text: part,
                    });
                }
            }
        }

        // 画像（Vision LLM で説明文化）
        if let Some(v) = vision {
            let mut processed = 0u32;
            for (ii, img_ref) in parsed.image_refs.iter().enumerate() {
                if processed >= options.max_images_per_doc {
                    break;
                }
                let att = match attachments.iter().find(|a| a.title == *img_ref) {
                    Some(a) => a,
                    None => continue,
                };
                let mime = att.media_type.clone().unwrap_or_default();
                if !mime.starts_with("image/") {
                    continue;
                }
                let Some(dl) = attachment_download_url(att) else { continue };
                let bytes = match client.download_bytes(&dl).await {
                    Ok((b, _)) => b,
                    Err(e) => {
                        tracing::warn!("image download: {e}");
                        continue;
                    }
                };
                let desc = match v.describe_image(&bytes, &mime).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("vision: {e}");
                        continue;
                    }
                };
                let chunk_text = format!("[画像: {img_ref}]\n{desc}");
                let parts = chunking::split(&chunk_text);
                for (pi, part) in parts.into_iter().enumerate() {
                    out.push(PreChunk {
                        id: format!("confluence:{}:img:{ii}:{pi}", page.id),
                        source_type: "confluence".to_owned(),
                        source_id: page.id.clone(),
                        title: format!("{} (画像: {})", page.title, img_ref),
                        url: url.clone(),
                        space_or_project: page.space_id.clone().unwrap_or_default(),
                        author: String::new(),
                        created_at,
                        updated_at: created_at,
                        labels: vec!["image".to_owned()],
                        text: part,
                    });
                }
                processed += 1;
            }
        }
    }
    out
}

async fn jira_image_chunks(
    client: &AtlassianClient,
    issues: &[JiraIssue],
    vision: &VisionClient,
    options: &IngestOptions,
    tx: &UnboundedSender<crate::app::SyncEvent>,
) -> Vec<PreChunk> {
    let mut out = vec![];
    for issue in issues {
        let mut processed = 0u32;
        let title = issue.fields.summary.clone().unwrap_or_default();
        for (ai, att) in issue.fields.attachment.iter().enumerate() {
            if processed >= options.max_images_per_doc {
                break;
            }
            let mime = att.mime_type.clone().unwrap_or_default();
            if !mime.starts_with("image/") {
                continue;
            }
            let Some(url) = att.content.as_deref() else { continue };
            let bytes = match client.download_bytes(url).await {
                Ok((b, _)) => b,
                Err(e) => {
                    let _ = tx.send(crate::app::SyncEvent::Log(format!(
                        "jira image download fail {}: {e}",
                        att.filename
                    )));
                    continue;
                }
            };
            let desc = match vision.describe_image(&bytes, &mime).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("vision (jira): {e}");
                    continue;
                }
            };
            let chunk_text = format!("[画像: {}]\n{desc}", att.filename);
            let parts = chunking::split(&chunk_text);
            for (pi, part) in parts.into_iter().enumerate() {
                out.push(PreChunk {
                    id: format!("jira:{}:img:{ai}:{pi}", issue.key),
                    source_type: "jira".to_owned(),
                    source_id: issue.key.clone(),
                    title: format!("{} (画像: {})", title, att.filename),
                    url: client.issue_url(&issue.key),
                    space_or_project: issue
                        .fields
                        .project
                        .as_ref()
                        .map(|p| p.key.clone())
                        .unwrap_or_default(),
                    author: String::new(),
                    created_at: chunking::parse_iso(issue.fields.created.as_deref()),
                    updated_at: chunking::parse_iso(issue.fields.updated.as_deref()),
                    labels: vec!["image".to_owned()],
                    text: part,
                });
            }
            processed += 1;
        }
    }
    out
}

/// drawio 添付の探し方:
/// 1) 正規化（小文字 + 連続空白を1個に縮約 + trim）して比較
/// 2) ベース名（拡張子なし） == 正規化された名前
/// 3) 名前そのものと一致
/// 4) どれも当たらず、`.drawio` 添付がページ内に1つしかなければそれを採用（フォールバック）
fn find_drawio_attachment<'a>(
    attachments: &'a [ConfluenceAttachment],
    name: &str,
) -> Option<&'a ConfluenceAttachment> {
    let needle = normalize_name(name);
    if needle.is_empty() {
        return None;
    }
    // 完全 / ベース名一致
    let direct = attachments.iter().find(|a| {
        let t = normalize_name(&a.title);
        t == needle || base_name(&t) == needle
    });
    if direct.is_some() {
        return direct;
    }
    // ファジー: 先頭一致 + 拡張子が drawio/xml
    let fuzzy = attachments.iter().find(|a| {
        let t = normalize_name(&a.title);
        t.starts_with(&needle) && (t.ends_with(".drawio") || t.ends_with(".xml"))
    });
    if fuzzy.is_some() {
        return fuzzy;
    }
    // フォールバック: ページに drawio 添付が 1 つだけならそれを使う
    let drawios: Vec<&ConfluenceAttachment> = attachments
        .iter()
        .filter(|a| {
            let t = normalize_name(&a.title);
            t.ends_with(".drawio")
                || a.media_type.as_deref() == Some("application/vnd.jgraph.mxfile")
        })
        .collect();
    if drawios.len() == 1 {
        return Some(drawios[0]);
    }
    None
}

fn normalize_name(s: &str) -> String {
    let lower = s.to_lowercase();
    lower.split_whitespace().collect::<Vec<_>>().join(" ").trim().to_owned()
}

fn base_name(s: &str) -> String {
    match s.rfind('.') {
        Some(i) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

fn attachment_download_url(att: &ConfluenceAttachment) -> Option<String> {
    if let Some(d) = &att.download_link {
        return Some(d.clone());
    }
    att.links.as_ref().and_then(|l| l.download.clone())
}

async fn embed_and_upsert<E, V>(
    embedder: &E,
    store: &V,
    chunks: Vec<PreChunk>,
    tx: &UnboundedSender<crate::app::SyncEvent>,
    label: &str,
) -> Result<()>
where
    E: Embedder,
    V: VectorStore,
{
    let total = chunks.len();
    if total == 0 {
        return Ok(());
    }
    let _ = tx.send(crate::app::SyncEvent::Log(format!(
        "{label}: {total} チャンクを埋め込み中..."
    )));

    let mut done = 0usize;
    for batch in chunks.chunks(EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
        let vectors = embedder.embed(&texts).await?;
        let recs: Vec<Chunk> = batch
            .iter()
            .zip(vectors)
            .map(|(p, v)| Chunk {
                id: p.id.clone(),
                source_type: p.source_type.clone(),
                source_id: p.source_id.clone(),
                title: p.title.clone(),
                url: p.url.clone(),
                space_or_project: p.space_or_project.clone(),
                author: p.author.clone(),
                created_at: p.created_at,
                updated_at: p.updated_at,
                labels: p.labels.clone(),
                text: p.text.clone(),
                vector: v,
            })
            .collect();
        store.upsert(recs).await?;
        done += batch.len();
        let _ = tx.send(crate::app::SyncEvent::Log(format!(
            "{label}: {done}/{total} upsert 完了"
        )));
    }
    Ok(())
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AtlassianConfig, IngestOptions};
    use crate::vectordb::{InMemoryStore, SearchFilter};
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 決定論的なテキストハッシュ Embedder。テスト用。
    struct HashEmbedder {
        dim: usize,
    }

    impl Embedder for HashEmbedder {
        async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| hash_to_vec(t, self.dim)).collect())
        }
    }

    fn hash_to_vec(t: &str, dim: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        for (i, b) in t.bytes().enumerate() {
            v[i % dim] += (b as f32) / 255.0;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }

    fn mk_client(server: &MockServer) -> AtlassianClient {
        AtlassianClient::new(AtlassianConfig {
            site_url: server.uri(),
            email: "u@example.com".into(),
            api_token: "t".into(),
        })
    }

    fn att(_id: &str, title: &str, media: Option<&str>) -> ConfluenceAttachment {
        ConfluenceAttachment {
            title: title.into(),
            media_type: media.map(|s| s.into()),
            download_link: Some(format!("/wiki/download/attachments/x/{title}")),
            links: None,
        }
    }

    #[test]
    fn find_drawio_exact_match() {
        let atts = vec![att("1", "flow.drawio", None), att("2", "other.png", None)];
        let r = find_drawio_attachment(&atts, "flow").unwrap();
        assert_eq!(r.title, "flow.drawio");
    }

    #[test]
    fn find_drawio_case_insensitive() {
        let atts = vec![att("1", "Flow.DrawIO", None)];
        let r = find_drawio_attachment(&atts, "flow").unwrap();
        assert_eq!(r.title, "Flow.DrawIO");
    }

    #[test]
    fn find_drawio_whitespace_normalized() {
        let atts = vec![att("1", "Auth  Flow.drawio", None)];
        let r = find_drawio_attachment(&atts, "Auth Flow").unwrap();
        assert_eq!(r.title, "Auth  Flow.drawio");
    }

    #[test]
    fn find_drawio_fallback_single_diagram() {
        // 名前が一致しなくても drawio 添付が1つしかなければそれを返す
        let atts = vec![
            att("1", "unrelated_name.drawio", None),
            att("2", "screenshot.png", Some("image/png")),
        ];
        let r = find_drawio_attachment(&atts, "Something Else").unwrap();
        assert_eq!(r.title, "unrelated_name.drawio");
    }

    #[test]
    fn find_drawio_no_match_when_multiple_candidates() {
        let atts = vec![
            att("1", "a.drawio", None),
            att("2", "b.drawio", None),
            att("3", "c.png", None),
        ];
        // 候補が複数あって名前一致しないので None
        assert!(find_drawio_attachment(&atts, "totally_different").is_none());
    }

    #[tokio::test]
    async fn ingest_jira_creates_text_chunk() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issues": [{
                    "key": "PROJ-1",
                    "fields": {
                        "summary": "ログイン障害",
                        "description": {
                            "type": "doc",
                            "content": [{
                                "type": "paragraph",
                                "content": [{"type": "text", "text": "再現手順は..."}]
                            }]
                        },
                        "project": {"key": "PROJ", "name": "Project"},
                        "status": {"name": "Open"},
                        "created": "2026-05-01T00:00:00.000+0000",
                        "updated": "2026-05-02T00:00:00.000+0000"
                    }
                }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []})))
            .mount(&server)
            .await;

        let client = mk_client(&server);
        let embedder = HashEmbedder { dim: 8 };
        let store = InMemoryStore::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let opts = IngestOptions {
            fetch_attachments: false,
            parse_drawio: false,
            enable_vision: false,
            max_images_per_doc: 0,
            vision_model: "".into(), auto_review_new_pages: false, max_auto_reviews: 0,
        };

        run_sync(&client, &embedder, &store, None, &opts, tx, None)
            .await
            .expect("run_sync");

        let hits = store
            .search(&hash_to_vec("ログイン障害", 8), 10, &SearchFilter::default())
            .await
            .unwrap();
        assert!(!hits.is_empty(), "no chunks created");
        let jira = hits
            .iter()
            .find(|h| h.chunk.source_id == "PROJ-1")
            .expect("PROJ-1 chunk missing");
        assert_eq!(jira.chunk.source_type, "jira");
        assert!(jira.chunk.text.contains("ログイン障害") || jira.chunk.text.contains("再現手順"));
        assert!(jira.chunk.url.ends_with("/browse/PROJ-1"));
        assert_eq!(jira.chunk.space_or_project, "PROJ");
    }

    #[tokio::test]
    async fn ingest_confluence_creates_text_chunk() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"issues": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "p1",
                    "title": "認証フロー設計",
                    "spaceId": "DEV",
                    "version": {"number": 1, "createdAt": "2026-05-10T00:00:00Z"},
                    "body": {"storage": {"value": "<p>認証は OAuth2 を使う</p>"}}
                }]
            })))
            .mount(&server)
            .await;

        let client = mk_client(&server);
        let embedder = HashEmbedder { dim: 8 };
        let store = InMemoryStore::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let opts = IngestOptions {
            fetch_attachments: false,
            parse_drawio: false,
            enable_vision: false,
            max_images_per_doc: 0,
            vision_model: "".into(), auto_review_new_pages: false, max_auto_reviews: 0,
        };

        run_sync(&client, &embedder, &store, None, &opts, tx, None)
            .await
            .unwrap();

        let hits = store
            .search(&hash_to_vec("認証フロー設計", 8), 10, &SearchFilter::default())
            .await
            .unwrap();
        let conf = hits
            .iter()
            .find(|h| h.chunk.source_id == "p1")
            .expect("p1 chunk missing");
        assert_eq!(conf.chunk.source_type, "confluence");
        assert!(conf.chunk.text.contains("OAuth2"));
        assert_eq!(conf.chunk.space_or_project, "DEV");
    }

    #[tokio::test]
    async fn ingest_confluence_with_drawio_adds_diagram_chunk() {
        let drawio_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile>
  <diagram id="p" name="認証フロー">
    <mxGraphModel><root>
      <mxCell id="0"/>
      <mxCell id="1" parent="0"/>
      <mxCell id="A" value="ユーザー" vertex="1" parent="1"/>
      <mxCell id="B" value="API Gateway" vertex="1" parent="1"/>
      <mxCell id="E" value="HTTP request" edge="1" source="A" target="B" parent="1"/>
    </root></mxGraphModel>
  </diagram>
</mxfile>"#;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"issues": []})))
            .mount(&server)
            .await;
        // page 一覧
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "p1",
                    "title": "認証設計",
                    "spaceId": "DEV",
                    "version": {"number": 1, "createdAt": "2026-05-10T00:00:00Z"},
                    "body": {"storage": {"value": "<p>see diagram</p><ac:structured-macro ac:name=\"drawio\"><ac:parameter ac:name=\"diagramName\">flow</ac:parameter></ac:structured-macro>"}}
                }]
            })))
            .mount(&server)
            .await;
        // 添付一覧
        Mock::given(method("GET"))
            .and(path_regex(r"^/wiki/api/v2/pages/p1/attachments$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [{
                    "id": "att1",
                    "title": "flow.drawio",
                    "mediaType": "application/vnd.jgraph.mxfile",
                    "downloadLink": "/wiki/download/attachments/p1/flow.drawio"
                }]
            })))
            .mount(&server)
            .await;
        // 添付バイナリ
        Mock::given(method("GET"))
            .and(path("/wiki/download/attachments/p1/flow.drawio"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(drawio_xml.as_bytes().to_vec()))
            .mount(&server)
            .await;

        let client = mk_client(&server);
        let embedder = HashEmbedder { dim: 8 };
        let store = InMemoryStore::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let opts = IngestOptions {
            fetch_attachments: true,
            parse_drawio: true,
            enable_vision: false,
            max_images_per_doc: 0,
            vision_model: "".into(), auto_review_new_pages: false, max_auto_reviews: 0,
        };

        run_sync(&client, &embedder, &store, None, &opts, tx, None)
            .await
            .unwrap();

        let hits = store
            .search(&hash_to_vec("ユーザー API Gateway", 8), 10, &SearchFilter::default())
            .await
            .unwrap();
        // drawio チャンクが含まれることを id 形式で確認
        let drawio = hits
            .iter()
            .find(|h| h.chunk.id.contains(":drawio:"))
            .expect("drawio chunk missing");
        assert!(drawio.chunk.labels.contains(&"drawio".to_owned()));
        assert!(
            drawio.chunk.text.contains("ユーザー"),
            "got: {}",
            drawio.chunk.text
        );
        assert!(
            drawio.chunk.text.contains("HTTP request"),
            "got: {}",
            drawio.chunk.text
        );
    }
}
