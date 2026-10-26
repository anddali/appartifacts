//! Artifact content origins (<id>.<content domain>): file serving, the sign-in hand-off from the
//! portal, and the same-origin shared-state API used by the page SDK.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header, request::Parts};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::AppState;
use crate::artifacts::{self, Artifact};
use crate::auth::{self, CSRF_HEADER, User};
use crate::data::{self, DataEvent, WriteOpts};
use crate::error::{AppError, AppResult};
use crate::util::{get_cookie, request_host, safe_return_path, set_cookie, sign, verify};

const CONTENT_COOKIE: &str = "apphost_c";
const CONTENT_SESSION_SECS: i64 = 8 * 3600;
const SDK_JS: &str = include_str!("../ui/sdk.js");

pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/__apphost/sdk.js", get(sdk))
        .route("/__apphost/auth", get(handoff))
        .route("/__apphost/api/me", get(me))
        .route("/__apphost/api/events", get(events))
        .route("/__apphost/api/data/{collection}", get(list_docs).post(create_doc))
        .route("/__apphost/api/data/{collection}/{doc_id}", get(get_doc).put(put_doc).patch(patch_doc).delete(delete_doc))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(serve_file)
        .with_state(app)
}

#[derive(Serialize, Deserialize)]
pub struct ContentClaims {
    pub a: String,
    pub u: Uuid,
}

/// Portal-issued, short-lived token that proves the user passed SSO and the access check.
pub fn handoff_token(app: &AppState, artifact_id: &str, user_id: Uuid) -> String {
    sign(&app.cfg.secret, "content-handoff", &ContentClaims { a: artifact_id.into(), u: user_id }, 60)
}

fn artifact_id(app: &AppState, parts_headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let host = request_host(parts_headers, uri);
    app.cfg.artifact_id_from_host(&host).map(str::to_string)
}

/// The viewer of an artifact page, with access re-checked on every request.
pub struct Viewer {
    pub user: User,
    pub artifact: Artifact,
}

pub enum ViewerRejection {
    SignIn(String),
    Api(AppError),
}

impl IntoResponse for ViewerRejection {
    fn into_response(self) -> Response {
        match self {
            ViewerRejection::SignIn(url) => Redirect::to(&url).into_response(),
            ViewerRejection::Api(e) => e.into_response(),
        }
    }
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = ViewerRejection;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Self::Rejection> {
        let is_api = parts.uri.path().starts_with("/__apphost/api/");
        let Some(id) = artifact_id(app, &parts.headers, &parts.uri) else {
            return Err(ViewerRejection::Api(AppError::not_found("Artifact")));
        };
        let sign_in = || {
            if is_api {
                ViewerRejection::Api(AppError::unauthorized())
            } else {
                let p = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                ViewerRejection::SignIn(format!(
                    "{}/content-auth?a={}&p={}",
                    app.cfg.public_url,
                    id,
                    urlencoding::encode(p)
                ))
            }
        };
        let claims: Option<ContentClaims> = get_cookie(&parts.headers, &app.cfg.cookie_name(CONTENT_COOKIE))
            .and_then(|c| verify(&app.cfg.secret, "content-session", &c))
            .filter(|c: &ContentClaims| c.a == id);
        let Some(claims) = claims else { return Err(sign_in()) };
        let user = auth::get_user(app, claims.u).await.map_err(ViewerRejection::Api)?.ok_or_else(sign_in)?;
        let artifact = artifacts::get_accessible(app, &user, &id).await.map_err(ViewerRejection::Api)?;
        let Some(artifact) = artifact else {
            // Access was revoked (or the artifact deleted) since the cookie was issued.
            return Err(if is_api { ViewerRejection::Api(AppError::forbidden("No access")) } else { sign_in() });
        };
        let safe = matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS);
        if is_api && !safe && !parts.headers.contains_key(CSRF_HEADER) {
            return Err(ViewerRejection::Api(AppError::forbidden("Missing CSRF header")));
        }
        Ok(Viewer { user, artifact })
    }
}

#[derive(Deserialize)]
struct HandoffQuery {
    t: String,
    p: Option<String>,
}

async fn handoff(State(app): State<AppState>, headers: HeaderMap, uri: Uri, Query(q): Query<HandoffQuery>) -> AppResult<Response> {
    let id = artifact_id(&app, &headers, &uri).ok_or_else(|| AppError::not_found("Artifact"))?;
    let claims: ContentClaims = verify(&app.cfg.secret, "content-handoff", &q.t)
        .filter(|c: &ContentClaims| c.a == id)
        .ok_or_else(|| AppError::bad_request("Sign-in link expired. Reload the page."))?;
    let cookie = set_cookie(
        &app.cfg.cookie_name(CONTENT_COOKIE),
        &sign(&app.cfg.secret, "content-session", &claims, CONTENT_SESSION_SECS),
        CONTENT_SESSION_SECS,
        app.cfg.secure,
    );
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to(&safe_return_path(q.p.as_deref()))).into_response())
}

fn csp(app: &AppState) -> String {
    let c = &app.cfg;
    format!(
        "default-src 'self'; script-src 'self' 'unsafe-inline' 'unsafe-eval' {script}; style-src 'self' 'unsafe-inline' {style}; \
         font-src 'self' data: {font}; img-src 'self' data: blob: {img}; media-src 'self' data: blob:; \
         connect-src 'self' {connect}; worker-src 'self' blob:; frame-src 'self'; frame-ancestors 'self'; \
         form-action 'self'; base-uri 'self'; object-src 'none'",
        script = c.csp_script_src,
        style = c.csp_style_src,
        font = c.csp_font_src,
        img = c.csp_img_src,
        connect = c.csp_connect_src,
    )
}

fn security_headers(app: &AppState, res: &mut Response) {
    let h = res.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(&csp(app)).unwrap());
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("permissions-policy", HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=(), usb=()"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
}

async fn serve_file(State(app): State<AppState>, viewer: Viewer, method: Method, headers: HeaderMap, uri: Uri) -> AppResult<Response> {
    if !matches!(method, Method::GET | Method::HEAD) {
        return Err(AppError::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "Method not allowed"));
    }
    let raw = urlencoding::decode(uri.path()).map_err(|_| AppError::bad_request("Bad path"))?;
    let mut path = raw.trim_start_matches('/').to_string();
    if path.is_empty() || path.ends_with('/') {
        path.push_str("index.html");
    }
    let a = &viewer.artifact;
    let files = artifacts::version_files(&app, &a.id, a.current_version).await?;
    let entry = files
        .iter()
        .find(|f| f.path == path)
        .or_else(|| files.iter().find(|f| f.path == format!("{path}/index.html")));
    let Some(entry) = entry else {
        let mut res = (StatusCode::NOT_FOUND, "Not found").into_response();
        security_headers(&app, &mut res);
        return Ok(res);
    };
    let etag = format!("\"{}\"", entry.sha256);
    let mut res = if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let data = app
            .storage
            .get(&artifacts::blob_key(&a.id, &entry.sha256))
            .await?
            .ok_or_else(|| AppError::internal("blob missing from storage"))?;
        ([(header::CONTENT_TYPE, entry.content_type.clone())], data).into_response()
    };
    let h = res.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    security_headers(&app, &mut res);
    Ok(res)
}

async fn sdk() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=300")],
        SDK_JS,
    )
        .into_response()
}

async fn me(viewer: Viewer) -> Json<Value> {
    Json(json!({
        "id": viewer.user.id,
        "email": viewer.user.email,
        "name": viewer.user.name,
        "role": viewer.artifact.role(),
        "artifact": { "id": viewer.artifact.id, "title": viewer.artifact.title, "version": viewer.artifact.current_version },
    }))
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_docs(
    State(app): State<AppState>,
    viewer: Viewer,
    Path(collection): Path<String>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let docs = data::list(&app, &viewer.artifact.id, &collection, q.limit.unwrap_or(1000), q.offset.unwrap_or(0)).await?;
    Ok(Json(json!({ "docs": docs })))
}

async fn get_doc(State(app): State<AppState>, viewer: Viewer, Path((collection, doc_id)): Path<(String, String)>) -> AppResult<Json<Value>> {
    let doc = data::get(&app, &viewer.artifact.id, &collection, &doc_id).await?.ok_or_else(|| AppError::not_found("Document"))?;
    Ok(Json(json!(doc)))
}

#[derive(Deserialize)]
pub struct WriteBody {
    pub data: Value,
    pub expected_version: Option<i64>,
}

async fn create_doc(
    State(app): State<AppState>,
    viewer: Viewer,
    Path(collection): Path<String>,
    Json(body): Json<WriteBody>,
) -> AppResult<Response> {
    let doc = data::write(&app, &viewer.artifact.id, &collection, None, body.data, viewer.user.id, WriteOpts {
        merge: false,
        expected_version: Some(0),
    })
    .await?;
    Ok((StatusCode::CREATED, Json(json!(doc))).into_response())
}

async fn put_doc(
    State(app): State<AppState>,
    viewer: Viewer,
    Path((collection, doc_id)): Path<(String, String)>,
    Json(body): Json<WriteBody>,
) -> AppResult<Json<Value>> {
    let opts = WriteOpts { merge: false, expected_version: body.expected_version };
    let doc = data::write(&app, &viewer.artifact.id, &collection, Some(&doc_id), body.data, viewer.user.id, opts).await?;
    Ok(Json(json!(doc)))
}

async fn patch_doc(
    State(app): State<AppState>,
    viewer: Viewer,
    Path((collection, doc_id)): Path<(String, String)>,
    Json(body): Json<WriteBody>,
) -> AppResult<Json<Value>> {
    let opts = WriteOpts { merge: true, expected_version: body.expected_version };
    let doc = data::write(&app, &viewer.artifact.id, &collection, Some(&doc_id), body.data, viewer.user.id, opts).await?;
    Ok(Json(json!(doc)))
}

async fn delete_doc(State(app): State<AppState>, viewer: Viewer, Path((collection, doc_id)): Path<(String, String)>) -> AppResult<Response> {
    if !data::delete(&app, &viewer.artifact.id, &collection, &doc_id).await? {
        return Err(AppError::not_found("Document"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Server-sent events with every change to this artifact's data. The stream ends after an hour so
/// the browser reconnects and access is checked again.
async fn events(State(app): State<AppState>, viewer: Viewer) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let id = viewer.artifact.id.clone();
    let rx = BroadcastStream::new(app.events.subscribe());
    let stream = rx
        .filter_map(move |msg| {
            let id = id.clone();
            async move {
                let ev = match msg {
                    Ok(ev) if ev.artifact_id == id || ev.artifact_id == "*" => ev,
                    Ok(_) => return None,
                    // This subscriber fell behind; tell the page to refetch.
                    Err(_) => DataEvent { artifact_id: id, op: "resync".into(), collection: None, id: None, doc: None },
                };
                Some(Ok(Event::default().event("change").json_data(&ev).unwrap_or_default()))
            }
        })
        .take_until(tokio::time::sleep(Duration::from_secs(3600)));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
