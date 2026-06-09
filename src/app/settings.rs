use eframe::egui;

use crate::config::LlmProviderKind;

use super::{theme, JiraAiApp};

/// セクション小見出し（薄字・太字）。
fn group_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(text).strong().color(theme::MUTED));
    ui.add_space(2.0);
}

impl JiraAiApp {
    pub(super) fn show_settings(&mut self, ui: &mut egui::Ui) {
        theme::section_heading(ui, "設定");

        group_label(ui, "Atlassian Cloud");
        egui::Grid::new("atlassian_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Site URL");
                if ui
                    .text_edit_singleline(&mut self.config.atlassian.site_url)
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("Email");
                if ui
                    .text_edit_singleline(&mut self.config.atlassian.email)
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("API Token");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.config.atlassian.api_token)
                            .password(true),
                    )
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();
            });

        ui.add_space(12.0);
        group_label(ui, "LLM プロバイダ");
        egui::ComboBox::from_label("")
            .selected_text(match self.config.llm.kind {
                LlmProviderKind::Claude => "Claude",
                LlmProviderKind::Gemini => "Gemini",
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_value(&mut self.config.llm.kind, LlmProviderKind::Claude, "Claude")
                    .changed()
                {
                    self.config_dirty = true;
                }
                if ui
                    .selectable_value(&mut self.config.llm.kind, LlmProviderKind::Gemini, "Gemini")
                    .changed()
                {
                    self.config_dirty = true;
                }
            });

        egui::Grid::new("llm_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Model");
                if ui
                    .text_edit_singleline(&mut self.config.llm.model)
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("API Key");
                if ui
                    .add(egui::TextEdit::singleline(&mut self.config.llm.api_key).password(true))
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();
            });

        ui.add_space(12.0);
        group_label(ui, "埋め込み (Gemini)");
        egui::Grid::new("embedding_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Model");
                if ui
                    .text_edit_singleline(&mut self.config.embedding.model)
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("API Key");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.config.embedding.api_key)
                            .password(true)
                            .hint_text("空欄なら Gemini 利用時の LLM キーを流用"),
                    )
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();
            });

        ui.add_space(12.0);
        group_label(ui, "取り込みオプション");
        egui::Grid::new("ingest_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("添付ファイル取得");
                if ui
                    .checkbox(&mut self.config.ingest.fetch_attachments, "")
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("draw.io 解析");
                if ui
                    .checkbox(&mut self.config.ingest.parse_drawio, "")
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("画像 Vision 解析");
                if ui
                    .checkbox(
                        &mut self.config.ingest.enable_vision,
                        "Gemini Vision で画像を説明文化（API コスト発生）",
                    )
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("Vision 用モデル");
                if ui
                    .text_edit_singleline(&mut self.config.ingest.vision_model)
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("1ドキュメントあたりの最大画像数");
                let mut images_str = self.config.ingest.max_images_per_doc.to_string();
                if ui.text_edit_singleline(&mut images_str).changed() {
                    if let Ok(n) = images_str.parse::<u32>() {
                        self.config.ingest.max_images_per_doc = n;
                        self.config_dirty = true;
                    }
                }
                ui.end_row();

                ui.label("新規ページの自動レビュー");
                if ui
                    .checkbox(
                        &mut self.config.ingest.auto_review_new_pages,
                        "同期後に新規 Confluence ページを LLM レビュー（コスト発生）",
                    )
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();

                ui.label("1同期あたり最大レビュー件数");
                let mut reviews_str = self.config.ingest.max_auto_reviews.to_string();
                if ui.text_edit_singleline(&mut reviews_str).changed() {
                    if let Ok(n) = reviews_str.parse::<u32>() {
                        self.config.ingest.max_auto_reviews = n;
                        self.config_dirty = true;
                    }
                }
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.add(theme::primary_button("保存")).clicked() {
                // keyring への書き込みはバックグラウンドで実行（UI を固めない）
                self.save_config_async();
                self.config_dirty = false;
            }
            if self.config_dirty {
                ui.colored_label(egui::Color32::YELLOW, "未保存の変更があります");
            }
        });
    }
}
