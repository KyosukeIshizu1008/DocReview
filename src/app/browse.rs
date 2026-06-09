use eframe::egui;
use tokio::sync::mpsc;

use crate::vectordb::{Chunk, SearchFilter};

use super::{get_or_init_store, theme, BrowseCounts, BrowseEvent, JiraAiApp};

impl JiraAiApp {
    pub(super) fn drain_browse_events(&mut self) {
        if let Some(rx) = self.browse.rx.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    BrowseEvent::Counts(c) => {
                        self.browse.counts = c;
                    }
                    BrowseEvent::Chunks(c) => {
                        self.browse.chunks = c;
                        self.browse.busy = false;
                    }
                    BrowseEvent::Failed(msg) => {
                        self.browse.busy = false;
                        tracing::error!("browse: {msg}");
                    }
                }
            }
        }
    }

    pub(super) fn show_browse(&mut self, ui: &mut egui::Ui) {
        theme::section_heading(ui, "ブラウザ");

        // 初回 or タブ切替時に読み込み
        if !self.browse.last_loaded && !self.browse.busy {
            self.browse.last_loaded = true;
            self.kick_browse_load();
        }

        // フィルタ + 検索バー
        ui.horizontal(|ui| {
            ui.label("種別:");
            let current = match self.browse.filter_source_type.as_str() {
                "" => "すべて",
                "jira" => "Jira",
                "confluence" => "Confluence",
                other => other,
            };
            let mut changed = false;
            egui::ComboBox::from_id_salt("browse_filter")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.browse.filter_source_type,
                            "".to_owned(),
                            "すべて",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.browse.filter_source_type,
                            "jira".to_owned(),
                            "Jira",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.browse.filter_source_type,
                            "confluence".to_owned(),
                            "Confluence",
                        )
                        .changed();
                });
            ui.label("キーワード:");
            ui.add(
                egui::TextEdit::singleline(&mut self.browse.keyword)
                    .desired_width(200.0)
                    .hint_text("チャンク本文の部分一致"),
            );
            if ui.button("再読込").clicked() || changed {
                self.kick_browse_load();
            }
        });

        ui.add_space(6.0);
        ui.label(format!(
            "合計: {} 件  (jira: {}, confluence: {})",
            self.browse.counts.total, self.browse.counts.jira, self.browse.counts.confluence
        ));

        if self.browse.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small("読み込み中...");
            });
        }

        ui.separator();

        // キーワードフィルタを Rust 側でかける
        let keyword = self.browse.keyword.trim().to_lowercase();
        let chunks_view: Vec<&Chunk> = self
            .browse
            .chunks
            .iter()
            .filter(|c| {
                if keyword.is_empty() {
                    true
                } else {
                    c.text.to_lowercase().contains(&keyword)
                        || c.title.to_lowercase().contains(&keyword)
                }
            })
            .collect();

        ui.small(format!("表示: {} 件 (上限 500件取得)", chunks_view.len()));

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for chunk in chunks_view {
                    render_chunk_row(ui, chunk, &mut self.browse.expanded_id);
                }
            });
    }

    fn kick_browse_load(&mut self) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.browse.rx = Some(rx);
        self.browse.busy = true;
        let store_cell = self.store.clone();
        let filter_st = if self.browse.filter_source_type.is_empty() {
            None
        } else {
            Some(self.browse.filter_source_type.clone())
        };
        self.rt.spawn(async move {
            let store = match get_or_init_store(&store_cell).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(BrowseEvent::Failed(format!("store init: {e:#}")));
                    return;
                }
            };
            // 件数
            let total = store.count_all().await.unwrap_or(0);
            let jira = store.count_by_source("jira").await.unwrap_or(0);
            let confluence = store.count_by_source("confluence").await.unwrap_or(0);
            let _ = tx.send(BrowseEvent::Counts(BrowseCounts {
                total,
                jira,
                confluence,
            }));
            // チャンク一覧
            let filter = SearchFilter {
                source_type: filter_st,
                project_or_space: None,
            };
            match store.list(&filter, 500).await {
                Ok(chunks) => {
                    let _ = tx.send(BrowseEvent::Chunks(chunks));
                }
                Err(e) => {
                    let _ = tx.send(BrowseEvent::Failed(format!("list: {e:#}")));
                }
            }
        });
    }
}

fn render_chunk_row(ui: &mut egui::Ui, chunk: &Chunk, expanded_id: &mut Option<String>) {
    let is_expanded = expanded_id.as_deref() == Some(chunk.id.as_str());
    let preview = chunk
        .text
        .chars()
        .take(120)
        .collect::<String>()
        .replace('\n', " ");
    let summary = format!(
        "[{}] {}  —  {}",
        chunk.source_type,
        chunk.title,
        if preview.chars().count() == 120 {
            format!("{preview}…")
        } else {
            preview
        }
    );
    let resp = ui.add(egui::SelectableLabel::new(is_expanded, summary));
    if resp.clicked() {
        if is_expanded {
            *expanded_id = None;
        } else {
            *expanded_id = Some(chunk.id.clone());
        }
    }
    if is_expanded {
        ui.indent(format!("chunk_detail_{}", chunk.id), |ui| {
            ui.horizontal(|ui| {
                ui.small("ID:");
                ui.monospace(&chunk.id);
            });
            ui.horizontal(|ui| {
                ui.small("URL:");
                ui.hyperlink_to(&chunk.url, &chunk.url);
            });
            ui.horizontal(|ui| {
                ui.small("source_id:");
                ui.monospace(&chunk.source_id);
                ui.separator();
                ui.small("project/space:");
                ui.monospace(&chunk.space_or_project);
            });
            if !chunk.labels.is_empty() {
                ui.horizontal(|ui| {
                    ui.small("labels:");
                    for l in &chunk.labels {
                        ui.small(format!("[{l}]"));
                    }
                });
            }
            ui.add_space(4.0);
            ui.label(&chunk.text);
        });
    }
    ui.separator();
}
