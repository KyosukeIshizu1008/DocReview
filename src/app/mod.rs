use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::Result;
use eframe::egui;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, OnceCell};

use crate::chat::{ChatMessage, Citation};
use crate::config::{AppConfig, LlmProviderKind};
use crate::embedding::{GeminiEmbedder, EMBED_DIM};
use crate::review::ReviewResult;
use crate::vectordb::{self, Chunk, LanceStore};

mod browse;
mod chat;
mod review;
mod settings;
mod sync;
mod theme;

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
    /// 履歴クリック時に該当メッセージへスクロールするための一時フラグ
    scroll_to_msg: Option<usize>,

    sync_status: Arc<Mutex<SyncStatus>>,
    sync_rx: Option<mpsc::UnboundedReceiver<SyncEvent>>,

    // 共有リソース（遅延初期化）。埋め込みは Gemini API のため毎タスクで安価に生成する。
    store: Arc<OnceCell<Arc<LanceStore>>>,

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
        theme::apply(&cc.egui_ctx);

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
            scroll_to_msg: None,
            sync_status,
            sync_rx: None,
            store: Arc::new(OnceCell::new()),
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
                    .insert("jp".to_owned(), egui::FontData::from_owned(data));
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

    /// config をバックグラウンドスレッドで保存する。
    /// keyring への書き込みは同期 I/O のため、UI スレッドを直接ブロックしないよう
    /// tokio の blocking プールへ逃がす。保存成否はログにのみ反映する。
    /// 埋め込みに使う (api_key, model) を解決する。埋め込みは Gemini 固定。
    /// 専用キーが未設定でも、LLM が Gemini ならそのキーを流用する。
    fn embedding_params(&self) -> (String, String) {
        let key = if !self.config.embedding.api_key.is_empty() {
            self.config.embedding.api_key.clone()
        } else if matches!(self.config.llm.kind, LlmProviderKind::Gemini) {
            self.config.llm.api_key.clone()
        } else {
            String::new()
        };
        (key, self.config.embedding.model.clone())
    }

    fn save_config_async(&self) {
        let cfg = self.config.clone();
        self.rt.spawn_blocking(move || {
            if let Err(e) = cfg.save() {
                tracing::error!("config save failed: {e:?}");
            }
        });
    }
}

/// SyncStatus の Mutex を poison しても回復してロックを取得する。
/// 推論モデルと違い SyncStatus はただのデータなので、poison しても継続して問題ない。
fn lock_status(m: &Mutex<SyncStatus>) -> MutexGuard<'_, SyncStatus> {
    match m.lock() {
        Ok(g) => g,
        Err(poison) => {
            tracing::warn!("sync_status mutex poisoned; recovering");
            poison.into_inner()
        }
    }
}

async fn get_or_init_store(cell: &Arc<OnceCell<Arc<LanceStore>>>) -> Result<Arc<LanceStore>> {
    if let Some(s) = cell.get() {
        return Ok(s.clone());
    }
    let path = vectordb::default_db_path()?;
    let store = LanceStore::open(&path, EMBED_DIM).await?;
    let arc = Arc::new(store);
    let _ = cell.set(arc.clone());
    Ok(arc)
}

/// store を遅延初期化し、Gemini 埋め込みクライアントを生成して返す。
/// 各非同期タスクが共通で必要とするためヘルパーに切り出している。
/// embedder は reqwest クライアント + キーだけの安価な値なので毎回生成する
/// （設定でキーを変えても次のタスクから反映される）。
async fn init_store_embedder(
    store_cell: &Arc<OnceCell<Arc<LanceStore>>>,
    embed_key: String,
    embed_model: String,
) -> Result<(Arc<LanceStore>, GeminiEmbedder)> {
    let store = get_or_init_store(store_cell).await?;
    let embedder = GeminiEmbedder::new(embed_key, embed_model);
    Ok((store, embedder))
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

        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(210.0)
            .frame(theme::sidebar_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    theme::mark(ui, theme::ACCENT);
                    ui.label(
                        egui::RichText::new("Atlas")
                            .heading()
                            .strong()
                            .color(theme::TEXT),
                    );
                });
                ui.add_space(14.0);

                nav_tab(ui, &mut self.active_tab, Tab::Chat, "チャット");
                ui.add_space(3.0);
                nav_tab(ui, &mut self.active_tab, Tab::Sync, "同期");
                ui.add_space(3.0);
                nav_tab(ui, &mut self.active_tab, Tab::Browse, "ブラウズ");
                ui.add_space(3.0);
                let review_label = if self.review.auto_results.is_empty() {
                    "レビュー".to_owned()
                } else {
                    format!("レビュー ({})", self.review.auto_results.len())
                };
                nav_tab(ui, &mut self.active_tab, Tab::Review, &review_label);
                ui.add_space(3.0);
                nav_tab(ui, &mut self.active_tab, Tab::Settings, "設定");

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("履歴")
                        .small()
                        .strong()
                        .color(theme::SIDEBAR_MUTED),
                );
                ui.add_space(4.0);

                // 現在の会話でした質問の一覧。クリックでその箇所へスクロール。
                let questions: Vec<(usize, String)> = self
                    .chat_history
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| matches!(m.role, crate::chat::Role::User))
                    .map(|(i, m)| (i, history_label(&m.content)))
                    .collect();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if questions.is_empty() {
                            ui.label(
                                egui::RichText::new("まだありません")
                                    .small()
                                    .color(theme::MUTED),
                            );
                        }
                        for (idx, label) in questions {
                            let resp = ui.add_sized(
                                [ui.available_width(), 26.0],
                                egui::SelectableLabel::new(
                                    false,
                                    egui::RichText::new(label).small(),
                                ),
                            );
                            if resp.clicked() {
                                self.active_tab = Tab::Chat;
                                self.scroll_to_msg = Some(idx);
                            }
                        }
                    });
            });

        egui::TopBottomPanel::bottom("status")
            .frame(theme::bar_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                let st = lock_status(&self.sync_status);
                status_chip(
                    ui,
                    "Jira",
                    st.jira_last_synced.as_deref().unwrap_or("未同期"),
                    theme::JIRA,
                );
                status_chip(
                    ui,
                    "Confluence",
                    st.confluence_last_synced.as_deref().unwrap_or("未同期"),
                    theme::CONFLUENCE,
                );
                if st.busy {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.small(egui::RichText::new("同期中...").color(theme::MUTED));
                    });
                }
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::BG).inner_margin(egui::Margin::same(18.0)))
            .show(ctx, |ui| match self.active_tab {
                Tab::Chat => self.show_chat(ui),
                Tab::Sync => self.show_sync(ui),
                Tab::Browse => self.show_browse(ui),
                Tab::Review => self.show_review(ui),
                Tab::Settings => self.show_settings(ui),
            });
    }
}

/// サイドバーの縦並びナビ項目（全幅・左寄せ）。
fn nav_tab(ui: &mut egui::Ui, active_tab: &mut Tab, tab: Tab, label: &str) {
    let selected = *active_tab == tab;
    let text = egui::RichText::new(label)
        .strong()
        .color(if selected { theme::TEXT } else { theme::MUTED });
    let fill = if selected {
        theme::SIDEBAR_ACTIVE
    } else {
        theme::SIDEBAR
    };
    let button = egui::Button::new(text)
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, fill))
        .min_size(egui::vec2(0.0, 32.0));
    if ui
        .add_sized([ui.available_width(), 32.0], button)
        .clicked()
    {
        *active_tab = tab;
    }
}

/// 履歴サイドバー用に質問文を1行へ短縮する。
fn history_label(s: &str) -> String {
    let one = s.replace('\n', " ");
    let short: String = one.chars().take(22).collect();
    if one.chars().count() > 22 {
        format!("{short}…")
    } else {
        short
    }
}

fn status_chip(ui: &mut egui::Ui, label: &str, value: &str, accent: egui::Color32) {
    egui::Frame::none()
        .fill(theme::PANEL)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                theme::mark(ui, accent);
                ui.small(egui::RichText::new(label).strong().color(theme::TEXT));
                ui.small(egui::RichText::new(value).color(theme::MUTED));
            });
        });
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
