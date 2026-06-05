use anyhow::Result;
use serde::Deserialize;

use super::AtlassianClient;

#[derive(Debug, Clone, Deserialize)]
pub struct JiraIssue {
    pub key: String,
    pub fields: JiraFields,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraFields {
    pub summary: Option<String>,
    pub description: Option<serde_json::Value>,
    pub updated: Option<String>,
    pub created: Option<String>,
    pub project: Option<JiraProject>,
    pub status: Option<JiraStatus>,
    #[serde(default)]
    pub attachment: Vec<JiraAttachment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraAttachment {
    pub id: String,
    pub filename: String,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    /// ダウンロード用 URL（フル URL で返ってくる）
    pub content: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraProject {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JiraStatus {
    pub name: String,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    issues: Vec<JiraIssue>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

impl AtlassianClient {
    /// JQL で issue を取得（Cloud の新エンドポイント `/rest/api/3/search/jql` を使用）。
    /// `nextPageToken` を辿って max_total 件まで取得。
    pub async fn jira_search(&self, jql: &str, max_total: usize) -> Result<Vec<JiraIssue>> {
        const PER_PAGE: u32 = 100;
        let mut out: Vec<JiraIssue> = vec![];
        let mut next_token: Option<String> = None;
        loop {
            let mut url = format!(
                "{}/rest/api/3/search/jql?jql={}&maxResults={}&fields=summary,description,updated,created,project,status,attachment",
                self.site(),
                urlencoding(jql),
                PER_PAGE,
            );
            if let Some(t) = &next_token {
                url.push_str(&format!("&nextPageToken={}", urlencoding(t)));
            }
            let resp: SearchResponse = self.get_json(&url).await?;
            let got = resp.issues.len();
            out.extend(resp.issues);
            next_token = resp.next_page_token;
            if next_token.is_none() || got == 0 || out.len() >= max_total {
                break;
            }
        }
        out.truncate(max_total);
        Ok(out)
    }

    pub fn issue_url(&self, key: &str) -> String {
        format!("{}/browse/{}", self.site(), key)
    }
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AtlassianConfig;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> AtlassianClient {
        AtlassianClient::new(AtlassianConfig {
            site_url: server.uri(),
            email: "u@example.com".into(),
            api_token: "tok".into(),
        })
    }

    #[tokio::test]
    async fn jira_search_paginates_until_no_token() {
        let server = MockServer::start().await;

        // 1st call: no nextPageToken in URL
        Mock::given(method("GET"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(|req: &wiremock::Request| {
                let q = req.url.query().unwrap_or("");
                if !q.contains("nextPageToken=") {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "issues": [
                            {"key": "P-1", "fields": {"summary": "first"}},
                            {"key": "P-2", "fields": {"summary": "second"}}
                        ],
                        "nextPageToken": "abc"
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "issues": [
                            {"key": "P-3", "fields": {"summary": "third"}}
                        ]
                    }))
                }
            })
            .mount(&server)
            .await;

        let client = client_for(&server);
        let issues = client.jira_search("ORDER BY updated DESC", 100).await.unwrap();
        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0].key, "P-1");
        assert_eq!(issues[2].key, "P-3");
    }

    #[tokio::test]
    async fn jira_search_respects_max_total() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issues": (0..150).map(|i| serde_json::json!({
                    "key": format!("P-{i}"),
                    "fields": {"summary": format!("s{i}")}
                })).collect::<Vec<_>>(),
                "nextPageToken": "next"
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let issues = client.jira_search("ORDER BY updated DESC", 50).await.unwrap();
        assert_eq!(issues.len(), 50);
    }

    #[test]
    fn issue_url_format() {
        let client = AtlassianClient::new(AtlassianConfig {
            site_url: "https://my.atlassian.net/".into(),
            email: "x".into(),
            api_token: "y".into(),
        });
        // trailing slash should be trimmed
        assert_eq!(client.issue_url("ABC-123"), "https://my.atlassian.net/browse/ABC-123");
    }
}
