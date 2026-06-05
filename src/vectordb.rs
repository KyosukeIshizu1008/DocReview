use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, ListArray, RecordBatch,
    RecordBatchIterator, StringArray,
};
use arrow_array::builder::{ListBuilder, StringBuilder};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::Connection;
use lancedb::index::Index;
use lancedb::index::vector::IvfPqIndexBuilder;
use lancedb::query::{ExecutableQuery, QueryBase};
use serde::{Deserialize, Serialize};

/// LanceDB に入れるレコード
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub source_type: String,   // "jira" | "confluence"
    pub source_id: String,     // issue key / page id
    pub title: String,
    pub url: String,
    pub space_or_project: String,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub labels: Vec<String>,
    pub text: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk: Chunk,
    pub score: f32,
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    pub source_type: Option<String>,
    pub project_or_space: Option<String>,
}

#[allow(async_fn_in_trait)]
pub trait VectorStore: Send + Sync {
    async fn upsert(&self, chunks: Vec<Chunk>) -> Result<()>;
    async fn search(
        &self,
        query_vec: &[f32],
        top_k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>>;
}

const TABLE_NAME: &str = "chunks";

pub struct LanceStore {
    conn: Connection,
    dim: usize,
    schema: SchemaRef,
}

impl LanceStore {
    pub async fn open(path: &Path, dim: usize) -> Result<Self> {
        std::fs::create_dir_all(path).ok();
        let conn = lancedb::connect(path.to_string_lossy().as_ref())
            .execute()
            .await
            .context("lancedb connect")?;
        let schema = Arc::new(build_schema(dim));

        let names = conn.table_names().execute().await?;
        if !names.iter().any(|n| n == TABLE_NAME) {
            conn.create_empty_table(TABLE_NAME, schema.clone())
                .execute()
                .await
                .context("create_empty_table")?;
        }
        Ok(Self { conn, dim, schema })
    }

    async fn table(&self) -> Result<lancedb::Table> {
        Ok(self.conn.open_table(TABLE_NAME).execute().await?)
    }

    /// 全体行数
    pub async fn count_all(&self) -> Result<usize> {
        let table = self.table().await?;
        let n = table.count_rows(None).await?;
        Ok(n)
    }

    /// source_type ごとの行数
    pub async fn count_by_source(&self, source_type: &str) -> Result<usize> {
        let table = self.table().await?;
        let filter = format!("source_type = '{}'", escape_sql(source_type));
        let n = table.count_rows(Some(filter)).await?;
        Ok(n)
    }

    /// フィルタに合致するチャンクを最大 `limit` 件返す（ベクター検索なし、メタデータ列挙のみ）
    pub async fn list(&self, filter: &SearchFilter, limit: usize) -> Result<Vec<Chunk>> {
        use lancedb::query::ExecutableQuery;
        let table = self.table().await?;
        let mut q = table.query().limit(limit);
        if let Some(expr) = build_filter_expr(filter) {
            q = q.only_if(expr);
        }
        let stream = q.execute().await?;
        let batches: Vec<RecordBatch> = stream.try_collect().await?;
        let mut out = Vec::new();
        for b in batches {
            let hits = batch_to_hits(&b)?;
            out.extend(hits.into_iter().map(|h| h.chunk));
        }
        Ok(out)
    }

    /// 指定 id のチャンクを1件取得
    pub async fn get_by_id(&self, id: &str) -> Result<Option<Chunk>> {
        use lancedb::query::ExecutableQuery;
        let table = self.table().await?;
        let q = table
            .query()
            .only_if(format!("id = '{}'", escape_sql(id)))
            .limit(1);
        let stream = q.execute().await?;
        let batches: Vec<RecordBatch> = stream.try_collect().await?;
        for b in batches {
            let hits = batch_to_hits(&b)?;
            if let Some(h) = hits.into_iter().next() {
                return Ok(Some(h.chunk));
            }
        }
        Ok(None)
    }

    /// 行数が `min_rows` 以上なら IVF_PQ index を作成（既存ならスキップ）。
    /// 数百件未満では brute force のほうが速いので min_rows のしきい値を使う。
    pub async fn ensure_vector_index(&self, min_rows: usize) -> Result<()> {
        let table = self.table().await?;
        let count = table.count_rows(None).await.unwrap_or(0);
        if count < min_rows {
            return Ok(());
        }
        let res = table
            .create_index(&["vector"], Index::IvfPq(IvfPqIndexBuilder::default()))
            .execute()
            .await;
        if let Err(e) = res {
            tracing::warn!("create_index skipped: {e}");
        }
        Ok(())
    }
}

impl VectorStore for LanceStore {
    async fn upsert(&self, chunks: Vec<Chunk>) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let batch = chunks_to_batch(self.schema.clone(), self.dim, &chunks)?;
        let table = self.table().await?;
        let it = RecordBatchIterator::new(vec![Ok(batch)].into_iter(), self.schema.clone());
        let mut mi = table.merge_insert(&["id"]);
        mi.when_matched_update_all(None);
        mi.when_not_matched_insert_all();
        mi.execute(Box::new(it)).await.context("merge_insert")?;
        Ok(())
    }

    async fn search(
        &self,
        query_vec: &[f32],
        top_k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let table = self.table().await?;
        let mut q = table
            .vector_search(query_vec.to_vec())
            .context("vector_search")?
            .limit(top_k);
        if let Some(expr) = build_filter_expr(filter) {
            q = q.only_if(expr);
        }
        let stream = q.execute().await?;
        let batches: Vec<RecordBatch> = stream.try_collect().await?;

        let mut hits = vec![];
        for batch in batches {
            hits.extend(batch_to_hits(&batch)?);
        }
        Ok(hits)
    }
}

fn build_filter_expr(filter: &SearchFilter) -> Option<String> {
    let mut clauses: Vec<String> = vec![];
    if let Some(s) = &filter.source_type {
        clauses.push(format!("source_type = '{}'", escape_sql(s)));
    }
    if let Some(s) = &filter.project_or_space {
        clauses.push(format!("space_or_project = '{}'", escape_sql(s)));
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    }
}

/// SQL リテラル用のサニタイズ。
/// シングルクォートのダブリング + バックスラッシュエスケープ + 制御文字除去。
/// 入力は LanceDB の DataFusion SQL 風 expr に挿入される。
fn escape_sql(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' | '\r' | '\n' | '\x1a' => {
                // 制御文字は丸ごと落とす（SQL 終端攻撃対策）
            }
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                // その他の制御文字も除去
            }
            c => out.push(c),
        }
    }
    out
}

fn build_schema(dim: usize) -> Schema {
    let vector_field = Field::new("item", DataType::Float32, true);
    let labels_field = Field::new("item", DataType::Utf8, true);
    Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("source_type", DataType::Utf8, false),
        Field::new("source_id", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("url", DataType::Utf8, false),
        Field::new("space_or_project", DataType::Utf8, false),
        Field::new("author", DataType::Utf8, true),
        Field::new("created_at", DataType::Int64, false),
        Field::new("updated_at", DataType::Int64, false),
        Field::new("labels", DataType::List(Arc::new(labels_field)), true),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(vector_field), dim as i32),
            false,
        ),
    ])
}

fn chunks_to_batch(schema: SchemaRef, dim: usize, chunks: &[Chunk]) -> Result<RecordBatch> {
    let n = chunks.len();
    let id = StringArray::from_iter_values(chunks.iter().map(|c| c.id.as_str()));
    let source_type = StringArray::from_iter_values(chunks.iter().map(|c| c.source_type.as_str()));
    let source_id = StringArray::from_iter_values(chunks.iter().map(|c| c.source_id.as_str()));
    let title = StringArray::from_iter_values(chunks.iter().map(|c| c.title.as_str()));
    let url = StringArray::from_iter_values(chunks.iter().map(|c| c.url.as_str()));
    let space_or_project =
        StringArray::from_iter_values(chunks.iter().map(|c| c.space_or_project.as_str()));
    let author = StringArray::from(
        chunks
            .iter()
            .map(|c| Some(c.author.clone()))
            .collect::<Vec<_>>(),
    );
    let created_at = Int64Array::from_iter_values(chunks.iter().map(|c| c.created_at));
    let updated_at = Int64Array::from_iter_values(chunks.iter().map(|c| c.updated_at));

    // labels: List<Utf8>
    let mut labels_builder = ListBuilder::new(StringBuilder::new());
    for c in chunks {
        for l in &c.labels {
            labels_builder.values().append_value(l);
        }
        labels_builder.append(true);
    }
    let labels = labels_builder.finish();

    // vector: FixedSizeList<Float32, dim>
    let mut flat: Vec<f32> = Vec::with_capacity(n * dim);
    for c in chunks {
        if c.vector.len() != dim {
            anyhow::bail!(
                "vector dim mismatch: expected {dim}, got {} for id={}",
                c.vector.len(),
                c.id
            );
        }
        flat.extend_from_slice(&c.vector);
    }
    let values = Arc::new(Float32Array::from(flat));
    let vector = FixedSizeListArray::try_new(
        Arc::new(Field::new("item", DataType::Float32, true)),
        dim as i32,
        values,
        None,
    )
    .context("FixedSizeListArray::try_new")?;

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(id),
            Arc::new(source_type),
            Arc::new(source_id),
            Arc::new(title),
            Arc::new(url),
            Arc::new(space_or_project),
            Arc::new(author),
            Arc::new(created_at),
            Arc::new(updated_at),
            Arc::new(labels),
            Arc::new(StringArray::from_iter_values(
                chunks.iter().map(|c| c.text.as_str()),
            )),
            Arc::new(vector),
        ],
    )
    .context("RecordBatch::try_new")
}

fn batch_to_hits(batch: &RecordBatch) -> Result<Vec<SearchHit>> {
    let get_str = |name: &str| -> Result<&StringArray> {
        batch
            .column_by_name(name)
            .with_context(|| format!("missing col {name}"))?
            .as_any()
            .downcast_ref::<StringArray>()
            .with_context(|| format!("col {name} not Utf8"))
    };
    let get_i64 = |name: &str| -> Result<&Int64Array> {
        batch
            .column_by_name(name)
            .with_context(|| format!("missing col {name}"))?
            .as_any()
            .downcast_ref::<Int64Array>()
            .with_context(|| format!("col {name} not Int64"))
    };

    let id = get_str("id")?;
    let source_type = get_str("source_type")?;
    let source_id = get_str("source_id")?;
    let title = get_str("title")?;
    let url = get_str("url")?;
    let space_or_project = get_str("space_or_project")?;
    let author = batch
        .column_by_name("author")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let created_at = get_i64("created_at")?;
    let updated_at = get_i64("updated_at")?;
    let text = get_str("text")?;

    // _distance column (lance が付与)。score = -distance で sort 用に変換
    let distance = batch
        .column_by_name("_distance")
        .and_then(|c| c.as_any().downcast_ref::<arrow_array::Float32Array>());

    // labels (List<Utf8>): スキップ可能にしておく
    let labels_col = batch
        .column_by_name("labels")
        .and_then(|c| c.as_any().downcast_ref::<ListArray>());

    // vector は検索結果としては必須ではないので空で返す
    let n = batch.num_rows();
    let mut hits = Vec::with_capacity(n);
    for i in 0..n {
        let labels: Vec<String> = match labels_col {
            Some(la) => {
                let arr = la.value(i);
                if let Some(sa) = arr.as_any().downcast_ref::<StringArray>() {
                    (0..sa.len()).map(|j| sa.value(j).to_string()).collect()
                } else {
                    vec![]
                }
            }
            None => vec![],
        };
        let chunk = Chunk {
            id: id.value(i).to_string(),
            source_type: source_type.value(i).to_string(),
            source_id: source_id.value(i).to_string(),
            title: title.value(i).to_string(),
            url: url.value(i).to_string(),
            space_or_project: space_or_project.value(i).to_string(),
            author: author
                .map(|a| if a.is_null(i) { String::new() } else { a.value(i).to_string() })
                .unwrap_or_default(),
            created_at: created_at.value(i),
            updated_at: updated_at.value(i),
            labels,
            text: text.value(i).to_string(),
            vector: vec![],
        };
        let score = match distance {
            Some(d) if !d.is_null(i) => -d.value(i),
            _ => 0.0,
        };
        hits.push(SearchHit { chunk, score });
    }
    Ok(hits)
}

/// 開発初期のインメモリ実装 — テスト用に残す
#[allow(dead_code)]
pub struct InMemoryStore {
    inner: tokio::sync::RwLock<Vec<Chunk>>,
}

#[allow(dead_code)]
impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::RwLock::new(vec![]),
        }
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorStore for InMemoryStore {
    async fn upsert(&self, chunks: Vec<Chunk>) -> Result<()> {
        let mut g = self.inner.write().await;
        for c in chunks {
            if let Some(existing) = g.iter_mut().find(|x| x.id == c.id) {
                *existing = c;
            } else {
                g.push(c);
            }
        }
        Ok(())
    }

    async fn search(
        &self,
        query_vec: &[f32],
        top_k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<SearchHit>> {
        let g = self.inner.read().await;
        let mut scored: Vec<SearchHit> = g
            .iter()
            .filter(|c| match &filter.source_type {
                Some(s) => &c.source_type == s,
                None => true,
            })
            .filter(|c| match &filter.project_or_space {
                Some(s) => &c.space_or_project == s,
                None => true,
            })
            .map(|c| SearchHit {
                score: cosine(&c.vector, query_vec),
                chunk: c.clone(),
            })
            .collect();
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        Ok(scored)
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

pub fn default_db_path() -> Result<PathBuf> {
    let dir = crate::config::data_dir()?.join("lancedb");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_chunk(id: &str, source_type: &str, project: &str, vector: Vec<f32>) -> Chunk {
        Chunk {
            id: id.to_owned(),
            source_type: source_type.to_owned(),
            source_id: id.to_owned(),
            title: format!("title-{id}"),
            url: format!("https://example.com/{id}"),
            space_or_project: project.to_owned(),
            author: String::new(),
            created_at: 0,
            updated_at: 0,
            labels: vec![],
            text: format!("body-{id}"),
            vector,
        }
    }

    #[test]
    fn escape_sql_doubles_single_quotes() {
        assert_eq!(escape_sql("O'Reilly"), "O''Reilly");
        assert_eq!(escape_sql("''"), "''''");
    }

    #[test]
    fn escape_sql_handles_backslash() {
        assert_eq!(escape_sql("a\\b"), "a\\\\b");
    }

    #[test]
    fn escape_sql_strips_control_chars() {
        assert_eq!(escape_sql("a\0b\rc\nd"), "abcd");
        assert_eq!(escape_sql("a\x01b\x02"), "ab");
    }

    #[test]
    fn escape_sql_passthrough_normal() {
        assert_eq!(escape_sql("Hello World 日本語"), "Hello World 日本語");
    }

    #[test]
    fn cosine_identical_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6, "got {sim}");
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_opposite_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 1.0];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    #[tokio::test]
    async fn inmem_upsert_and_search_with_filter() {
        let store = InMemoryStore::new();
        let chunks = vec![
            mk_chunk("j1", "jira", "PROJ", vec![1.0, 0.0]),
            mk_chunk("c1", "confluence", "DOCS", vec![0.0, 1.0]),
            mk_chunk("j2", "jira", "PROJ", vec![0.9, 0.1]),
        ];
        store.upsert(chunks).await.unwrap();

        let hits = store
            .search(
                &[1.0, 0.0],
                10,
                &SearchFilter {
                    source_type: Some("jira".into()),
                    project_or_space: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.chunk.source_type == "jira"));
        assert_eq!(hits[0].chunk.id, "j1", "best match should be j1");
    }

    #[tokio::test]
    async fn inmem_upsert_overwrites_by_id() {
        let store = InMemoryStore::new();
        store
            .upsert(vec![mk_chunk("x", "jira", "P", vec![1.0, 0.0])])
            .await
            .unwrap();
        store
            .upsert(vec![mk_chunk("x", "jira", "P", vec![0.0, 1.0])])
            .await
            .unwrap();
        let hits = store
            .search(&[0.0, 1.0], 5, &SearchFilter::default())
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn lance_store_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LanceStore::open(tmp.path(), 4).await.expect("open lance store");
        let chunks = vec![
            mk_chunk("a", "jira", "PROJ", vec![1.0, 0.0, 0.0, 0.0]),
            mk_chunk("b", "confluence", "DOCS", vec![0.0, 1.0, 0.0, 0.0]),
            mk_chunk("c", "jira", "PROJ", vec![0.9, 0.1, 0.0, 0.0]),
        ];
        store.upsert(chunks.clone()).await.expect("upsert");

        // 同じ id で更新（merge_insert idempotency）
        store.upsert(chunks).await.expect("second upsert");

        let hits = store
            .search(&[1.0, 0.0, 0.0, 0.0], 10, &SearchFilter::default())
            .await
            .expect("search");
        let ids: Vec<&str> = hits.iter().map(|h| h.chunk.id.as_str()).collect();
        assert!(ids.contains(&"a"), "expected 'a' in hits: {ids:?}");
        // top hit should be 'a' (exact match)
        assert_eq!(hits[0].chunk.id, "a", "top hit mismatch: {ids:?}");
    }

    #[tokio::test]
    async fn lance_store_filter_by_source_type() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LanceStore::open(tmp.path(), 4).await.unwrap();
        store
            .upsert(vec![
                mk_chunk("j", "jira", "P", vec![1.0, 0.0, 0.0, 0.0]),
                mk_chunk("c", "confluence", "P", vec![1.0, 0.0, 0.0, 0.0]),
            ])
            .await
            .unwrap();
        let hits = store
            .search(
                &[1.0, 0.0, 0.0, 0.0],
                10,
                &SearchFilter {
                    source_type: Some("jira".into()),
                    project_or_space: None,
                },
            )
            .await
            .unwrap();
        assert!(hits.iter().all(|h| h.chunk.source_type == "jira"));
        assert_eq!(hits.len(), 1);
    }
}
