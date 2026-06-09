use eframe::egui;
use tokio::sync::mpsc;

use crate::llm::LlmActive;
use crate::review::{review_document, ReviewResult};

use super::{init_store_embedder, save_review_history, theme, JiraAiApp, NewPageInfo, ReviewEvent};

impl JiraAiApp {
    pub(super) fn drain_review_events(&mut self) {
        let mut changed = false;
        if let Some(rx) = self.review.rx.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    ReviewEvent::Manual(r) => {
                        self.review.manual_result = Some(r);
                        self.review.busy = false;
                    }
                    ReviewEvent::Auto(r) => {
                        self.review.auto_results.push(r);
                        self.review.pending_auto = self.review.pending_auto.saturating_sub(1);
                        changed = true;
                    }
                    ReviewEvent::Failed(msg) => {
                        tracing::error!("review failed: {msg}");
                        self.review.busy = false;
                        self.review.pending_auto = self.review.pending_auto.saturating_sub(1);
                    }
                }
            }
        }
        if changed {
            if let Err(e) = save_review_history(&self.review.auto_results) {
                tracing::warn!("review history save failed: {e:?}");
            }
        }
    }

    pub(super) fn show_review(&mut self, ui: &mut egui::Ui) {
        theme::section_heading(ui, "レビュー");

        // ---- 貼り付けレビュー ----
        ui.label("貼り付けレビュー（新規ドキュメントを既存ナレッジと照らしてチェック）");
        ui.add(
            egui::TextEdit::singleline(&mut self.review.paste_title)
                .desired_width(400.0)
                .hint_text("タイトル（任意）"),
        );
        ui.add(
            egui::TextEdit::multiline(&mut self.review.paste_text)
                .desired_rows(8)
                .desired_width(f32::INFINITY)
                .hint_text("レビュー対象ドキュメントを貼り付け..."),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.review.busy && !self.review.paste_text.trim().is_empty(),
                    theme::primary_button("レビュー実行"),
                )
                .clicked()
            {
                let title = if self.review.paste_title.trim().is_empty() {
                    "(無題)".to_owned()
                } else {
                    self.review.paste_title.clone()
                };
                let text = self.review.paste_text.clone();
                self.kick_manual_review(title, text);
            }
            if ui.button("クリア").clicked() {
                self.review.paste_title.clear();
                self.review.paste_text.clear();
                self.review.manual_result = None;
            }
            if self.review.busy {
                ui.spinner();
                ui.small("レビュー中...");
            }
        });
        if let Some(r) = self.review.manual_result.clone() {
            ui.add_space(8.0);
            render_review_result(ui, &r);
        }

        // ---- 自動レビュー結果 ----
        ui.add_space(16.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!(
                "自動レビュー履歴: {} 件",
                self.review.auto_results.len()
            ));
            if self.review.pending_auto > 0 {
                ui.spinner();
                ui.small(format!("実行中: 残り {} 件", self.review.pending_auto));
            }
            if !self.review.auto_results.is_empty() && ui.button("履歴をクリア").clicked() {
                self.review.auto_results.clear();
                let _ = save_review_history(&self.review.auto_results);
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("review_history")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let auto_results = self.review.auto_results.clone();
                for (idx, r) in auto_results.iter().enumerate().rev() {
                    let is_expanded = self.review.expanded_idx == Some(idx);
                    let header = format!(
                        "[{}] {}  —  {}",
                        r.overall_grade,
                        r.doc_title,
                        r.summary.chars().take(80).collect::<String>()
                    );
                    let resp = ui.add(egui::SelectableLabel::new(is_expanded, header));
                    if resp.clicked() {
                        self.review.expanded_idx = if is_expanded { None } else { Some(idx) };
                    }
                    if is_expanded {
                        ui.indent(format!("auto_review_{idx}"), |ui| {
                            render_review_result(ui, r);
                        });
                    }
                    ui.separator();
                }
            });
    }

    fn kick_manual_review(&mut self, title: String, text: String) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.review.rx = Some(rx);
        self.review.busy = true;
        let store_cell = self.store.clone();
        let (embed_key, embed_model) = self.embedding_params();
        let llm_cfg = self.config.llm.clone();
        self.rt.spawn(async move {
            let (store, embedder) =
                match init_store_embedder(&store_cell, embed_key, embed_model).await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx.send(ReviewEvent::Failed(format!("init: {e:#}")));
                        return;
                    }
                };
            let llm = LlmActive::from_config(&llm_cfg);
            match review_document(
                &title,
                "manual",
                "",
                &text,
                &embedder,
                store.as_ref(),
                &llm,
                &llm_cfg.model,
            )
            .await
            {
                Ok(r) => {
                    let _ = tx.send(ReviewEvent::Manual(r));
                }
                Err(e) => {
                    let _ = tx.send(ReviewEvent::Failed(format!("{e:#}")));
                }
            }
        });
    }

    pub(super) fn kick_auto_reviews(&mut self, pages: Vec<NewPageInfo>) {
        // 既存タスク用の sender はもう無い場合があるので、毎回新しいチャネルを作る
        let (tx, rx) = mpsc::unbounded_channel();
        self.review.rx = Some(rx);
        self.review.pending_auto += pages.len();
        let store_cell = self.store.clone();
        let (embed_key, embed_model) = self.embedding_params();
        let llm_cfg = self.config.llm.clone();
        for page in pages {
            let tx = tx.clone();
            let store_cell = store_cell.clone();
            let embed_key = embed_key.clone();
            let embed_model = embed_model.clone();
            let llm_cfg = llm_cfg.clone();
            self.rt.spawn(async move {
                let (store, embedder) =
                    match init_store_embedder(&store_cell, embed_key, embed_model).await {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = tx.send(ReviewEvent::Failed(format!("auto init: {e:#}")));
                            return;
                        }
                    };
                let llm = LlmActive::from_config(&llm_cfg);
                match review_document(
                    &page.title,
                    &page.id,
                    &page.url,
                    &page.text,
                    &embedder,
                    store.as_ref(),
                    &llm,
                    &llm_cfg.model,
                )
                .await
                {
                    Ok(r) => {
                        let _ = tx.send(ReviewEvent::Auto(r));
                    }
                    Err(e) => {
                        let _ = tx.send(ReviewEvent::Failed(format!("auto: {e:#}")));
                    }
                }
            });
        }
    }
}

fn render_review_result(ui: &mut egui::Ui, r: &ReviewResult) {
    ui.horizontal(|ui| {
        ui.colored_label(egui::Color32::LIGHT_BLUE, &r.doc_title);
        ui.small(format!("  評価: {}", r.overall_grade));
        if !r.doc_url.is_empty() {
            ui.hyperlink_to("元ページを開く", &r.doc_url);
        }
    });
    if !r.summary.is_empty() {
        ui.label(&r.summary);
    }
    ui.add_space(4.0);

    if !r.contradictions.is_empty() {
        ui.colored_label(
            egui::Color32::from_rgb(220, 100, 100),
            format!("⚠️ 矛盾 ({}件)", r.contradictions.len()),
        );
        for c in &r.contradictions {
            ui.indent(format!("c_{}", &c.new_excerpt), |ui| {
                ui.label(format!("新: \"{}\"", c.new_excerpt));
                ui.horizontal(|ui| {
                    ui.label(format!("既存: {} —", c.existing_title));
                    if !c.existing_url.is_empty() {
                        ui.hyperlink_to("開く", &c.existing_url);
                    }
                });
                ui.label(format!("→ {}", c.explanation));
            });
            ui.separator();
        }
    }
    if !r.duplicates.is_empty() {
        ui.colored_label(
            egui::Color32::from_rgb(220, 180, 80),
            format!("🔁 重複 ({}件)", r.duplicates.len()),
        );
        for d in &r.duplicates {
            ui.indent(format!("d_{}", &d.new_section), |ui| {
                ui.label(format!("新: {}", d.new_section));
                ui.horizontal(|ui| {
                    ui.label(format!("既存: {} —", d.existing_title));
                    if !d.existing_url.is_empty() {
                        ui.hyperlink_to("開く", &d.existing_url);
                    }
                });
                if !d.overlap_note.is_empty() {
                    ui.label(format!("→ {}", d.overlap_note));
                }
            });
            ui.separator();
        }
    }
    if !r.gaps.is_empty() {
        ui.colored_label(
            egui::Color32::from_rgb(100, 150, 220),
            format!("❓ 欠落 ({}件)", r.gaps.len()),
        );
        for g in &r.gaps {
            ui.label(format!("• {g}"));
        }
        ui.separator();
    }
    if !r.terminology.is_empty() {
        ui.colored_label(
            egui::Color32::from_rgb(180, 180, 180),
            format!("🏷️ 用語不整合 ({}件)", r.terminology.len()),
        );
        for t in &r.terminology {
            ui.label(format!(
                "• \"{}\" → 既存「{}」({})",
                t.new_term, t.existing_term, t.suggestion
            ));
        }
        ui.separator();
    }
    if !r.raw_response.is_empty() {
        ui.collapsing("LLM 生レスポンス (JSON パース失敗)", |ui| {
            ui.monospace(&r.raw_response);
        });
    }
}
