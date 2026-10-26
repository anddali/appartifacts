//! Shared state for artifact pages: JSON documents in named collections, scoped per artifact,
//! with change events fanned out to every server instance through Postgres LISTEN/NOTIFY.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use uuid::Uuid;

use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::util::random_id;

const CHANNEL: &str = "apphost_data";
pub const MAX_DOC_BYTES: usize = 256 * 1024;
const MAX_DOCS_PER_ARTIFACT: i64 = 100_000;

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Doc {
    pub id: String,
    pub collection: String,
    pub data: sqlx::types::Json<Value>,
    pub version: i64,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const DOC_SELECT: &str = "SELECT d.doc_id AS id, d.collection, d.data, d.version,
        cu.email AS created_by, uu.email AS updated_by, d.created_at, d.updated_at
    FROM app_data d LEFT JOIN users cu ON cu.id = d.created_by LEFT JOIN users uu ON uu.id = d.updated_by";

/// A change, delivered to pages over server-sent events.
#[derive(Clone, Debug, Serialize)]
pub struct DataEvent {
    #[serde(skip)]
    pub artifact_id: String,
    /// "upsert", "delete", "clear" (whole collection) or "resync" (refetch everything).
    pub op: String,
    pub collection: Option<String>,
    pub id: Option<String>,
    pub doc: Option<Doc>,
}

#[derive(Serialize, Deserialize)]
struct Notice {
    a: String,
    c: String,
    d: Option<String>,
    op: String,
}

pub fn validate_collection(c: &str) -> AppResult<()> {
    let ok = !c.is_empty() && c.len() <= 64 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok { Ok(()) } else { Err(AppError::bad_request("Collection names are 1-64 characters: letters, digits, _ and -")) }
}

pub fn validate_doc_id(d: &str) -> AppResult<()> {
    let ok = !d.is_empty()
        && d.len() <= 128
        && d.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':' | b'@'));
    if ok { Ok(()) } else { Err(AppError::bad_request("Document ids are 1-128 characters: letters, digits and _ - . : @")) }
}

fn check_size(data: &Value) -> AppResult<()> {
    if serde_json::to_vec(data).map(|v| v.len()).unwrap_or(usize::MAX) > MAX_DOC_BYTES {
        return Err(AppError::too_large(format!("Documents are limited to {} KB", MAX_DOC_BYTES / 1024)));
    }
    Ok(())
}

async fn notify(app: &AppState, artifact_id: &str, collection: &str, doc_id: Option<&str>, op: &str) -> AppResult<()> {
    let payload = serde_json::to_string(&Notice { a: artifact_id.into(), c: collection.into(), d: doc_id.map(Into::into), op: op.into() })
        .expect("serializable");
    sqlx::query("SELECT pg_notify($1, $2)").bind(CHANNEL).bind(payload).execute(&app.db).await?;
    Ok(())
}

pub async fn list(app: &AppState, artifact_id: &str, collection: &str, limit: i64, offset: i64) -> AppResult<Vec<Doc>> {
    validate_collection(collection)?;
    Ok(sqlx::query_as::<_, Doc>(&format!(
        "{DOC_SELECT} WHERE d.artifact_id = $1 AND d.collection = $2 ORDER BY d.created_at, d.doc_id LIMIT $3 OFFSET $4"
    ))
    .bind(artifact_id)
    .bind(collection)
    .bind(limit.clamp(1, 1000))
    .bind(offset.max(0))
    .fetch_all(&app.db)
    .await?)
}

pub async fn get(app: &AppState, artifact_id: &str, collection: &str, doc_id: &str) -> AppResult<Option<Doc>> {
    validate_collection(collection)?;
    validate_doc_id(doc_id)?;
    Ok(sqlx::query_as::<_, Doc>(&format!("{DOC_SELECT} WHERE d.artifact_id = $1 AND d.collection = $2 AND d.doc_id = $3"))
        .bind(artifact_id)
        .bind(collection)
        .bind(doc_id)
        .fetch_optional(&app.db)
        .await?)
}

pub struct WriteOpts {
    /// Shallow-merge `data` into the existing document instead of replacing it.
    pub merge: bool,
    /// Optimistic concurrency: only write if the stored version matches (0 = must not exist).
    pub expected_version: Option<i64>,
}

/// Creates or updates a document. `doc_id` None generates a new id.
pub async fn write(
    app: &AppState,
    artifact_id: &str,
    collection: &str,
    doc_id: Option<&str>,
    data: Value,
    user_id: Uuid,
    opts: WriteOpts,
) -> AppResult<Doc> {
    validate_collection(collection)?;
    let doc_id = match doc_id {
        Some(d) => {
            validate_doc_id(d)?;
            d.to_string()
        }
        None => random_id(16),
    };
    check_size(&data)?;
    if opts.merge && !data.is_object() {
        return Err(AppError::bad_request("A merge update needs a JSON object"));
    }
    if opts.merge {
        if let Some(existing) = get(app, artifact_id, collection, &doc_id).await? {
            if let (Value::Object(mut merged), Value::Object(patch)) = (existing.data.0, &data) {
                merged.extend(patch.clone());
                check_size(&Value::Object(merged))?;
            }
        }
    }

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM app_data WHERE artifact_id = $1").bind(artifact_id).fetch_one(&app.db).await?;
    if count >= MAX_DOCS_PER_ARTIFACT {
        return Err(AppError::too_large(format!("This artifact has reached {MAX_DOCS_PER_ARTIFACT} documents")));
    }

    let written: Option<String> = match opts.expected_version {
        Some(0) => {
            sqlx::query_scalar(
                "INSERT INTO app_data (artifact_id, collection, doc_id, data, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $5)
                 ON CONFLICT DO NOTHING RETURNING doc_id",
            )
            .bind(artifact_id)
            .bind(collection)
            .bind(&doc_id)
            .bind(sqlx::types::Json(&data))
            .bind(user_id)
            .fetch_optional(&app.db)
            .await?
        }
        Some(v) => {
            sqlx::query_scalar(
                "UPDATE app_data SET data = CASE WHEN $6 AND jsonb_typeof(data) = 'object' THEN data || $4 ELSE $4 END,
                        version = version + 1, updated_by = $5, updated_at = now()
                 WHERE artifact_id = $1 AND collection = $2 AND doc_id = $3 AND version = $7 RETURNING doc_id",
            )
            .bind(artifact_id)
            .bind(collection)
            .bind(&doc_id)
            .bind(sqlx::types::Json(&data))
            .bind(user_id)
            .bind(opts.merge)
            .bind(v)
            .fetch_optional(&app.db)
            .await?
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO app_data (artifact_id, collection, doc_id, data, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $5)
                 ON CONFLICT (artifact_id, collection, doc_id) DO UPDATE SET
                    data = CASE WHEN $6 AND jsonb_typeof(app_data.data) = 'object' THEN app_data.data || EXCLUDED.data ELSE EXCLUDED.data END,
                    version = app_data.version + 1, updated_by = EXCLUDED.updated_by, updated_at = now()
                 RETURNING doc_id",
            )
            .bind(artifact_id)
            .bind(collection)
            .bind(&doc_id)
            .bind(sqlx::types::Json(&data))
            .bind(user_id)
            .bind(opts.merge)
            .fetch_optional(&app.db)
            .await?
        }
    };
    if written.is_none() {
        return Err(AppError::conflict("The document was changed by someone else (version mismatch)"));
    }
    let doc = get(app, artifact_id, collection, &doc_id).await?.ok_or_else(|| AppError::internal("written doc vanished"))?;
    notify(app, artifact_id, collection, Some(&doc_id), "upsert").await?;
    Ok(doc)
}

pub async fn delete(app: &AppState, artifact_id: &str, collection: &str, doc_id: &str) -> AppResult<bool> {
    validate_collection(collection)?;
    validate_doc_id(doc_id)?;
    let r = sqlx::query("DELETE FROM app_data WHERE artifact_id = $1 AND collection = $2 AND doc_id = $3")
        .bind(artifact_id)
        .bind(collection)
        .bind(doc_id)
        .execute(&app.db)
        .await?;
    if r.rows_affected() > 0 {
        notify(app, artifact_id, collection, Some(doc_id), "delete").await?;
    }
    Ok(r.rows_affected() > 0)
}

pub async fn clear(app: &AppState, artifact_id: &str, collection: &str) -> AppResult<u64> {
    validate_collection(collection)?;
    let r = sqlx::query("DELETE FROM app_data WHERE artifact_id = $1 AND collection = $2")
        .bind(artifact_id)
        .bind(collection)
        .execute(&app.db)
        .await?;
    notify(app, artifact_id, collection, None, "clear").await?;
    Ok(r.rows_affected())
}

pub async fn collections(app: &AppState, artifact_id: &str) -> AppResult<Value> {
    let rows: Vec<(String, i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT collection, count(*), max(updated_at) FROM app_data WHERE artifact_id = $1 GROUP BY collection ORDER BY collection",
    )
    .bind(artifact_id)
    .fetch_all(&app.db)
    .await?;
    Ok(Value::Array(
        rows.into_iter().map(|(name, count, updated_at)| json!({ "name": name, "count": count, "updated_at": updated_at })).collect(),
    ))
}

/// Listens for change notifications from every apphost instance and re-broadcasts them in-process.
pub fn spawn_listener(app: AppState) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = listen(&app).await {
                tracing::warn!(error = %e, "data listener failed; retrying");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

async fn listen(app: &AppState) -> anyhow::Result<()> {
    let mut listener = PgListener::connect_with(&app.db).await?;
    listener.listen(CHANNEL).await?;
    // Clients may have missed events while we were disconnected.
    let _ = app.events.send(DataEvent { artifact_id: "*".into(), op: "resync".into(), collection: None, id: None, doc: None });
    loop {
        let Some(n) = listener.try_recv().await? else {
            let _ = app.events.send(DataEvent { artifact_id: "*".into(), op: "resync".into(), collection: None, id: None, doc: None });
            continue;
        };
        let Ok(notice) = serde_json::from_str::<Notice>(n.payload()) else { continue };
        if app.events.receiver_count() == 0 {
            continue;
        }
        let doc = match (&notice.d, notice.op.as_str()) {
            (Some(d), "upsert") => get(app, &notice.a, &notice.c, d).await.ok().flatten(),
            _ => None,
        };
        let _ = app.events.send(DataEvent { artifact_id: notice.a, op: notice.op, collection: Some(notice.c), id: notice.d, doc });
    }
}
