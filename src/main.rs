use anyhow::Result;

mod app;
mod atlassian;
mod chat;
mod chunking;
mod config;
mod diagrams;
mod embedding;
mod ingest;
mod llm;
mod review;
mod vectordb;
mod vision;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,jira_ai=debug")),
        )
        .init();

    // tokio タスク内 panic を確実にログへ流す（デフォルトは silent）
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".into());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<unknown payload>".into());
        tracing::error!("panic at {location}: {payload}");
        default_hook(info);
    }));

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title("JiraAI - Jira & Confluence RAG"),
        ..Default::default()
    };

    eframe::run_native(
        "JiraAI",
        native_options,
        Box::new(|cc| Ok(Box::new(app::JiraAiApp::new(cc)?))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}
