//! Portal origin: UI, JSON API, sign-in, OAuth and the MCP endpoint.

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use crate::AppState;
use crate::artifacts::{self, PublishRequest, Role, Share};
use crate::auth::{self, AuthUser, TokenKind};
use crate::content::handoff_token;
use crate::data;
use crate::error::{AppError, AppResult};
use crate::login::{self, error_page, login_redirect};
use crate::{mcp, oauth};

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const BACKDROP_JS: &str = include_str!("../ui/backdrop.js");
const APP_CSS: &str = include_str!("../ui/app.css");
const TOKENS_CSS: &str = include_str!("../ui/tokens.css");
const BASE_CSS: &str = include_str!("../ui/base.css");
const FAVICON: &str = include_str!("../ui/favicon.svg");

pub fn router(app: AppState) -> Router {
    let max = app.cfg.max_publish_bytes;
    // Bearer-token endpoints used by MCP clients; no cookies involved, so open CORS is safe.
    let open_cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any).expose_headers(Any);
    let machine = Router::new()
        .merge(oauth::machine_routes())
        .route("/mcp", post(mcp::handle).get(mcp::not_allowed).delete(mcp::not_allowed))
        .layer(DefaultBodyLimit::max(max))
        .layer(open_cors);

    let api = Router::new()
        .route("/api/me", get(me))
        .route("/api/artifacts", get(list_artifacts).post(publish))
        .route("/api/artifacts/{id}", get(get_artifact).patch(patch_artifact).delete(delete_artifact))
        .route("/api/artifacts/{id}/versions", post(publish_version))
        .route("/api/artifacts/{id}/rollback", post(rollback))
        .route("/api/artifacts/{id}/shares", post(add_share).delete(remove_share))
        .route("/api/artifacts/{id}/data", get(data_collections))
        .route("/api/artifacts/{id}/data/{collection}", get(data_docs).delete(data_clear))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/{id}", axum::routing::delete(revoke_token))
        .route("/api/connections/{client_id}", axum::routing::delete(revoke_connection))
        .layer(DefaultBodyLimit::max(max))
        .layer(axum::middleware::map_response(no_store));

    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }))
        .route("/assets/backdrop.js", get(|| async { asset("text/javascript; charset=utf-8", BACKDROP_JS) }))
        .route("/assets/app.css", get(|| async { asset("text/css; charset=utf-8", APP_CSS) }))
        .route("/assets/tokens.css", get(|| async { asset("text/css; charset=utf-8", TOKENS_CSS) }))
        .route("/assets/base.css", get(|| async { asset("text/css; charset=utf-8", BASE_CSS) }))
        .route("/assets/favicon.svg", get(|| async { asset("image/svg+xml", FAVICON) }))
        .route("/content-auth", get(content_auth))
        .route("/healthz", get(healthz))
        .merge(login::routes())
        .merge(oauth::browser_routes())
        .merge(api)
        .merge(machine)
        .layer(axum::middleware::map_response(portal_headers))
        .with_state(app)
}

async fn portal_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'",
            ),
        );
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin-when-cross-origin"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    res
}

async fn no_store(mut res: Response) -> Response {
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn healthz(State(app): State<AppState>) -> Response {
    match sqlx::query("SELECT 1").execute(&app.db).await {
        Ok(_) => "ok".into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")).into_response(),
    }
}

async fn index(State(app): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    if auth::session_user(&app, &headers).await?.is_none() {
        return Ok(login_redirect("/"));
    }
    Ok(Html(INDEX_HTML.replace("<!--backdrop-->", login::BACKDROP)).into_response())
}

#[derive(Deserialize)]
struct ContentAuthQuery {
    a: String,
    p: Option<String>,
}

/// Step 1 of opening an artifact: the content origin sends the browser here; we check SSO and
/// access, then bounce back with a one-minute hand-off token.
async fn content_auth(State(app): State<AppState>, headers: HeaderMap, uri: axum::http::Uri, Query(q): Query<ContentAuthQuery>) -> AppResult<Response> {
    let Some(user) = auth::session_user(&app, &headers).await? else {
        return Ok(login_redirect(&uri.to_string()));
    };
    let Some(a) = artifacts::get_accessible(&app, &user, &q.a).await? else {
        return Ok(error_page(
            StatusCode::FORBIDDEN,
            "No access",
            &format!("You ({}) don't have access to this artifact, or it no longer exists. Ask its owner to share it with you.", user.email),
        ));
    };
    let token = handoff_token(&app, &a.id, user.id);
    let p = crate::util::safe_return_path(q.p.as_deref());
    Ok(Redirect::to(&format!(
        "{}/__apphost/auth?t={}&p={}",
        app.cfg.content_origin(&a.id),
        urlencoding::encode(&token),
        urlencoding::encode(&p)
    ))
    .into_response())
}

async fn me(State(app): State<AppState>, AuthUser { user }: AuthUser) -> Json<Value> {
    Json(json!({
        "user": user,
        "mcp_url": app.cfg.mcp_url(),
        "portal_url": app.cfg.public_url,
        "sso": if app.entra.is_some() { "entra" } else { "dev" },
    }))
}

#[derive(Deserialize)]
struct ListQuery {
    scope: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
}

fn artifact_json(app: &AppState, a: &artifacts::Artifact) -> Value {
    let mut v = json!(a);
    v["url"] = json!(app.cfg.content_origin(&a.id));
    v
}

async fn list_artifacts(State(app): State<AppState>, AuthUser { user }: AuthUser, Query(q): Query<ListQuery>) -> AppResult<Json<Value>> {
    let list = artifacts::list(&app, &user, q.scope.as_deref().unwrap_or("all"), q.q.as_deref().unwrap_or(""), q.limit.unwrap_or(200)).await?;
    Ok(Json(json!({ "artifacts": list.iter().map(|a| artifact_json(&app, a)).collect::<Vec<_>>() })))
}

async fn get_artifact(State(app): State<AppState>, AuthUser { user }: AuthUser, Path(id): Path<String>) -> AppResult<Json<Value>> {
    let a = artifacts::require(&app, &user, &id, Role::Viewer).await?;
    let mut v = artifact_json(&app, &a);
    v["files"] = json!(artifacts::version_files(&app, &a.id, a.current_version).await?);
    v["versions"] = json!(artifacts::versions(&app, &a.id).await?);
    v["shares"] = json!(if a.role() >= Role::Editor { artifacts::shares(&app, &a.id).await? } else { vec![] });
    Ok(Json(v))
}

#[derive(Deserialize)]
struct PatchBody {
    title: Option<String>,
    description: Option<String>,
    visibility: Option<String>,
}

async fn patch_artifact(
    State(app): State<AppState>,
    AuthUser { user }: AuthUser,
    Path(id): Path<String>,
    Json(b): Json<PatchBody>,
) -> AppResult<Json<Value>> {
    let a = artifacts::require(&app, &user, &id, Role::Editor).await?;
    if let Some(vis) = &b.visibility {
        if a.role() < Role::Owner {
            return Err(AppError::forbidden("Only the owner can change visibility"));
        }
        artifacts::set_visibility(&app, &a.id, vis).await?;
    }
    artifacts::update_meta(&app, &a.id, b.title.as_deref(), b.description.as_deref()).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_artifact(State(app): State<AppState>, AuthUser { user }: AuthUser, Path(id): Path<String>) -> AppResult<StatusCode> {
    artifacts::delete(&app, &user, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn publish(State(app): State<AppState>, AuthUser { user }: AuthUser, Json(req): Json<PublishRequest>) -> AppResult<Response> {
    let r = artifacts::publish(&app, &user, req).await?;
    Ok((if r.created { StatusCode::CREATED } else { StatusCode::OK }, Json(json!(r))).into_response())
}

async fn publish_version(
    State(app): State<AppState>,
    AuthUser { user }: AuthUser,
    Path(id): Path<String>,
    Json(mut req): Json<PublishRequest>,
) -> AppResult<Json<Value>> {
    req.artifact_id = Some(id);
    Ok(Json(json!(artifacts::publish(&app, &user, req).await?)))
}

#[derive(Deserialize)]
struct RollbackBody {
    version: i32,
}

async fn rollback(
    State(app): State<AppState>,
    AuthUser { user }: AuthUser,
    Path(id): Path<String>,
    Json(b): Json<RollbackBody>,
) -> AppResult<Json<Value>> {
    let v = artifacts::rollback(&app, &user, &id, b.version).await?;
    Ok(Json(json!({ "version": v })))
}

async fn add_share(State(app): State<AppState>, AuthUser { user }: AuthUser, Path(id): Path<String>, Json(s): Json<Share>) -> AppResult<Json<Value>> {
    let a = artifacts::require(&app, &user, &id, Role::Owner).await?;
    artifacts::upsert_share(&app, &a.id, &s).await?;
    Ok(Json(json!({ "shares": artifacts::shares(&app, &a.id).await? })))
}

#[derive(Deserialize)]
struct RemoveShareQuery {
    principal_type: String,
    principal: String,
}

async fn remove_share(
    State(app): State<AppState>,
    AuthUser { user }: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<RemoveShareQuery>,
) -> AppResult<Json<Value>> {
    let a = artifacts::require(&app, &user, &id, Role::Owner).await?;
    artifacts::remove_share(&app, &a.id, &q.principal_type, &q.principal).await?;
    Ok(Json(json!({ "shares": artifacts::shares(&app, &a.id).await? })))
}

async fn data_collections(State(app): State<AppState>, AuthUser { user }: AuthUser, Path(id): Path<String>) -> AppResult<Json<Value>> {
    let a = artifacts::require(&app, &user, &id, Role::Viewer).await?;
    Ok(Json(json!({ "collections": data::collections(&app, &a.id).await? })))
}

#[derive(Deserialize)]
struct DocsQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn data_docs(
    State(app): State<AppState>,
    AuthUser { user }: AuthUser,
    Path((id, collection)): Path<(String, String)>,
    Query(q): Query<DocsQuery>,
) -> AppResult<Json<Value>> {
    let a = artifacts::require(&app, &user, &id, Role::Viewer).await?;
    let docs = data::list(&app, &a.id, &collection, q.limit.unwrap_or(200), q.offset.unwrap_or(0)).await?;
    Ok(Json(json!({ "docs": docs })))
}

async fn data_clear(
    State(app): State<AppState>,
    AuthUser { user }: AuthUser,
    Path((id, collection)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let a = artifacts::require(&app, &user, &id, Role::Owner).await?;
    Ok(Json(json!({ "deleted": data::clear(&app, &a.id, &collection).await? })))
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct TokenRow {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct ConnectionRow {
    client_id: String,
    client_name: String,
    connected_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

async fn list_tokens(State(app): State<AppState>, AuthUser { user }: AuthUser) -> AppResult<Json<Value>> {
    let tokens = sqlx::query_as::<_, TokenRow>(
        "SELECT id, name, created_at, expires_at, last_used_at FROM tokens
         WHERE user_id = $1 AND kind = 'pat' AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())
         ORDER BY created_at DESC",
    )
    .bind(user.id)
    .fetch_all(&app.db)
    .await?;
    let connections = sqlx::query_as::<_, ConnectionRow>(
        "SELECT c.client_id, c.client_name, min(t.created_at) AS connected_at, max(t.last_used_at) AS last_used_at
         FROM tokens t JOIN oauth_clients c ON c.client_id = t.client_id
         WHERE t.user_id = $1
           AND EXISTS (SELECT 1 FROM tokens r WHERE r.user_id = $1 AND r.client_id = c.client_id AND r.kind = 'oauth_refresh'
                       AND r.revoked_at IS NULL AND r.expires_at > now())
         GROUP BY c.client_id, c.client_name ORDER BY connected_at DESC",
    )
    .bind(user.id)
    .fetch_all(&app.db)
    .await?;
    Ok(Json(json!({ "tokens": tokens, "connections": connections })))
}

#[derive(Deserialize)]
struct CreateTokenBody {
    name: String,
    expires_in_days: Option<i64>,
}

async fn create_token(State(app): State<AppState>, AuthUser { user }: AuthUser, Json(b): Json<CreateTokenBody>) -> AppResult<Json<Value>> {
    let name = b.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(AppError::bad_request("Give the token a name (up to 100 characters)"));
    }
    let days = b.expires_in_days.unwrap_or(90);
    if !(1..=365).contains(&days) {
        return Err(AppError::bad_request("Tokens expire after 1 to 365 days"));
    }
    let expires = Utc::now() + Duration::days(days);
    let (id, token) = auth::create_token(&app, user.id, TokenKind::Pat, name, None, Some(expires)).await?;
    Ok(Json(json!({ "id": id, "token": token, "expires_at": expires })))
}

async fn revoke_token(State(app): State<AppState>, AuthUser { user }: AuthUser, Path(id): Path<Uuid>) -> AppResult<StatusCode> {
    sqlx::query("UPDATE tokens SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND kind = 'pat'")
        .bind(id)
        .bind(user.id)
        .execute(&app.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_connection(State(app): State<AppState>, AuthUser { user }: AuthUser, Path(client_id): Path<String>) -> AppResult<StatusCode> {
    sqlx::query("UPDATE tokens SET revoked_at = now() WHERE client_id = $1 AND user_id = $2 AND revoked_at IS NULL")
        .bind(client_id)
        .bind(user.id)
        .execute(&app.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
