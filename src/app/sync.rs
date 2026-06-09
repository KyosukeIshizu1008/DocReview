use eframe::egui;
use tokio::sync::mpsc;

use crate::atlassian::AtlassianClient;
use crate::ingest;
use crate::vision::VisionClient;

use super::{init_store_embedder, lock_status, theme, JiraAiApp, NewPageInfo, SyncEvent};

impl JiraAiApp {
    pub(super) fn drain_sync_events(&mut self) {
        let mut finished = false;
        let mut to_review: Vec<NewPageInfo> = vec![];
        if let Some(rx) = self.sync_rx.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                let mut st = lock_status(&self.sync_status);
                match ev {
                    SyncEvent::Log(line) => st.log.push(line),
                    SyncEvent::JiraDone(ts) => {
                        self.config.sync_state.jira_last_synced = Some(ts.clone());
                        st.jira_last_synced = Some(ts);
                        st.log.push("Jira sync 完了".to_owned());
                    }
                    SyncEvent::ConfluenceDone(ts) => {
                        self.config.sync_state.confluence_last_synced = Some(ts.clone());
                        st.confluence_last_synced = Some(ts);
                        st.log.push("Confluence sync 完了".to_owned());
                    }
                    SyncEvent::NewPagesDetected(pages) => {
                        st.log
                            .push(format!("新規 {} 件を自動レビュー候補に", pages.len()));
                        if self.config.ingest.auto_review_new_pages {
                            to_review.extend(pages);
                        }
                    }
                    SyncEvent::Finished => {
                        st.busy = false;
                        finished = true;
                    }
                    SyncEvent::Failed(msg) => {
                        st.busy = false;
                        st.log.push(format!("エラー: {msg}"));
                        finished = true;
                    }
                }
            }
        }
        // 自動レビューをキック（同期完了後）
        if finished && !to_review.is_empty() {
            let max = self.config.ingest.max_auto_reviews as usize;
            to_review.truncate(max);
            self.kick_auto_reviews(to_review);
        }
        if finished {
            self.sync_rx = None;
            // sync_state を永続化
            self.save_config_async();
        }
    }

    pub(super) fn show_sync(&mut self, ui: &mut egui::Ui) {
        theme::section_heading(ui, "同期");

        let busy = lock_status(&self.sync_status).busy;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, theme::primary_button("全件同期"))
                .clicked()
            {
                self.kick_sync(None);
            }
            let since = self.config.sync_state.jira_last_synced.clone();
            let label = if since.is_some() {
                "増分同期"
            } else {
                "増分同期 (未同期)"
            };
            if ui
                .add_enabled(!busy && since.is_some(), egui::Button::new(label))
                .clicked()
            {
                self.kick_sync(since);
            }
            if busy {
                ui.add_space(4.0);
                ui.spinner();
            }
        });

        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("ログ")
                .small()
                .strong()
                .color(theme::MUTED),
        );
        ui.add_space(4.0);
        theme::card().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let st = lock_status(&self.sync_status);
                    if st.log.is_empty() {
                        ui.label(egui::RichText::new("ログはまだありません").color(theme::MUTED));
                    }
                    for line in st.log.iter().rev().take(200) {
                        ui.monospace(line);
                    }
                });
        });
    }

    fn kick_sync(&mut self, since: Option<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.sync_rx = Some(rx);
        {
            let mut st = lock_status(&self.sync_status);
            st.busy = true;
            st.log.push(if since.is_some() {
                "増分同期を開始".to_owned()
            } else {
                "全件同期を開始".to_owned()
            });
        }

        let cfg = self.config.atlassian.clone();
        let options = self.config.ingest.clone();
        let store_cell = self.store.clone();
        let (embed_key, embed_model) = self.embedding_params();
        // Vision も埋め込みと同じ Gemini キー解決を使う（専用キー、無ければ Gemini 利用時の LLM キー）
        let vision_key = embed_key.clone();
        let tx_done = tx.clone();
        self.rt.spawn(async move {
            let _ = tx.send(SyncEvent::Log("Embedder を準備中".into()));
            let (store, embedder) =
                match init_store_embedder(&store_cell, embed_key, embed_model).await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx_done.send(SyncEvent::Failed(format!("init: {e:#}")));
                        let _ = tx_done.send(SyncEvent::Finished);
                        return;
                    }
                };
            let client = AtlassianClient::new(cfg);
            // Vision クライアントは Gemini API キーが設定されていて、かつトグルが ON の場合のみ
            let vision = if options.enable_vision && !vision_key.is_empty() {
                Some(VisionClient::new(vision_key, options.vision_model.clone()))
            } else {
                None
            };
            let result = ingest::run_sync(
                &client,
                &embedder,
                store.as_ref(),
                vision.as_ref(),
                &options,
                tx.clone(),
                since.as_deref(),
            )
            .await;
            if let Err(e) = result {
                let _ = tx.send(SyncEvent::Failed(format!("{e:#}")));
            } else {
                let _ = tx.send(SyncEvent::Log("ベクターindexを最適化中...".into()));
                if let Err(e) = ingest::ensure_index(store.as_ref()).await {
                    let _ = tx.send(SyncEvent::Log(format!("index skip: {e:#}")));
                }
            }
            let _ = tx.send(SyncEvent::Finished);
        });
    }
}
