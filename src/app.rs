use std::sync::{Arc, Mutex};

use anyhow::Result;
use eframe::egui;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, OnceCell};

use crate::atlassian::AtlassianClient;
use crate::chat::{ask_rag, Citation, ChatMessage, Role};
use crate::config::{AppConfig, LlmProviderKind};
use crate::embedding::LocalEmbedder;
use crate::ingest;
use crate::llm::LlmActive;
use crate::review::{review_document, ReviewResult};
use crate::vectordb::{self, Chunk, LanceStore, SearchFilter};
use crate::vision::VisionClient;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Chat,
    Sync,
    Browse,
    Review,
    Settings,
}

pub struct JiraAiApp {
    rt: Runtime,
    active_tab: Tab,

    config: AppConfig,
    config_dirty: bool,

    chat_input: String,
    chat_history: Vec<ChatMessage>,
    chat_busy: bool,
    chat_rx: Option<mpsc::UnboundedReceiver<ChatEvent>>,

    sync_status: Arc<Mutex<SyncStatus>>,
    sync_rx: Option<mpsc::UnboundedReceiver<SyncEvent>>,

    // 共有リソース（遅延初期化）
    store: Arc<OnceCell<Arc<LanceStore>>>,
    embedder: Arc<OnceCell<Arc<LocalEmbedder>>>,

    // Browse タブ状態
    browse: BrowseState,

    // Review タブ状態
    review: ReviewState,
}

#[derive(Default)]
struct ReviewState {
    paste_title: String,
    paste_text: String,
    busy: bool,
    pending_auto: usize,
    rx: Option<mpsc::UnboundedReceiver<ReviewEvent>>,
    manual_result: Option<ReviewResult>,
    auto_results: Vec<ReviewResult>,
    expanded_idx: Option<usize>,
}

#[derive(Debug)]
enum ReviewEvent {
    Manual(ReviewResult),
    Auto(ReviewResult),
    Failed(String),
}

#[derive(Default)]
struct BrowseState {
    filter_source_type: String, // "" = all
    keyword: String,
    counts: BrowseCounts,
    chunks: Vec<Chunk>,
    busy: bool,
    last_loaded: bool,
    rx: Option<mpsc::UnboundedReceiver<BrowseEvent>>,
    expanded_id: Option<String>,
}

#[derive(Default, Clone, Debug)]
struct BrowseCounts {
    total: usize,
    jira: usize,
    confluence: usize,
}

#[derive(Debug)]
enum BrowseEvent {
    Counts(BrowseCounts),
    Chunks(Vec<Chunk>),
    Failed(String),
}

#[derive(Default, Clone)]
struct SyncStatus {
    busy: bool,
    jira_last_synced: Option<String>,
    confluence_last_synced: Option<String>,
    log: Vec<String>,
}

#[derive(Debug)]
pub enum SyncEvent {
    Log(String),
    JiraDone(String),
    ConfluenceDone(String),
    /// 同期で見つかった新規ページ（version.number == 1）。auto-review の入力に使う
    NewPagesDetected(Vec<NewPageInfo>),
    Finished,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct NewPageInfo {
    pub id: String,
    pub title: String,
    pub url: String,
    pub text: String,
}

#[derive(Debug)]
enum ChatEvent {
    Response(String, Vec<Citation>),
    Failed(String),
}

impl JiraAiApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        Self::install_jp_font(&cc.egui_ctx);

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let mut config = AppConfig::load().unwrap_or_default();
        // 同期状態を UI 反映
        let sync_status = Arc::new(Mutex::new(SyncStatus {
            jira_last_synced: config.sync_state.jira_last_synced.clone(),
            confluence_last_synced: config.sync_state.confluence_last_synced.clone(),
            ..Default::default()
        }));
        // 旧版で空フィールドが入っていたら埋める
        if config.llm.model.is_empty() {
            config.llm.model = match config.llm.kind {
                LlmProviderKind::Gemini => "gemini-2.5-flash".to_owned(),
                LlmProviderKind::Claude => "claude-haiku-4-5".to_owned(),
            };
        }

        let auto_results = load_review_history().unwrap_or_default();
        Ok(Self {
            rt,
            active_tab: Tab::Chat,
            config,
            config_dirty: false,
            chat_input: String::new(),
            chat_history: vec![],
            chat_busy: false,
            chat_rx: None,
            sync_status,
            sync_rx: None,
            store: Arc::new(OnceCell::new()),
            embedder: Arc::new(OnceCell::new()),
            browse: BrowseState::default(),
            review: ReviewState {
                auto_results,
                ..Default::default()
            },
        })
    }

    fn install_jp_font(ctx: &egui::Context) {
        let mut fonts = egui::FontDefinitions::default();
        // OS 別の日本語フォント探索パス
        let candidates: &[&str] = &[
            // macOS
            "/System/Library/Fonts/ヒラギノ角ゴシック W4.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/AquaKana.ttc",
            "/Library/Fonts/Arial Unicode.ttf",
            // Linux (Debian/Ubuntu の Noto CJK)
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJKjp-Regular.otf",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            // Linux (Arch, Fedora の Noto)
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto/NotoSansJP-Regular.otf",
            // Linux (Source Han)
            "/usr/share/fonts/opentype/source-han-sans/SourceHanSans.ttc",
            // Windows
            "C:\\Windows\\Fonts\\YuGothR.ttc",
            "C:\\Windows\\Fonts\\YuGothM.ttc",
            "C:\\Windows\\Fonts\\meiryo.ttc",
            "C:\\Windows\\Fonts\\msgothic.ttc",
        ];
        let mut loaded = false;
        for path in candidates {
            if let Ok(data) = std::fs::read(path) {
                fonts
                    .font_data
                    .insert("jp".to_owned(), egui::FontData::from_owned(data).into());
                if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                    fam.insert(0, "jp".to_owned());
                }
                if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                    fam.push("jp".to_owned());
                }
                loaded = true;
                break;
            }
        }
        if !loaded {
            tracing::warn!(
                "Japanese font not found in standard paths; CJK characters may render as boxes"
            );
        }
        ctx.set_fonts(fonts);
    }
}

async fn get_or_init_store(
    cell: &Arc<OnceCell<Arc<LanceStore>>>,
) -> Result<Arc<LanceStore>> {
    if let Some(s) = cell.get() {
        return Ok(s.clone());
    }
    let path = vectordb::default_db_path()?;
    let store = LanceStore::open(&path, LocalEmbedder::DIM).await?;
    let arc = Arc::new(store);
    let _ = cell.set(arc.clone());
    Ok(arc)
}

async fn get_or_init_embedder(
    cell: &Arc<OnceCell<Arc<LocalEmbedder>>>,
) -> Result<Arc<LocalEmbedder>> {
    if let Some(e) = cell.get() {
        return Ok(e.clone());
    }
    let e = tokio::task::spawn_blocking(LocalEmbedder::new).await??;
    let arc = Arc::new(e);
    let _ = cell.set(arc.clone());
    Ok(arc)
}

impl eframe::App for JiraAiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_sync_events();
        self.drain_chat_events();
        self.drain_browse_events();
        self.drain_review_events();

        if self.sync_rx.is_some()
            || self.chat_busy
            || self.browse.busy
            || self.review.busy
            || self.review.pending_auto > 0
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("JiraAI");
                ui.separator();
                ui.selectable_value(&mut self.active_tab, Tab::Chat, "チャット");
                ui.selectable_value(&mut self.active_tab, Tab::Sync, "同期");
                ui.selectable_value(&mut self.active_tab, Tab::Browse, "ブラウザ");
                let review_label = if self.review.auto_results.is_empty() {
                    "レビュー".to_owned()
                } else {
                    format!("レビュー ({})", self.review.auto_results.len())
                };
                ui.selectable_value(&mut self.active_tab, Tab::Review, review_label);
                ui.selectable_value(&mut self.active_tab, Tab::Settings, "設定");
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let st = self.sync_status.lock().unwrap();
                ui.small(format!(
                    "Jira: {}    Confluence: {}",
                    st.jira_last_synced.as_deref().unwrap_or("未同期"),
                    st.confluence_last_synced.as_deref().unwrap_or("未同期"),
                ));
                if st.busy {
                    ui.spinner();
                    ui.small("同期中...");
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.active_tab {
            Tab::Chat => self.show_chat(ui),
            Tab::Sync => self.show_sync(ui),
            Tab::Browse => self.show_browse(ui),
            Tab::Review => self.show_review(ui),
            Tab::Settings => self.show_settings(ui),
        });
    }
}

impl JiraAiApp {
    fn drain_sync_events(&mut self) {
        let mut finished = false;
        let mut to_review: Vec<NewPageInfo> = vec![];
        if let Some(rx) = self.sync_rx.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                let mut st = self.sync_status.lock().unwrap();
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
                        st.log.push(format!("新規 {} 件を自動レビュー候補に", pages.len()));
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
            if let Err(e) = self.config.save() {
                tracing::warn!("config save after sync failed: {e:?}");
            }
        }
    }

    fn drain_review_events(&mut self) {
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
                        self.review.pending_auto =
                            self.review.pending_auto.saturating_sub(1);
                        changed = true;
                    }
                    ReviewEvent::Failed(msg) => {
                        tracing::error!("review failed: {msg}");
                        self.review.busy = false;
                        self.review.pending_auto =
                            self.review.pending_auto.saturating_sub(1);
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

    fn drain_browse_events(&mut self) {
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

    fn show_browse(&mut self, ui: &mut egui::Ui) {
        ui.heading("ブラウザ");
        ui.separator();

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
                        .selectable_value(&mut self.browse.filter_source_type, "".to_owned(), "すべて")
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

    fn drain_chat_events(&mut self) {
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

    fn show_chat(&mut self, ui: &mut egui::Ui) {
        ui.heading("チャット");
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - 90.0)
            .show(ui, |ui| {
                for msg in &self.chat_history {
                    render_message(ui, msg);
                    ui.add_space(8.0);
                }
                if self.chat_busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("考え中...");
                    });
                }
            });

        ui.separator();
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.chat_input)
                    .desired_width(ui.available_width() - 80.0)
                    .hint_text("Jira / Confluence に質問..."),
            );
            let send = ui.add_enabled(!self.chat_busy, egui::Button::new("送信"));
            let submitted =
                send.clicked() || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
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
    }

    fn show_sync(&mut self, ui: &mut egui::Ui) {
        ui.heading("同期");
        ui.separator();

        let busy = self.sync_status.lock().unwrap().busy;
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("全件同期")).clicked() {
                self.kick_sync(None);
            }
            let since = self.config.sync_state.jira_last_synced.clone();
            let label = if since.is_some() { "増分同期" } else { "増分同期 (未同期)" };
            if ui
                .add_enabled(!busy && since.is_some(), egui::Button::new(label))
                .clicked()
            {
                self.kick_sync(since);
            }
        });

        ui.add_space(8.0);
        ui.label("ログ:");
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let st = self.sync_status.lock().unwrap();
                for line in st.log.iter().rev().take(200) {
                    ui.monospace(line);
                }
            });
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("設定");
        ui.separator();

        ui.label("Atlassian Cloud");
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
                    .add(egui::TextEdit::singleline(&mut self.config.atlassian.api_token).password(true))
                    .changed()
                {
                    self.config_dirty = true;
                }
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.label("LLM プロバイダ");
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
                if ui.text_edit_singleline(&mut self.config.llm.model).changed() {
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
        ui.label("取り込みオプション");
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
            if ui.button("保存").clicked() {
                match self.config.save() {
                    Ok(_) => {
                        self.config_dirty = false;
                    }
                    Err(e) => {
                        tracing::error!("config save failed: {e:?}");
                    }
                }
            }
            if self.config_dirty {
                ui.colored_label(egui::Color32::YELLOW, "未保存の変更があります");
            }
        });
    }

    fn kick_sync(&mut self, since: Option<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.sync_rx = Some(rx);
        {
            let mut st = self.sync_status.lock().unwrap();
            st.busy = true;
            st.log.push(if since.is_some() {
                "増分同期を開始".to_owned()
            } else {
                "全件同期を開始".to_owned()
            });
        }

        let cfg = self.config.atlassian.clone();
        let options = self.config.ingest.clone();
        // Vision は Gemini プロバイダ時のみ API キーを流用
        let gemini_key = if matches!(self.config.llm.kind, LlmProviderKind::Gemini) {
            self.config.llm.api_key.clone()
        } else {
            String::new()
        };
        let store_cell = self.store.clone();
        let embedder_cell = self.embedder.clone();
        let tx_done = tx.clone();
        self.rt.spawn(async move {
            let store = match get_or_init_store(&store_cell).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx_done.send(SyncEvent::Failed(format!("store init: {e:#}")));
                    let _ = tx_done.send(SyncEvent::Finished);
                    return;
                }
            };
            let _ = tx.send(SyncEvent::Log("Embedder を準備中 (初回はモデルDLあり)".into()));
            let embedder = match get_or_init_embedder(&embedder_cell).await {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx_done.send(SyncEvent::Failed(format!("embedder init: {e:#}")));
                    let _ = tx_done.send(SyncEvent::Finished);
                    return;
                }
            };
            let client = AtlassianClient::new(cfg);
            // Vision クライアントは Gemini API キーが設定されていて、かつトグルが ON の場合のみ
            let vision = if options.enable_vision && !gemini_key.is_empty() {
                Some(VisionClient::new(gemini_key.clone(), options.vision_model.clone()))
            } else {
                None
            };
            let result = ingest::run_sync(
                &client,
                embedder.as_ref(),
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

    fn show_review(&mut self, ui: &mut egui::Ui) {
        ui.heading("レビュー");
        ui.separator();

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
                    egui::Button::new("レビュー実行"),
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
            ui.label(format!("自動レビュー履歴: {} 件", self.review.auto_results.len()));
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
        let embedder_cell = self.embedder.clone();
        let llm_cfg = self.config.llm.clone();
        self.rt.spawn(async move {
            let store = match get_or_init_store(&store_cell).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(ReviewEvent::Failed(format!("store init: {e:#}")));
                    return;
                }
            };
            let embedder = match get_or_init_embedder(&embedder_cell).await {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx.send(ReviewEvent::Failed(format!("embedder init: {e:#}")));
                    return;
                }
            };
            let llm = LlmActive::from_config(&llm_cfg);
            match review_document(
                &title,
                "manual",
                "",
                &text,
                embedder.as_ref(),
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

    fn kick_auto_reviews(&mut self, pages: Vec<NewPageInfo>) {
        let tx = match self.review.rx.as_ref() {
            Some(_) => {
                // 既存 rx を使う - 既存タスク用の sender はもうない場合があるので新規作成
                let (tx, rx) = mpsc::unbounded_channel();
                self.review.rx = Some(rx);
                tx
            }
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                self.review.rx = Some(rx);
                tx
            }
        };
        self.review.pending_auto += pages.len();
        let store_cell = self.store.clone();
        let embedder_cell = self.embedder.clone();
        let llm_cfg = self.config.llm.clone();
        for page in pages {
            let tx = tx.clone();
            let store_cell = store_cell.clone();
            let embedder_cell = embedder_cell.clone();
            let llm_cfg = llm_cfg.clone();
            self.rt.spawn(async move {
                let store = match get_or_init_store(&store_cell).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(ReviewEvent::Failed(format!("auto store init: {e:#}")));
                        return;
                    }
                };
                let embedder = match get_or_init_embedder(&embedder_cell).await {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = tx.send(ReviewEvent::Failed(format!("auto embedder init: {e:#}")));
                        return;
                    }
                };
                let llm = LlmActive::from_config(&llm_cfg);
                match review_document(
                    &page.title,
                    &page.id,
                    &page.url,
                    &page.text,
                    embedder.as_ref(),
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

    fn kick_chat(&mut self, question: String, history: Vec<ChatMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.chat_rx = Some(rx);
        self.chat_busy = true;

        let store_cell = self.store.clone();
        let embedder_cell = self.embedder.clone();
        let llm_cfg = self.config.llm.clone();
        self.rt.spawn(async move {
            let store = match get_or_init_store(&store_cell).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(ChatEvent::Failed(format!("store init: {e:#}")));
                    return;
                }
            };
            let embedder = match get_or_init_embedder(&embedder_cell).await {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx.send(ChatEvent::Failed(format!("embedder init: {e:#}")));
                    return;
                }
            };
            let llm = LlmActive::from_config(&llm_cfg);
            match ask_rag(
                embedder.as_ref(),
                store.as_ref(),
                &llm,
                &llm_cfg.model,
                5,
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

fn render_message(ui: &mut egui::Ui, msg: &ChatMessage) {
    let (label, color) = match msg.role {
        Role::User => ("あなた", egui::Color32::from_rgb(80, 140, 220)),
        Role::Assistant => ("AI", egui::Color32::from_rgb(120, 200, 140)),
    };
    ui.colored_label(color, label);
    ui.label(&msg.content);
    if !msg.citations.is_empty() {
        ui.add_space(4.0);
        ui.label("参照:");
        for (i, c) in msg.citations.iter().enumerate() {
            render_citation(ui, i + 1, c);
        }
    }
}

fn render_citation(ui: &mut egui::Ui, idx: usize, c: &Citation) {
    ui.horizontal(|ui| {
        ui.small(format!("[{idx}]"));
        ui.hyperlink_to(&c.title, &c.url);
        ui.small(format!("({})", c.source_type));
    });
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
        ui.colored_label(egui::Color32::from_rgb(220, 100, 100), format!("⚠️ 矛盾 ({}件)", r.contradictions.len()));
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
        ui.colored_label(egui::Color32::from_rgb(220, 180, 80), format!("🔁 重複 ({}件)", r.duplicates.len()));
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
        ui.colored_label(egui::Color32::from_rgb(100, 150, 220), format!("❓ 欠落 ({}件)", r.gaps.len()));
        for g in &r.gaps {
            ui.label(format!("• {g}"));
        }
        ui.separator();
    }
    if !r.terminology.is_empty() {
        ui.colored_label(egui::Color32::from_rgb(180, 180, 180), format!("🏷️ 用語不整合 ({}件)", r.terminology.len()));
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

/// レビュー履歴の永続化先
fn review_history_path() -> anyhow::Result<std::path::PathBuf> {
    let dir = crate::config::data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("reviews.json"))
}

fn load_review_history() -> anyhow::Result<Vec<ReviewResult>> {
    let path = review_history_path()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let raw = std::fs::read_to_string(&path)?;
    let v: Vec<ReviewResult> = serde_json::from_str(&raw).unwrap_or_default();
    Ok(v)
}

fn save_review_history(results: &[ReviewResult]) -> anyhow::Result<()> {
    let path = review_history_path()?;
    let raw = serde_json::to_string_pretty(results)?;
    std::fs::write(&path, raw)?;
    Ok(())
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
