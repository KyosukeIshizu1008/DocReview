use eframe::egui;
use tokio::sync::mpsc;

use crate::chat::{ask_rag, ChatMessage, Citation, Role};
use crate::llm::LlmActive;

use super::{init_store_embedder, theme, ChatEvent, JiraAiApp};

impl JiraAiApp {
    pub(super) fn drain_chat_events(&mut self) {
        if let Some(rx) = self.chat_rx.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    ChatEvent::Response(content, citations) => {
                        self.chat_history.push(ChatMessage {
                            role: Role::Assistant,
                            content,
                            citations,
                        });
                        self.chat_busy = false;
                    }
                    ChatEvent::Failed(msg) => {
                        self.chat_history.push(ChatMessage {
                            role: Role::Assistant,
                            content: format!("エラー: {msg}"),
                            citations: vec![],
                        });
                        self.chat_busy = false;
                    }
                }
            }
        }
    }

    pub(super) fn show_chat(&mut self, ui: &mut egui::Ui) {
        let scroll_target = self.scroll_to_msg.take();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                theme::section_heading(ui, "チャット");
                theme::subtitle(ui, "同期済みナレッジに質問して、根拠つきで確認します");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.chat_busy {
                    ui.spinner();
                    ui.small(egui::RichText::new("回答を生成中").color(theme::MUTED));
                }
            });
        });

        let composer_height = 76.0;
        let transcript_height = (ui.available_height() - composer_height - 14.0).max(260.0);
        theme::card()
            .fill(theme::PANEL)
            .inner_margin(egui::Margin::same(0.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("chat_transcript")
                    .auto_shrink([false, false])
                    .max_height(transcript_height)
                    .show(ui, |ui| {
                        ui.set_min_height(transcript_height);
                        ui.set_width(ui.available_width());
                        if self.chat_history.is_empty() && !self.chat_busy {
                            render_empty_state(ui);
                        }
                        for (idx, msg) in self.chat_history.iter().enumerate() {
                            let resp = render_message(ui, msg);
                            if scroll_target == Some(idx) {
                                resp.scroll_to_me(Some(egui::Align::Center));
                            }
                            ui.add_space(10.0);
                        }
                        if self.chat_busy {
                            theme::bubble(false).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(
                                        egui::RichText::new("関連ページを探して回答を組み立てています")
                                            .color(theme::MUTED),
                                    );
                                });
                            });
                        }
                    });
            });

        ui.add_space(8.0);
        egui::Frame::none()
            .fill(theme::PANEL)
            .stroke(egui::Stroke::new(1.0, theme::ACCENT_SOFT))
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(12.0, 10.0))
            .show(ui, |ui| {
                ui.small(egui::RichText::new("Ask Atlas").strong().color(theme::MUTED));
                ui.add_space(2.0);
                ui.horizontal_centered(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.chat_input)
                            .desired_width((ui.available_width() - 78.0).max(160.0))
                            .hint_text("例: 最新の仕様変更と関連チケットを要約"),
                    );
                    let send = ui.add_enabled(!self.chat_busy, theme::primary_button("送信"));
                    let submitted = send.clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if submitted && !self.chat_input.trim().is_empty() {
                        let question = std::mem::take(&mut self.chat_input);
                        // 現在の質問を追加する前の履歴を渡す
                        let history_before = self.chat_history.clone();
                        self.chat_history.push(ChatMessage {
                            role: Role::User,
                            content: question.clone(),
                            citations: vec![],
                        });
                        self.kick_chat(question, history_before);
                    }
                });
            });
    }

    fn kick_chat(&mut self, question: String, history: Vec<ChatMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.chat_rx = Some(rx);
        self.chat_busy = true;

        let store_cell = self.store.clone();
        let (embed_key, embed_model) = self.embedding_params();
        let llm_cfg = self.config.llm.clone();
        self.rt.spawn(async move {
            let (store, embedder) =
                match init_store_embedder(&store_cell, embed_key, embed_model).await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx.send(ChatEvent::Failed(format!("init: {e:#}")));
                        return;
                    }
                };
            let llm = LlmActive::from_config(&llm_cfg);
            match ask_rag(
                &embedder,
                store.as_ref(),
                &llm,
                &llm_cfg.model,
                10,
                &question,
                &history,
            )
            .await
            {
                Ok((content, citations)) => {
                    let _ = tx.send(ChatEvent::Response(content, citations));
                }
                Err(e) => {
                    let _ = tx.send(ChatEvent::Failed(format!("{e:#}")));
                }
            }
        });
    }
}

fn render_empty_state(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(102.0);
        ui.set_max_width(560.0);
        ui.label(
            egui::RichText::new("何を調べますか？")
                .strong()
                .size(19.0)
                .color(theme::TEXT),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("仕様、障害、議事録、過去チケットを横断して質問できます")
                .color(theme::MUTED),
        );
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            theme::chip(ui, "仕様差分", theme::ACCENT, theme::ACCENT_SOFT);
            theme::chip(ui, "関連チケット", theme::JIRA, theme::SURFACE);
            theme::chip(
                ui,
                "Confluence 要約",
                theme::CONFLUENCE,
                theme::SUCCESS_SOFT,
            );
        });
    });
}

fn render_message(ui: &mut egui::Ui, msg: &ChatMessage) -> egui::Response {
    let is_user = matches!(msg.role, Role::User);
    let (name, fg, bg) = if is_user {
        ("あなた", theme::ACCENT, theme::ACCENT_SOFT)
    } else {
        ("AI", theme::PANEL, theme::ACCENT)
    };
    let max_width = (ui.available_width() * 0.78).max(320.0);
    let layout = if is_user {
        egui::Layout::right_to_left(egui::Align::TOP)
    } else {
        egui::Layout::left_to_right(egui::Align::TOP)
    };
    ui.with_layout(layout, |ui| {
        theme::bubble(is_user).show(ui, |ui| {
            ui.set_max_width(max_width);
            theme::chip(ui, name, fg, bg);
            ui.add_space(5.0);
            ui.label(egui::RichText::new(&msg.content).color(theme::TEXT));
            if !msg.citations.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("参照")
                        .small()
                        .strong()
                        .color(theme::MUTED),
                );
                for (i, c) in msg.citations.iter().enumerate() {
                    render_citation(ui, i + 1, c);
                }
            }
        });
    })
    .response
}

fn render_citation(ui: &mut egui::Ui, idx: usize, c: &Citation) {
    ui.horizontal(|ui| {
        ui.small(format!("[{idx}]"));
        ui.hyperlink_to(&c.title, &c.url);
        ui.small(format!("({})", c.source_type));
    });
}
