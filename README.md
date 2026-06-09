# Atlas

Jira / Confluence Cloud のナレッジを横断検索・レビューできる RAG デスクトップアプリ。
[egui](https://github.com/emilk/egui) 製の GUI で、埋め込み生成は [Gemini 埋め込み API](https://ai.google.dev/gemini-api/docs/embeddings)、ベクター検索はローカルの [LanceDB](https://lancedb.com/)、回答生成は Claude / Gemini を切り替えて利用します。

## 主な機能

- **同期**: Jira issue と Confluence ページを取得し、チャンク分割 → 埋め込み → LanceDB に格納（全件 / 増分）。増分同期は Jira(JQL `updated`)・Confluence(更新日時順 + `since` 打ち切り)の両方に対応し、`updated_at` が変わっていないチャンクは再埋め込みをスキップ（Gemini 埋め込みコスト削減）
- **チャット**: 蓄積したナレッジに対する RAG 質問応答（引用リンク付き）
- **ブラウザ**: 取り込んだチャンクを種別・キーワードで一覧／確認
- **レビュー**: 新規ドキュメントを既存ナレッジと突き合わせ、矛盾・重複・欠落・用語不整合を LLM が指摘（手動 / 同期後の自動レビュー）
- **マルチモーダル取り込み**: 添付ファイル、draw.io 図、Mermaid 図、画像（Gemini Vision・任意）

## アーキテクチャ

```
egui UI (src/app/)
  │  各タブ = chat / sync / browse / review / settings
  ▼  tokio ランタイムへ非同期タスクを spawn
同期パイプライン (src/ingest.rs)
  Atlassian API (src/atlassian/) → パース/チャンク (src/chunking.rs, src/diagrams/)
  → 埋め込み (src/embedding.rs, Gemini API) → LanceDB (src/vectordb.rs)
チャット / レビュー
  クエリ → 埋め込み (Gemini API) → ベクター検索 → LLM (src/llm/{claude,gemini}.rs)
```

## ビルドと実行

前提: 安定版 Rust ツールチェイン（`rustup`）。

```sh
cargo run --release
```

埋め込みは Gemini の `text-embedding-004`（768 次元）を使うため、ローカルモデルのダウンロードは不要です。その代わり同期・チャット・レビュー時に Gemini への通信が必要で、ドキュメント本文は埋め込みのため Google に送信されます。

> 埋め込みモデルや次元を変更して既存 DB と次元が食い違う場合、起動時に `chunks` テーブルを自動で作り直します（中身は失われるため再同期が必要です）。

TLS インスペクションを行うプロキシ配下では、企業 CA 証明書を OS の証明書ストア（macOS キーチェーン等）に入れておけば動作します（HTTP クライアントは native-tls = システム証明書を使用）。

## 設定

アプリ内の「設定」タブから入力します。

- **Atlassian Cloud**: Site URL（例 `https://yourorg.atlassian.net`）、Email、API Token
- **LLM プロバイダ**: Claude または Gemini、モデル名、API キー
- **埋め込み (Gemini)**: モデル名（既定 `text-embedding-004`）、Gemini API キー（空欄なら LLM が Gemini のときそのキーを流用）
- **取り込みオプション**: 添付取得 / draw.io 解析 / Vision 解析 / 自動レビュー など

API トークン・各種 API キーは設定ファイルには平文保存されず、OS のキーチェーン（keyring）に格納されます。その他の設定は `directories` が解決する OS 標準の設定ディレクトリ配下の `config.json` に保存されます。

## 開発

```sh
cargo fmt --all          # 整形
cargo clippy --all-targets -- -D warnings   # lint
cargo test --all         # テスト
```

CI（GitHub Actions, `.github/workflows/ci.yml`）で fmt / clippy / test を実行します。
