//! Artifacts: publishing versions, access control, sharing. Shared by the portal API and MCP.

use std::collections::BTreeMap;

use axum::body::Bytes;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::User;
use crate::error::{AppError, AppResult};
use crate::util::{ARTIFACT_ID_LEN, is_valid_artifact_id, random_id, sha256_hex};

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Owner,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct Artifact {
    pub id: String,
    pub owner_id: Uuid,
    pub owner_email: String,
    pub owner_name: String,
    pub title: String,
    pub description: String,
    pub visibility: String,
    pub current_version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[sqlx(try_from = "String")]
    pub role: RoleCol,
}

/// Role as returned by SQL ('owner' | 'editor' | 'viewer').
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(transparent)]
pub struct RoleCol(pub Role);

impl TryFrom<String> for RoleCol {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Ok(RoleCol(match s.as_str() {
            "owner" => Role::Owner,
            "editor" => Role::Editor,
            "viewer" => Role::Viewer,
            _ => return Err(format!("bad role {s}")),
        }))
    }
}

impl Artifact {
    pub fn role(&self) -> Role {
        self.role.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub content_type: String,
    pub size: i64,
    pub sha256: String,
}

// $1 user id, $2 lower-case email, $3 user's group ids.
const SHARE_MATCH: &str = "((s.principal_type = 'user' AND s.principal = $2) OR (s.principal_type = 'group' AND s.principal = ANY($3)))";

fn accessible_sql(extra_where: &str, tail: &str) -> String {
    format!(
        "SELECT a.id, a.owner_id, u.email AS owner_email, u.name AS owner_name, a.title, a.description, a.visibility,
                a.current_version, a.created_at, a.updated_at,
                CASE WHEN a.owner_id = $1 THEN 'owner'
                     WHEN EXISTS (SELECT 1 FROM artifact_shares s WHERE s.artifact_id = a.id AND s.role = 'editor' AND {SHARE_MATCH}) THEN 'editor'
                     ELSE 'viewer' END AS role
         FROM artifacts a JOIN users u ON u.id = a.owner_id
         WHERE (a.owner_id = $1 OR a.visibility = 'org'
                OR EXISTS (SELECT 1 FROM artifact_shares s WHERE s.artifact_id = a.id AND {SHARE_MATCH}))
           AND {extra_where}
         {tail}"
    )
}

/// Loads an artifact if `user` can at least view it.
pub async fn get_accessible(app: &AppState, user: &User, id: &str) -> AppResult<Option<Artifact>> {
    if !is_valid_artifact_id(id) {
        return Ok(None);
    }
    Ok(sqlx::query_as::<_, Artifact>(&accessible_sql("a.id = $4", ""))
        .bind(user.id)
        .bind(user.email.to_lowercase())
        .bind(&user.groups)
        .bind(id)
        .fetch_optional(&app.db)
        .await?)
}

/// Like get_accessible but requires at least `min` role.
pub async fn require(app: &AppState, user: &User, id: &str, min: Role) -> AppResult<Artifact> {
    let a = get_accessible(app, user, id).await?.ok_or_else(|| AppError::not_found("Artifact"))?;
    if a.role() < min {
        return Err(AppError::forbidden(format!("This needs {} access to the artifact", format!("{min:?}").to_lowercase())));
    }
    Ok(a)
}

pub async fn list(app: &AppState, user: &User, scope: &str, query: &str, limit: i64) -> AppResult<Vec<Artifact>> {
    let filter = match scope {
        "mine" => "a.owner_id = $1",
        "shared" => "a.owner_id <> $1",
        _ => "TRUE",
    };
    let pattern = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let sql = accessible_sql(
        &format!("{filter} AND ($4 = '%%' OR a.title ILIKE $4 OR a.description ILIKE $4)"),
        "ORDER BY a.updated_at DESC LIMIT $5",
    );
    Ok(sqlx::query_as::<_, Artifact>(&sql)
        .bind(user.id)
        .bind(user.email.to_lowercase())
        .bind(&user.groups)
        .bind(pattern)
        .bind(limit.clamp(1, 500))
        .fetch_all(&app.db)
        .await?)
}

#[derive(Debug, Deserialize)]
pub struct PublishFile {
    pub path: String,
    pub content: Option<String>,
    pub content_base64: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    pub artifact_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub files: Vec<PublishFile>,
    /// Paths to drop from the previous version.
    #[serde(default)]
    pub remove_paths: Vec<String>,
    /// Start from an empty file set instead of the previous version.
    #[serde(default)]
    pub replace_all: bool,
    pub label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PublishResult {
    pub artifact_id: String,
    pub url: String,
    pub version: i32,
    pub created: bool,
    pub files: Vec<FileEntry>,
}

pub fn normalize_path(p: &str) -> AppResult<String> {
    let p = p.trim().trim_start_matches("./").trim_start_matches('/');
    let bad = |why: &str| Err(AppError::bad_request(format!("Invalid file path '{p}': {why}")));
    if p.is_empty() || p.len() > 512 {
        return bad("must be 1-512 characters");
    }
    if p.contains('\\') || p.chars().any(|c| c.is_control()) || p.contains(['?', '#']) {
        return bad("contains a backslash, '?', '#' or control character");
    }
    if p.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return bad("empty, '.' or '..' segment");
    }
    if p == "__apphost" || p.starts_with("__apphost/") {
        return bad("the __apphost/ prefix is reserved");
    }
    Ok(p.to_string())
}

pub fn content_type_for(path: &str, given: Option<&str>) -> String {
    let ct = given
        .map(str::to_string)
        .unwrap_or_else(|| mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string());
    if (ct.starts_with("text/") || ct == "application/javascript" || ct == "application/json" || ct == "image/svg+xml")
        && !ct.contains("charset")
    {
        format!("{ct}; charset=utf-8")
    } else {
        ct
    }
}

pub fn blob_key(artifact_id: &str, sha: &str) -> String {
    format!("artifacts/{artifact_id}/blobs/{sha}")
}

pub async fn version_files(app: &AppState, artifact_id: &str, version: i32) -> AppResult<Vec<FileEntry>> {
    let files: Option<sqlx::types::Json<Vec<FileEntry>>> =
        sqlx::query_scalar("SELECT files FROM artifact_versions WHERE artifact_id = $1 AND version = $2")
            .bind(artifact_id)
            .bind(version)
            .fetch_optional(&app.db)
            .await?;
    Ok(files.map(|f| f.0).unwrap_or_default())
}

pub async fn publish(app: &AppState, user: &User, req: PublishRequest) -> AppResult<PublishResult> {
    if req.files.len() > MAX_FILES {
        return Err(AppError::bad_request(format!("At most {MAX_FILES} files per publish")));
    }
    // Decode and validate everything before touching storage.
    let mut uploads: Vec<(FileEntry, Bytes)> = Vec::with_capacity(req.files.len());
    for f in req.files {
        let path = normalize_path(&f.path)?;
        let data = match (f.content, f.content_base64) {
            (Some(text), None) => Bytes::from(text.into_bytes()),
            (None, Some(b64)) => Bytes::from(
                base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|_| AppError::bad_request(format!("{path}: content_base64 is not valid base64")))?,
            ),
            _ => return Err(AppError::bad_request(format!("{path}: provide exactly one of content or content_base64"))),
        };
        if data.len() > MAX_FILE_BYTES {
            return Err(AppError::too_large(format!("{path} is larger than {} MB", MAX_FILE_BYTES / 1024 / 1024)));
        }
        let entry = FileEntry {
            content_type: content_type_for(&path, f.content_type.as_deref()),
            size: data.len() as i64,
            sha256: sha256_hex(&data),
            path,
        };
        uploads.push((entry, data));
    }

    let (artifact_id, created, mut manifest) = match &req.artifact_id {
        Some(id) => {
            let a = require(app, user, id, Role::Editor).await?;
            let prev = if req.replace_all { Vec::new() } else { version_files(app, &a.id, a.current_version).await? };
            (a.id, false, prev.into_iter().map(|f| (f.path.clone(), f)).collect::<BTreeMap<_, _>>())
        }
        None => (random_id(ARTIFACT_ID_LEN), true, BTreeMap::new()),
    };
    for p in &req.remove_paths {
        manifest.remove(&normalize_path(p)?);
    }
    for (entry, data) in &uploads {
        app.storage.put(&blob_key(&artifact_id, &entry.sha256), data.clone(), &entry.content_type).await?;
        manifest.insert(entry.path.clone(), entry.clone());
    }
    if !manifest.contains_key("index.html") {
        return Err(AppError::bad_request("The artifact must contain an index.html"));
    }
    let files: Vec<FileEntry> = manifest.into_values().collect();

    let mut tx = app.db.begin().await?;
    if created {
        let title = req.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Untitled");
        sqlx::query("INSERT INTO artifacts (id, owner_id, title, description) VALUES ($1, $2, $3, $4)")
            .bind(&artifact_id)
            .bind(user.id)
            .bind(truncate(title, 200))
            .bind(truncate(req.description.as_deref().unwrap_or(""), 2000))
            .execute(&mut *tx)
            .await?;
    }
    let version: i32 = sqlx::query_scalar(
        "UPDATE artifacts SET current_version = current_version + 1, updated_at = now(),
                title = COALESCE($2, title), description = COALESCE($3, description)
         WHERE id = $1 RETURNING current_version",
    )
    .bind(&artifact_id)
    .bind(req.title.as_deref().map(str::trim).filter(|t| !t.is_empty() && !created).map(|t| truncate(t, 200)))
    .bind(req.description.as_deref().filter(|_| !created).map(|d| truncate(d, 2000)))
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO artifact_versions (artifact_id, version, files, label, created_by) VALUES ($1, $2, $3, $4, $5)")
        .bind(&artifact_id)
        .bind(version)
        .bind(sqlx::types::Json(&files))
        .bind(req.label.as_deref().map(|l| truncate(l, 200)))
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(artifact = %artifact_id, version, user = %user.email, "published");

    Ok(PublishResult { url: app.cfg.content_origin(&artifact_id), artifact_id, version, created, files })
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Makes an earlier version current again by copying its manifest into a new version.
pub async fn rollback(app: &AppState, user: &User, id: &str, version: i32) -> AppResult<i32> {
    let a = require(app, user, id, Role::Editor).await?;
    let mut tx = app.db.begin().await?;
    let new_version: i32 =
        sqlx::query_scalar("UPDATE artifacts SET current_version = current_version + 1, updated_at = now() WHERE id = $1 RETURNING current_version")
            .bind(&a.id)
            .fetch_one(&mut *tx)
            .await?;
    let copied = sqlx::query(
        "INSERT INTO artifact_versions (artifact_id, version, files, label, created_by)
         SELECT artifact_id, $3, files, 'Restored v' || version, $4 FROM artifact_versions WHERE artifact_id = $1 AND version = $2",
    )
    .bind(&a.id)
    .bind(version)
    .bind(new_version)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    if copied.rows_affected() == 0 {
        return Err(AppError::not_found("Version"));
    }
    tx.commit().await?;
    Ok(new_version)
}

#[derive(Serialize, sqlx::FromRow)]
pub struct VersionInfo {
    pub version: i32,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub created_by_email: Option<String>,
    pub file_count: i32,
}

pub async fn versions(app: &AppState, id: &str) -> AppResult<Vec<VersionInfo>> {
    Ok(sqlx::query_as::<_, VersionInfo>(
        "SELECT v.version, v.label, v.created_at, u.email AS created_by_email, jsonb_array_length(v.files) AS file_count
         FROM artifact_versions v LEFT JOIN users u ON u.id = v.created_by
         WHERE v.artifact_id = $1 ORDER BY v.version DESC LIMIT 100",
    )
    .bind(id)
    .fetch_all(&app.db)
    .await?)
}

#[derive(Serialize, Deserialize, sqlx::FromRow, Clone, Debug)]
pub struct Share {
    pub principal_type: String,
    pub principal: String,
    pub role: String,
}

pub async fn shares(app: &AppState, id: &str) -> AppResult<Vec<Share>> {
    Ok(sqlx::query_as::<_, Share>(
        "SELECT principal_type, principal, role FROM artifact_shares WHERE artifact_id = $1 ORDER BY principal_type, principal",
    )
    .bind(id)
    .fetch_all(&app.db)
    .await?)
}

pub fn validate_share(s: &Share) -> AppResult<Share> {
    let principal = s.principal.trim();
    let principal = match s.principal_type.as_str() {
        "user" if principal.contains('@') => principal.to_lowercase(),
        "group" if !principal.is_empty() => principal.to_string(),
        "user" => return Err(AppError::bad_request("A user share needs an email address")),
        "group" => return Err(AppError::bad_request("A group share needs an Entra group object id")),
        _ => return Err(AppError::bad_request("principal_type must be user or group")),
    };
    if !matches!(s.role.as_str(), "viewer" | "editor") {
        return Err(AppError::bad_request("role must be viewer or editor"));
    }
    Ok(Share { principal_type: s.principal_type.clone(), principal, role: s.role.clone() })
}

pub async fn upsert_share(app: &AppState, id: &str, s: &Share) -> AppResult<()> {
    let s = validate_share(s)?;
    sqlx::query(
        "INSERT INTO artifact_shares (artifact_id, principal_type, principal, role) VALUES ($1, $2, $3, $4)
         ON CONFLICT (artifact_id, principal_type, principal) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(id)
    .bind(&s.principal_type)
    .bind(&s.principal)
    .bind(&s.role)
    .execute(&app.db)
    .await?;
    Ok(())
}

pub async fn remove_share(app: &AppState, id: &str, principal_type: &str, principal: &str) -> AppResult<bool> {
    let p = if principal_type == "user" { principal.trim().to_lowercase() } else { principal.trim().to_string() };
    let r = sqlx::query("DELETE FROM artifact_shares WHERE artifact_id = $1 AND principal_type = $2 AND principal = $3")
        .bind(id)
        .bind(principal_type)
        .bind(p)
        .execute(&app.db)
        .await?;
    Ok(r.rows_affected() > 0)
}

pub async fn set_visibility(app: &AppState, id: &str, visibility: &str) -> AppResult<()> {
    if !matches!(visibility, "private" | "org") {
        return Err(AppError::bad_request("visibility must be private or org"));
    }
    sqlx::query("UPDATE artifacts SET visibility = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(visibility)
        .execute(&app.db)
        .await?;
    Ok(())
}

pub async fn update_meta(app: &AppState, id: &str, title: Option<&str>, description: Option<&str>) -> AppResult<()> {
    sqlx::query("UPDATE artifacts SET title = COALESCE($2, title), description = COALESCE($3, description), updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(title.map(str::trim).filter(|t| !t.is_empty()).map(|t| truncate(t, 200)))
        .bind(description.map(|d| truncate(d, 2000)))
        .execute(&app.db)
        .await?;
    Ok(())
}

pub async fn delete(app: &AppState, user: &User, id: &str) -> AppResult<()> {
    let a = require(app, user, id, Role::Owner).await?;
    sqlx::query("DELETE FROM artifacts WHERE id = $1").bind(&a.id).execute(&app.db).await?;
    app.storage.delete_prefix(&format!("artifacts/{}/", a.id)).await?;
    tracing::info!(artifact = %a.id, user = %user.email, "deleted");
    Ok(())
}

/// Reads one file of a version (current version when `version` is None).
pub async fn read_file(app: &AppState, a: &Artifact, path: &str, version: Option<i32>) -> AppResult<(FileEntry, Bytes)> {
    let files = version_files(app, &a.id, version.unwrap_or(a.current_version)).await?;
    let path = normalize_path(path)?;
    let entry = files.into_iter().find(|f| f.path == path).ok_or_else(|| AppError::not_found("File"))?;
    let data = app.storage.get(&blob_key(&a.id, &entry.sha256)).await?.ok_or_else(|| AppError::internal("blob missing from storage"))?;
    Ok((entry, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normalize_path("./index.html").unwrap(), "index.html");
        assert_eq!(normalize_path("/js/app.js").unwrap(), "js/app.js");
        assert!(normalize_path("../x").is_err());
        assert!(normalize_path("a//b").is_err());
        assert!(normalize_path("__apphost/sdk.js").is_err());
        assert!(normalize_path("a\\b").is_err());
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type_for("index.html", None), "text/html; charset=utf-8");
        assert_eq!(content_type_for("a.png", None), "image/png");
        assert_eq!(content_type_for("x.js", None), "text/javascript; charset=utf-8");
    }
}
