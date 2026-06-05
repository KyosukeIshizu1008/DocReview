use std::io::Read;

use anyhow::{Context, Result};
use base64::Engine;
use flate2::read::DeflateDecoder;
use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Default, Clone)]
pub struct DrawioDiagram {
    pub pages: Vec<DrawioPage>,
}

#[derive(Debug, Default, Clone)]
pub struct DrawioPage {
    pub name: Option<String>,
    pub nodes: Vec<DrawioNode>,
    pub edges: Vec<DrawioEdge>,
}

#[derive(Debug, Clone)]
pub struct DrawioNode {
    pub id: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct DrawioEdge {
    pub source: Option<String>,
    pub target: Option<String>,
    pub value: String,
}

/// `.drawio` ファイルバイト列を解析。圧縮 / 非圧縮 両対応。
pub fn parse_drawio(bytes: &[u8]) -> Result<DrawioDiagram> {
    let mut diagram = DrawioDiagram::default();
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_diagram = false;
    let mut diagram_text = String::new();
    let mut current_page_name: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "diagram" {
                    in_diagram = true;
                    diagram_text.clear();
                    current_page_name = attr_get(&e, "name");
                } else if name == "mxGraphModel" {
                    // 非圧縮ケース: そのまま XML が続く。reader を進めて mxCell を集める
                    let page = parse_mxgraph_model(&mut reader, &mut buf)?;
                    diagram.pages.push(DrawioPage {
                        name: current_page_name.clone(),
                        nodes: page.nodes,
                        edges: page.edges,
                    });
                }
            }
            Ok(Event::Empty(_)) => {
                // self-closing: 何も処理しない（mxCell は mxGraphModel 内で別途処理）
            }
            Ok(Event::Text(t)) if in_diagram => {
                if let Ok(s) = t.unescape() {
                    diagram_text.push_str(&s);
                }
            }
            Ok(Event::CData(c)) if in_diagram => {
                let s = String::from_utf8_lossy(c.as_ref());
                diagram_text.push_str(&s);
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "diagram" {
                    in_diagram = false;
                    let trimmed = diagram_text.trim();
                    if !trimmed.is_empty() {
                        // 圧縮ペイロード: base64 + raw deflate
                        if let Ok(inner) = decode_compressed(trimmed) {
                            let mut sub_reader = Reader::from_str(&inner);
                            sub_reader.config_mut().trim_text(true);
                            let mut sub_buf = Vec::new();
                            // mxGraphModel ルートを探して読む
                            loop {
                                match sub_reader.read_event_into(&mut sub_buf) {
                                    Ok(Event::Start(e2)) if e2.name().as_ref() == b"mxGraphModel" => {
                                        let page = parse_mxgraph_model(&mut sub_reader, &mut sub_buf)?;
                                        diagram.pages.push(DrawioPage {
                                            name: current_page_name.clone(),
                                            nodes: page.nodes,
                                            edges: page.edges,
                                        });
                                        break;
                                    }
                                    Ok(Event::Eof) => break,
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!("inner drawio xml parse: {e}");
                                        break;
                                    }
                                }
                                sub_buf.clear();
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!("drawio xml: {e}"));
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(diagram)
}

struct ParsedModel {
    nodes: Vec<DrawioNode>,
    edges: Vec<DrawioEdge>,
}

fn parse_mxgraph_model<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    buf: &mut Vec<u8>,
) -> Result<ParsedModel> {
    let mut nodes: Vec<DrawioNode> = vec![];
    let mut edges: Vec<DrawioEdge> = vec![];
    loop {
        buf.clear();
        match reader.read_event_into(buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"mxCell" => {
                let id = attr_get(&e, "id").unwrap_or_default();
                let value = attr_get(&e, "value").unwrap_or_default();
                let vertex = attr_get(&e, "vertex").unwrap_or_default() == "1";
                let edge = attr_get(&e, "edge").unwrap_or_default() == "1";
                let source = attr_get(&e, "source");
                let target = attr_get(&e, "target");
                let value_text = strip_html(&value);
                if vertex && !value_text.trim().is_empty() {
                    nodes.push(DrawioNode {
                        id,
                        value: value_text,
                    });
                } else if edge {
                    edges.push(DrawioEdge {
                        source,
                        target,
                        value: value_text,
                    });
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"mxGraphModel" => break,
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("mxGraphModel: {e}")),
            _ => {}
        }
    }
    Ok(ParsedModel { nodes, edges })
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

/// draw.io は base64 + raw deflate（zlib ヘッダなし）でエンコードされた mxGraphModel を持つ
fn decode_compressed(b64: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("drawio base64")?;
    let mut dec = DeflateDecoder::new(bytes.as_slice());
    let mut decompressed = String::new();
    dec.read_to_string(&mut decompressed)
        .context("drawio deflate")?;
    // 一部の draw.io は URL エンコードされた mxGraphModel を返すので解除
    let decoded = url_decode(&decompressed);
    Ok(decoded)
}

/// draw.io の deflate 後ペイロードは URL エンコードされた XML 1本のことが多い。
/// 既に XML ならそのまま、そうでなければパーセントデコードする。
fn url_decode(s: &str) -> String {
    if s.trim_start().starts_with('<') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// HTML タグを剥がす（mxCell の value には `<b>label</b>` のような HTML が入りうる）
fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 図全体を社内ナレッジ検索向けのテキストにシリアライズ
#[allow(dead_code)]
pub fn to_text_pub(d: &DrawioDiagram, title_hint: Option<&str>) -> String {
    to_text(d, title_hint)
}

pub fn to_text(d: &DrawioDiagram, title_hint: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("[draw.io 図");
    if let Some(t) = title_hint {
        out.push_str(": ");
        out.push_str(t);
    }
    out.push_str("]\n");

    for (pi, page) in d.pages.iter().enumerate() {
        if d.pages.len() > 1 {
            let name = page.name.as_deref().unwrap_or("(無題)");
            out.push_str(&format!("--- ページ{} {} ---\n", pi + 1, name));
        }
        if !page.nodes.is_empty() {
            out.push_str("ノード:\n");
            for n in &page.nodes {
                out.push_str(&format!("  - {}\n", n.value));
            }
        }
        if !page.edges.is_empty() {
            out.push_str("接続:\n");
            let mut id_to_label = std::collections::HashMap::<&str, &str>::new();
            for n in &page.nodes {
                id_to_label.insert(n.id.as_str(), n.value.as_str());
            }
            for e in &page.edges {
                let src = e
                    .source
                    .as_deref()
                    .and_then(|s| id_to_label.get(s).copied())
                    .unwrap_or("(?)");
                let dst = e
                    .target
                    .as_deref()
                    .and_then(|s| id_to_label.get(s).copied())
                    .unwrap_or("(?)");
                if e.value.trim().is_empty() {
                    out.push_str(&format!("  {src} -> {dst}\n"));
                } else {
                    out.push_str(&format!("  {src} -> {dst}: {}\n", e.value));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    const UNCOMPRESSED_SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile>
  <diagram id="page1" name="認証フロー">
    <mxGraphModel>
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="User" vertex="1" parent="1"/>
        <mxCell id="3" value="API Gateway" vertex="1" parent="1"/>
        <mxCell id="4" value="Auth Server" vertex="1" parent="1"/>
        <mxCell id="5" value="HTTP request" edge="1" source="2" target="3" parent="1"/>
        <mxCell id="6" value="validate" edge="1" source="3" target="4" parent="1"/>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

    #[test]
    fn parse_uncompressed_drawio() {
        let d = parse_drawio(UNCOMPRESSED_SAMPLE.as_bytes()).expect("parse ok");
        assert_eq!(d.pages.len(), 1);
        let page = &d.pages[0];
        assert_eq!(page.name.as_deref(), Some("認証フロー"));
        assert_eq!(page.nodes.len(), 3);
        let labels: Vec<_> = page.nodes.iter().map(|n| n.value.as_str()).collect();
        assert!(labels.contains(&"User"), "got: {labels:?}");
        assert!(labels.contains(&"API Gateway"), "got: {labels:?}");
        assert!(labels.contains(&"Auth Server"), "got: {labels:?}");
        assert_eq!(page.edges.len(), 2);
    }

    #[test]
    fn to_text_formats_nodes_and_edges() {
        let d = parse_drawio(UNCOMPRESSED_SAMPLE.as_bytes()).unwrap();
        let text = to_text(&d, Some("Sample"));
        assert!(text.contains("Sample"), "title missing: {text}");
        assert!(text.contains("User"), "node missing: {text}");
        assert!(text.contains("User -> API Gateway: HTTP request"), "edge missing: {text}");
        assert!(text.contains("API Gateway -> Auth Server: validate"), "edge missing: {text}");
    }

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(strip_html("<b>label</b>"), "label");
        assert_eq!(strip_html("<font color=\"red\">A</font> <i>B</i>"), "A B");
        assert_eq!(strip_html("plain"), "plain");
    }

    #[test]
    fn parse_compressed_drawio() {
        // mxGraphModel を deflate + base64 で <diagram> 配下に入れる
        let inner = r#"<mxGraphModel><root>
<mxCell id="0"/>
<mxCell id="1" parent="0"/>
<mxCell id="A" value="Node1" vertex="1" parent="1"/>
<mxCell id="B" value="Node2" vertex="1" parent="1"/>
<mxCell id="E" value="link" edge="1" source="A" target="B" parent="1"/>
</root></mxGraphModel>"#;
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(inner.as_bytes()).unwrap();
        let compressed = enc.finish().unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&compressed);

        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile>
  <diagram id="p" name="C">{}</diagram>
</mxfile>"#,
            b64
        );
        let d = parse_drawio(xml.as_bytes()).expect("parse ok");
        assert_eq!(d.pages.len(), 1, "pages: {:?}", d.pages);
        let p = &d.pages[0];
        assert_eq!(p.nodes.len(), 2);
        assert_eq!(p.edges.len(), 1);
    }

    #[test]
    fn url_decode_passthrough_xml() {
        let xml = "<root>hi</root>";
        assert_eq!(url_decode(xml), xml);
    }

    #[test]
    fn url_decode_percent_encoded() {
        assert_eq!(url_decode("a%20b%2Bc"), "a b+c");
    }
}
