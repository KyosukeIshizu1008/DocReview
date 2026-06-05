use anyhow::Result;
use serde::Deserialize;

use super::AtlassianClient;

#[derive(Debug, Clone, Deserialize)]
pub struct ConfluencePage {
    pub id: String,
    pub title: String,
    #[serde(rename = "spaceId")]
    pub space_id: Option<String>,
    pub version: Option<PageVersion>,
    pub body: Option<PageBody>,
    #[serde(rename = "_links")]
    pub links: Option<PageLinks>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageVersion {
    pub number: i64,
    /// ISO8601
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageBody {
    pub storage: Option<PageBodyVariant>,
    pub atlas_doc_format: Option<PageBodyVariant>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageBodyVariant {
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageLinks {
    pub webui: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PagesResponse {
    results: Vec<ConfluencePage>,
    #[serde(rename = "_links")]
    links: Option<PaginationLinks>,
}

#[derive(Debug, Deserialize)]
struct PaginationLinks {
    next: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfluenceAttachment {
    pub id: String,
    pub title: String,
    #[serde(rename = "mediaType")]
    pub media_type: Option<String>,
    #[serde(rename = "fileSize")]
    pub file_size: Option<u64>,
    #[serde(rename = "downloadLink")]
    pub download_link: Option<String>,
    #[serde(rename = "_links")]
    pub links: Option<AttachmentLinks>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AttachmentLinks {
    pub download: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AttachmentsResponse {
    results: Vec<ConfluenceAttachment>,
    #[serde(rename = "_links")]
    links: Option<PaginationLinks>,
}

impl AtlassianClient {
    /// ページに紐づく添付ファイル一覧を取得（pagination 対応）
    pub async fn confluence_attachments(
        &self,
        page_id: &str,
    ) -> Result<Vec<ConfluenceAttachment>> {
        let mut out: Vec<ConfluenceAttachment> = vec![];
        let mut url = format!(
            "{}/wiki/api/v2/pages/{}/attachments?limit=100",
            self.site(),
            page_id
        );
        loop {
            let resp: AttachmentsResponse = self.get_json(&url).await?;
            out.extend(resp.results);
            let next = resp.links.as_ref().and_then(|l| l.next.clone());
            match next {
                Some(n) if !n.is_empty() => {
                    url = if n.starts_with("http") {
                        n
                    } else {
                        format!("{}{}", self.site(), n)
                    };
                }
                _ => break,
            }
        }
        Ok(out)
    }

    /// Confluence Cloud v2 API でページ一覧 + body.storage を取得。
    /// `_links.next` を辿って max_total 件まで取得。
    pub async fn confluence_pages(&self, max_total: usize) -> Result<Vec<ConfluencePage>> {
        const PER_PAGE: u32 = 100;
        let mut out: Vec<ConfluencePage> = vec![];
        let mut url = format!(
            "{}/wiki/api/v2/pages?limit={}&body-format=storage",
            self.site(),
            PER_PAGE
        );
        loop {
            let resp: PagesResponse = self.get_json(&url).await?;
            out.extend(resp.results);
            if out.len() >= max_total {
                break;
            }
            let next = resp.links.as_ref().and_then(|l| l.next.clone());
            match next {
                Some(n) if !n.is_empty() => {
                    url = if n.starts_with("http") {
                        n
                    } else {
                        format!("{}{}", self.site(), n)
                    };
                }
                _ => break,
            }
        }
        out.truncate(max_total);
        Ok(out)
    }

    pub fn page_url(&self, page: &ConfluencePage) -> String {
        match &page.links.as_ref().and_then(|l| l.webui.clone()) {
            Some(webui) => format!("{}/wiki{}", self.site(), webui),
            None => format!("{}/wiki/spaces/_/pages/{}", self.site(), page.id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AtlassianConfig;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> AtlassianClient {
        AtlassianClient::new(AtlassianConfig {
            site_url: server.uri(),
            email: "u@example.com".into(),
            api_token: "tok".into(),
        })
    }

    #[tokio::test]
    async fn confluence_pages_follows_next_link() {
        let server = MockServer::start().await;
        let next_path = "/wiki/api/v2/pages?cursor=xyz&limit=100";

        // 1st request: no cursor → return page 1 + _links.next
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages"))
            .respond_with(move |req: &wiremock::Request| {
                let q = req.url.query().unwrap_or("");
                if q.contains("cursor=") {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "results": [
                            {"id": "p3", "title": "page 3"}
                        ]
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "results": [
                            {"id": "p1", "title": "page 1"},
                            {"id": "p2", "title": "page 2"}
                        ],
                        "_links": {"next": next_path}
                    }))
                }
            })
            .mount(&server)
            .await;

        let client = client_for(&server);
        let pages = client.confluence_pages(100).await.unwrap();
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].id, "p1");
        assert_eq!(pages[2].id, "p3");
    }

    #[tokio::test]
    async fn confluence_pages_respects_max_total() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": (0..50).map(|i| serde_json::json!({
                    "id": format!("p{i}"),
                    "title": format!("page {i}")
                })).collect::<Vec<_>>(),
                "_links": {"next": "/wiki/api/v2/pages?cursor=x"}
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let pages = client.confluence_pages(20).await.unwrap();
        assert_eq!(pages.len(), 20);
    }

    #[tokio::test]
    async fn confluence_attachments_returns_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/wiki/api/v2/pages/\d+/attachments$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [
                    {
                        "id": "a1",
                        "title": "diagram.drawio",
                        "mediaType": "application/vnd.jgraph.mxfile",
                        "downloadLink": "/wiki/download/attachments/123/diagram.drawio"
                    },
                    {
                        "id": "a2",
                        "title": "screenshot.png",
                        "mediaType": "image/png",
                        "_links": {"download": "/wiki/download/attachments/123/screenshot.png"}
                    }
                ]
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let atts = client.confluence_attachments("123").await.unwrap();
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].title, "diagram.drawio");
    }

    #[test]
    fn page_url_uses_webui() {
        let client = AtlassianClient::new(AtlassianConfig {
            site_url: "https://my.atlassian.net".into(),
            email: "x".into(),
            api_token: "y".into(),
        });
        let page = ConfluencePage {
            id: "1".into(),
            title: "t".into(),
            space_id: None,
            version: None,
            body: None,
            links: Some(PageLinks {
                webui: Some("/spaces/DEV/pages/1/Title".into()),
            }),
        };
        assert_eq!(
            client.page_url(&page),
            "https://my.atlassian.net/wiki/spaces/DEV/pages/1/Title"
        );
    }
}
