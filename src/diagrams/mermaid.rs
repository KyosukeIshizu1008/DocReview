/// Mermaid コードブロックはそのままテキストなので、フォーマットだけ整える
pub fn extract_mermaid_text(source: &str, title_hint: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("[Mermaid 図");
    if let Some(t) = title_hint {
        out.push_str(": ");
        out.push_str(t);
    }
    out.push_str("]\n");
    out.push_str(source);
    if !source.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_with_title() {
        let out = extract_mermaid_text("graph TD\n  A --> B", Some("flow"));
        assert!(out.starts_with("[Mermaid 図: flow]"));
        assert!(out.contains("graph TD"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn extract_without_title() {
        let out = extract_mermaid_text("sequenceDiagram\n  A->>B: x\n", None);
        assert!(out.starts_with("[Mermaid 図]"));
        assert!(out.contains("A->>B"));
    }
}
