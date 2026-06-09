//! Codex 風のニュートラルテーマ。色・角丸・余白・タイポを一括設定する。
use eframe::egui::{self, Color32, FontFamily, FontId, Margin, Rounding, Stroke, TextStyle};

// ---- パレット ----
/// ページ背景
pub(super) const BG: Color32 = Color32::from_rgb(0x1B, 0x1B, 0x1B);
/// カード/パネル
pub(super) const PANEL: Color32 = Color32::from_rgb(0x22, 0x22, 0x22);
/// 本文テキスト
pub(super) const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF2);
/// アクセント
pub(super) const ACCENT: Color32 = Color32::from_rgb(0xF4, 0xF4, 0xF4);
/// アクセント淡色（hover 等）
pub(super) const ACCENT_SOFT: Color32 = Color32::from_rgb(0x36, 0x36, 0x36);
/// 補助テキスト
pub(super) const MUTED: Color32 = Color32::from_rgb(0xA3, 0xA3, 0xA3);
/// 境界線
pub(super) const BORDER: Color32 = Color32::from_rgb(0x34, 0x34, 0x34);
/// 通常ボタン等の薄い面
pub(super) const SURFACE: Color32 = Color32::from_rgb(0x2C, 0x2C, 0x2C);
/// ユーザー発言バブル
pub(super) const USER_BUBBLE: Color32 = Color32::from_rgb(0x2D, 0x2D, 0x2D);
/// サイドバー背景
pub(super) const SIDEBAR: Color32 = Color32::from_rgb(0x10, 0x10, 0x10);
/// サイドバー内の補助テキスト
#[allow(dead_code)]
pub(super) const SIDEBAR_MUTED: Color32 = Color32::from_rgb(0xA5, 0xA5, 0xA5);
/// サイドバーの選択面
#[allow(dead_code)]
pub(super) const SIDEBAR_ACTIVE: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x2B);
/// サイドバー境界線
pub(super) const SIDEBAR_BORDER: Color32 = Color32::from_rgb(0x2B, 0x2B, 0x2B);
/// Jira 系アクセント
pub(super) const JIRA: Color32 = Color32::from_rgb(0x00, 0x82, 0xC9);
/// Confluence 系アクセント
pub(super) const CONFLUENCE: Color32 = Color32::from_rgb(0x28, 0xA7, 0x45);
/// 薄い成功色
pub(super) const SUCCESS_SOFT: Color32 = Color32::from_rgb(0x19, 0x3B, 0x2A);

const RADIUS: f32 = 8.0;
const CARD_RADIUS: f32 = 8.0;

/// アプリ全体のスタイルを適用する。
pub(super) fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    // タイポグラフィ
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.0, FontFamily::Monospace),
        ),
    ]
    .into();

    // 余白
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(13.0, 8.0);
    style.spacing.window_margin = Margin::same(16.0);
    style.spacing.menu_margin = Margin::same(8.0);
    style.spacing.interact_size.y = 34.0;

    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(0x18, 0x18, 0x18); // テキスト入力欄の背景
    v.faint_bg_color = SURFACE;
    v.hyperlink_color = Color32::from_rgb(0x8A, 0xB4, 0xF8);
    v.window_rounding = Rounding::same(CARD_RADIUS);
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.menu_rounding = Rounding::same(RADIUS);

    // 選択範囲・選択中タブ
    v.selection.bg_fill = Color32::from_rgb(0x3B, 0x3B, 0x3B);
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    let r = Rounding::same(RADIUS);
    let w = &mut v.widgets;

    // 非インタラクティブ（ラベル/枠）
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.weak_bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.rounding = r;

    // 通常状態（ボタン等）
    w.inactive.bg_fill = SURFACE;
    w.inactive.weak_bg_fill = SURFACE;
    w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.rounding = r;
    w.inactive.expansion = 0.0;

    // hover
    w.hovered.bg_fill = Color32::from_rgb(0x3A, 0x3A, 0x3A);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x3A, 0x3A, 0x3A);
    w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x50, 0x50, 0x50));
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.rounding = r;
    w.hovered.expansion = 1.0;

    // active（押下）
    w.active.bg_fill = Color32::from_rgb(0x44, 0x44, 0x44);
    w.active.weak_bg_fill = Color32::from_rgb(0x44, 0x44, 0x44);
    w.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x5A, 0x5A, 0x5A));
    w.active.fg_stroke = Stroke::new(1.0, TEXT);
    w.active.rounding = r;
    w.active.expansion = 1.0;

    // open（コンボ展開中）
    w.open.bg_fill = SURFACE;
    w.open.weak_bg_fill = SURFACE;
    w.open.bg_stroke = Stroke::new(1.0, ACCENT_SOFT);
    w.open.fg_stroke = Stroke::new(1.0, TEXT);
    w.open.rounding = r;

    ctx.set_style(style);
}

/// 上部タブバー/下部ステータスバー用のフレーム。
pub(super) fn bar_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(PANEL)
        .inner_margin(Margin::symmetric(16.0, 8.0))
        .stroke(Stroke::new(1.0, BORDER))
}

/// サイドバー用フレーム。
#[allow(dead_code)]
pub(super) fn sidebar_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(SIDEBAR)
        .inner_margin(Margin::symmetric(14.0, 16.0))
        .stroke(Stroke::new(1.0, SIDEBAR_BORDER))
}

/// 白いカード枠。
pub(super) fn card() -> egui::Frame {
    egui::Frame::none()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .rounding(Rounding::same(CARD_RADIUS))
        .inner_margin(Margin::symmetric(14.0, 10.0))
}

/// チャット吹き出し用フレーム。
pub(super) fn bubble(is_user: bool) -> egui::Frame {
    let fill = if is_user { USER_BUBBLE } else { PANEL };
    egui::Frame::none()
        .fill(fill)
        .stroke(Stroke::new(1.0, BORDER))
        .rounding(Rounding::same(CARD_RADIUS))
        .inner_margin(Margin::symmetric(14.0, 10.0))
}

/// アクセント塗りのプライマリボタン（白文字）。
pub(super) fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).color(BG).strong())
        .fill(ACCENT)
        .stroke(Stroke::new(1.0, ACCENT))
}

/// セクション見出し（太字 + 細い区切り線）。
pub(super) fn section_heading(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(text).heading().strong().color(TEXT));
    });
    ui.add_space(10.0);
}

/// 小さな丸いチップ（バッジ）。
pub(super) fn chip(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::none()
        .fill(bg)
        .rounding(Rounding::same(6.0))
        .inner_margin(Margin::symmetric(7.0, 2.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().strong().color(fg));
        });
}

/// 画面タイトルの補助文。
pub(super) fn subtitle(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().color(MUTED));
}

/// アイコン代わりの小さな識別マーク。
pub(super) fn mark(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, Rounding::same(2.0), color);
}
