use quick_xml::events::Event;
use quick_xml::Reader;
use text_splitter::{ChunkConfig, TextSplitter};

/// 1チャンクあたりの最大文字数（multilingual-e5 の context を考慮した安全値）
const CHUNK_CHARS: usize = 1200;
const CHUNK_OVERLAP: usize = 150;

pub fn split(text: &str) -> Vec<String> {
    let cfg = ChunkConfig::new(CHUNK_CHARS)
        .with_overlap(CHUNK_OVERLAP)
        .expect("overlap < size");
    let splitter = TextSplitter::new(cfg);
    splitter.chunks(text).map(|s| s.to_owned()).collect()
}

/// Confluence の storage format からテキストとマクロ情報を抽出
#[derive(Debug, Default, Clone)]
pub struct ConfluencePageContent {
    pub text: String,
    /// drawio マクロの diagramName 一覧
    pub drawio_refs: Vec<String>,
    /// mermaid ブロックのソース一覧
    pub mermaid_blocks: Vec<String>,
    /// 本文中の `<ri:attachment ri:filename="..."/>` 一覧
    pub image_refs: Vec<String>,
}

/// Confluence の storage XHTML を解析する。
/// quick-xml で要素を走査し、テキスト・マクロ・画像参照を分離して取り出す。
pub fn parse_confluence_storage(html: &str) -> ConfluencePageContent {
    let mut out = ConfluencePageContent::default();
    let wrapped = format!("<root>{html}</root>");
    let mut reader = Reader::from_str(&wrapped);
    {
        let cfg = reader.config_mut();
        cfg.trim_text(false);
        cfg.check_end_names = false;
        cfg.allow_unmatched_ends = true;
    }
    let mut buf = Vec::new();

    let mut in_drawio = false;
    let mut in_mermaid = false;
    let mut in_param = false;
    let mut current_param_name: Option<String> = None;
    let mut param_buf = String::new();
    let mut in_plain_body = false;
    let mut plain_body_buf = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let raw_name = e.name();
                let name = std::str::from_utf8(raw_name.as_ref()).unwrap_or("");
                match name {
                    "ac:structured-macro" => {
                        let macro_name = attr_get(&e, "ac:name").unwrap_or_default();
                        match macro_name.as_str() {
                            "drawio" => in_drawio = true,
                            "mermaid-cloud" | "mermaid" => in_mermaid = true,
                            _ => {}
                        }
                    }
                    "ac:parameter" => {
                        if in_drawio || in_mermaid {
                            in_param = true;
                            current_param_name = attr_get(&e, "ac:name");
                            param_buf.clear();
                        }
                    }
                    "ac:plain-text-body" => {
                        if in_mermaid {
                            in_plain_body = true;
                            plain_body_buf.clear();
                        }
                    }
                    "ri:attachment" => {
                        if !in_drawio && !in_mermaid {
                            if let Some(filename) = attr_get(&e, "ri:filename") {
                                out.image_refs.push(filename);
                            }
                        }
                    }
                    "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" => {
                        if !in_drawio && !in_mermaid {
                            out.text.push('\n');
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let raw_name = e.name();
                let name = std::str::from_utf8(raw_name.as_ref()).unwrap_or("");
                match name {
                    "ac:structured-macro" => {
                        in_drawio = false;
                        in_mermaid = false;
                    }
                    "ac:parameter" => {
                        if in_drawio && current_param_name.as_deref() == Some("diagramName") {
                            let name = param_buf.trim().to_owned();
                            if !name.is_empty() {
                                out.drawio_refs.push(name);
                            }
                        }
                        in_param = false;
                        current_param_name = None;
                    }
                    "ac:plain-text-body" => {
                        if in_mermaid {
                            let block = plain_body_buf.trim().to_owned();
                            if !block.is_empty() {
                                out.mermaid_blocks.push(block);
                            }
                        }
                        in_plain_body = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    if in_plain_body {
                        plain_body_buf.push_str(&s);
                    } else if in_param {
                        param_buf.push_str(&s);
                    } else if !in_drawio && !in_mermaid {
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            out.text.push_str(trimmed);
                            out.text.push(' ');
                        }
                    }
                }
            }
            Ok(Event::CData(c)) => {
                let s = String::from_utf8_lossy(c.as_ref());
                if in_plain_body {
                    plain_body_buf.push_str(&s);
                } else if in_param {
                    param_buf.push_str(&s);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                tracing::debug!("confluence storage parse warning: {e}");
                break;
            }
            _ => {}
        }
        buf.clear();
    }

    // 連続改行を圧縮
    out.text = compress_whitespace(&out.text);
    out
}

fn attr_get(e: &quick_xml::events::BytesStart, key: &str) -> Option<String> {
    for attr in e.attributes().with_checks(false).flatten() {
        if attr.key.as_ref() == key.as_bytes() {
            if let Ok(v) = attr.unescape_value() {
                return Some(v.into_owned());
            }
        }
    }
    None
}

fn compress_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_nl = false;
    let mut last_space = false;
    for c in s.chars() {
        if c == '\n' {
            if !last_nl {
                out.push('\n');
            }
            last_nl = true;
            last_space = false;
        } else if c.is_whitespace() {
            if !last_space && !last_nl {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_nl = false;
            last_space = false;
        }
    }
    out
}

/// Jira ADF (Atlassian Document Format / JSON) からテキストを抽出。
/// 主要なノード種別に対応:
/// - text / paragraph / heading
/// - bulletList / orderedList / listItem / taskList / taskItem
/// - codeBlock (言語名も注釈)
/// - blockquote / panel (info/warning/note 等)
/// - table / tableRow / tableHeader / tableCell
/// - mention / emoji / hardBreak / rule
/// - mediaSingle / media (filename を残す)
/// - inlineCard (URL を残す)
pub fn adf_to_text(node: &serde_json::Value) -> String {
    let mut out = String::new();
    walk_adf(node, &mut out);
    // 連続改行を最大 2 個に圧縮
    let mut compact = String::with_capacity(out.len());
    let mut nl_run = 0;
    for c in out.chars() {
        if c == '\n' {
            nl_run += 1;
            if nl_run <= 2 {
                compact.push('\n');
            }
        } else {
            nl_run = 0;
            compact.push(c);
        }
    }
    compact
}

fn walk_adf(node: &serde_json::Value, out: &mut String) {
    let node_type = node.get("type").and_then(|t| t.as_str()).unwrap_or("");

    // ノード種別ごとの特殊処理
    match node_type {
        "text" => {
            if let Some(t) = node.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
            return;
        }
        "hardBreak" => {
            out.push('\n');
            return;
        }
        "rule" => {
            out.push_str("\n---\n");
            return;
        }
        "mention" => {
            if let Some(t) = node
                .get("attrs")
                .and_then(|a| a.get("text"))
                .and_then(|v| v.as_str())
            {
                out.push_str(t);
            } else if let Some(id) = node
                .get("attrs")
                .and_then(|a| a.get("id"))
                .and_then(|v| v.as_str())
            {
                out.push('@');
                out.push_str(id);
            }
            return;
        }
        "emoji" => {
            if let Some(name) = node
                .get("attrs")
                .and_then(|a| a.get("shortName"))
                .and_then(|v| v.as_str())
            {
                out.push_str(name);
            } else if let Some(t) = node
                .get("attrs")
                .and_then(|a| a.get("text"))
                .and_then(|v| v.as_str())
            {
                out.push_str(t);
            }
            return;
        }
        "media" => {
            // 画像本体は別経路（添付） で扱うので、ファイル名だけメモる
            if let Some(name) = node
                .get("attrs")
                .and_then(|a| a.get("alt"))
                .and_then(|v| v.as_str())
            {
                out.push_str(&format!("[画像: {name}]\n"));
            } else if let Some(id) = node
                .get("attrs")
                .and_then(|a| a.get("id"))
                .and_then(|v| v.as_str())
            {
                out.push_str(&format!("[画像:id={id}]\n"));
            }
            return;
        }
        "inlineCard" => {
            if let Some(url) = node
                .get("attrs")
                .and_then(|a| a.get("url"))
                .and_then(|v| v.as_str())
            {
                out.push_str(url);
            }
            return;
        }
        "codeBlock" => {
            let lang = node
                .get("attrs")
                .and_then(|a| a.get("language"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            out.push_str(&format!("```{lang}\n"));
            if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
                for child in content {
                    walk_adf(child, out);
                }
            }
            out.push_str("\n```\n");
            return;
        }
        "panel" => {
            let panel_type = node
                .get("attrs")
                .and_then(|a| a.get("panelType"))
                .and_then(|v| v.as_str())
                .unwrap_or("info");
            out.push_str(&format!("[{panel_type}]\n"));
            if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
                for child in content {
                    walk_adf(child, out);
                }
            }
            out.push('\n');
            return;
        }
        "table" => {
            if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
                for row in content {
                    walk_adf_table_row(row, out);
                }
            }
            out.push('\n');
            return;
        }
        _ => {}
    }

    // 子要素を再帰
    if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
        for child in content {
            walk_adf(child, out);
        }
    }

    // ブロック要素の後ろに改行
    match node_type {
        "paragraph" | "heading" | "blockquote" | "listItem" | "taskItem" => out.push('\n'),
        "bulletList" | "orderedList" | "taskList" => out.push('\n'),
        _ => {}
    }
}

fn walk_adf_table_row(row: &serde_json::Value, out: &mut String) {
    if let Some(cells) = row.get("content").and_then(|c| c.as_array()) {
        let mut cell_texts: Vec<String> = vec![];
        for cell in cells {
            let mut buf = String::new();
            if let Some(content) = cell.get("content").and_then(|c| c.as_array()) {
                for child in content {
                    walk_adf(child, &mut buf);
                }
            }
            cell_texts.push(buf.trim().replace('\n', " "));
        }
        out.push_str(&cell_texts.join(" | "));
        out.push('\n');
    }
}

/// ISO8601 -> UNIX 秒。失敗時は 0
pub fn parse_iso(s: Option<&str>) -> i64 {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short_text_yields_one_chunk() {
        let chunks = split("hello world");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello world");
    }

    #[test]
    fn split_long_text_yields_multiple_chunks() {
        let long = "あ".repeat(3000);
        let chunks = split(&long);
        assert!(
            chunks.len() > 1,
            "expected multiple chunks, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(c.chars().count() <= CHUNK_CHARS + CHUNK_OVERLAP);
        }
    }

    #[test]
    fn parse_iso_handles_valid() {
        let t = parse_iso(Some("2026-06-04T12:00:00Z"));
        assert!(t > 0);
        assert!(t > 1700000000); // > 2023
    }

    #[test]
    fn parse_iso_handles_invalid() {
        assert_eq!(parse_iso(Some("not-a-date")), 0);
        assert_eq!(parse_iso(None), 0);
    }

    #[test]
    fn adf_to_text_extracts_table() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "table",
                "content": [
                    {
                        "type": "tableRow",
                        "content": [
                            {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "name"}]}]},
                            {"type": "tableHeader", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "value"}]}]}
                        ]
                    },
                    {
                        "type": "tableRow",
                        "content": [
                            {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "timeout"}]}]},
                            {"type": "tableCell", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "30s"}]}]}
                        ]
                    }
                ]
            }]
        });
        let text = adf_to_text(&adf);
        assert!(text.contains("name | value"), "got: {text:?}");
        assert!(text.contains("timeout | 30s"), "got: {text:?}");
    }

    #[test]
    fn adf_to_text_extracts_mention_and_emoji() {
        let adf = serde_json::json!({
            "type": "paragraph",
            "content": [
                {"type": "text", "text": "hello "},
                {"type": "mention", "attrs": {"id": "12345", "text": "@kyosuke"}},
                {"type": "text", "text": " "},
                {"type": "emoji", "attrs": {"shortName": ":wave:"}}
            ]
        });
        let text = adf_to_text(&adf);
        assert!(text.contains("@kyosuke"), "got: {text:?}");
        assert!(text.contains(":wave:"), "got: {text:?}");
    }

    #[test]
    fn adf_to_text_extracts_codeblock() {
        let adf = serde_json::json!({
            "type": "codeBlock",
            "attrs": {"language": "rust"},
            "content": [{"type": "text", "text": "fn main() {}"}]
        });
        let text = adf_to_text(&adf);
        assert!(text.contains("```rust"), "got: {text:?}");
        assert!(text.contains("fn main() {}"), "got: {text:?}");
    }

    #[test]
    fn adf_to_text_extracts_panel() {
        let adf = serde_json::json!({
            "type": "panel",
            "attrs": {"panelType": "warning"},
            "content": [{
                "type": "paragraph",
                "content": [{"type": "text", "text": "watch out"}]
            }]
        });
        let text = adf_to_text(&adf);
        assert!(text.contains("[warning]"), "got: {text:?}");
        assert!(text.contains("watch out"), "got: {text:?}");
    }

    #[test]
    fn adf_to_text_extracts_paragraph() {
        let adf: serde_json::Value = serde_json::json!({
            "type": "doc",
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        { "type": "text", "text": "Hello " },
                        { "type": "text", "text": "world" }
                    ]
                },
                {
                    "type": "heading",
                    "content": [{ "type": "text", "text": "Section 2" }]
                }
            ]
        });
        let text = adf_to_text(&adf);
        assert!(text.contains("Hello world"), "got: {text:?}");
        assert!(text.contains("Section 2"), "got: {text:?}");
    }

    #[test]
    fn compress_whitespace_collapses_runs() {
        assert_eq!(compress_whitespace("a   b\n\n\nc"), "a b\nc");
        assert_eq!(compress_whitespace("  hello   world  "), " hello world ");
    }

    #[test]
    fn parse_confluence_storage_extracts_plain_text() {
        let html = "<p>Hello <strong>world</strong></p>";
        let r = parse_confluence_storage(html);
        assert!(r.text.contains("Hello"), "text: {}", r.text);
        assert!(r.text.contains("world"), "text: {}", r.text);
    }

    #[test]
    fn parse_confluence_storage_detects_drawio_macro() {
        let html = r#"<p>before</p>
<ac:structured-macro ac:name="drawio">
  <ac:parameter ac:name="diagramName">MyDiagram</ac:parameter>
  <ac:parameter ac:name="contentId">12345</ac:parameter>
</ac:structured-macro>
<p>after</p>"#;
        let r = parse_confluence_storage(html);
        assert_eq!(r.drawio_refs, vec!["MyDiagram"]);
        assert!(r.text.contains("before"));
        assert!(r.text.contains("after"));
    }

    #[test]
    fn parse_confluence_storage_detects_mermaid_macro() {
        let html = r#"<ac:structured-macro ac:name="mermaid-cloud">
  <ac:parameter ac:name="code">
    <ac:plain-text-body><![CDATA[graph TD
  A --> B]]></ac:plain-text-body>
  </ac:parameter>
</ac:structured-macro>"#;
        let r = parse_confluence_storage(html);
        assert_eq!(
            r.mermaid_blocks.len(),
            1,
            "got blocks: {:?}",
            r.mermaid_blocks
        );
        assert!(r.mermaid_blocks[0].contains("graph TD"));
        assert!(r.mermaid_blocks[0].contains("A --> B"));
    }

    #[test]
    fn parse_confluence_storage_detects_image_refs() {
        let html = r#"<p>see this</p>
<ac:image><ri:attachment ri:filename="screenshot.png"/></ac:image>
<ac:image><ri:attachment ri:filename="arch.svg"/></ac:image>"#;
        let r = parse_confluence_storage(html);
        assert_eq!(r.image_refs, vec!["screenshot.png", "arch.svg"]);
    }

    #[test]
    fn parse_confluence_storage_separates_drawio_from_body_text() {
        // drawio マクロ内のテキストは本文テキストに含まれてはいけない
        let html = r#"<p>visible</p>
<ac:structured-macro ac:name="drawio">
  <ac:parameter ac:name="diagramName">Hidden</ac:parameter>
</ac:structured-macro>"#;
        let r = parse_confluence_storage(html);
        assert!(r.text.contains("visible"));
        assert!(
            !r.text.contains("Hidden"),
            "drawio param leaked into body: {}",
            r.text
        );
    }
}
