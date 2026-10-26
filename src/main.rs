mod artifacts;
mod auth;
mod config;
mod content;
mod data;
mod entra;
mod error;
mod guide;
mod login;
mod mcp;
mod oauth;
mod portal;
mod storage;
mod util;

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::broadcast;
use tower::ServiceExt;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::data::DataEvent;
use crate::entra::Entra;
use crate::storage::Storage;

pub struct App {
    pub cfg: Config,
    pub db: PgPool,
    pub storage: Storage,
    pub entra: Option<Entra>,
    pub events: broadcast::Sender<DataEvent>,
}

pub type AppState = Arc<App>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("apphost=info,tower_http=info"));
    if std::env::var("APPHOST_LOG_JSON").is_ok_and(|v| v == "true") {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let cfg = Config::from_env()?;
    let db = PgPoolOptions::new().max_connections(20).connect(&cfg.database_url).await?;
    sqlx::migrate!("./migrations").run(&db).await?;
    let storage = Storage::from_config(&cfg.storage).await?;
    let entra = cfg.entra.clone().map(Entra::new);
    let (events, _) = broadcast::channel(1024);
    let app: AppState = Arc::new(App { cfg, db, storage, entra, events });

    data::spawn_listener(app.clone());
    auth::spawn_cleanup(app.clone());

    let listener = tokio::net::TcpListener::bind(&app.cfg.bind).await?;
    tracing::info!(
        bind = %app.cfg.bind,
        portal = %app.cfg.public_url,
        content = %app.cfg.content_origin("<id>"),
        "apphost listening"
    );
    axum::serve(listener, router(app)).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

/// One listener serves two kinds of origin: the portal (UI, APIs, OAuth, MCP) and one isolated
/// origin per artifact (<id>.<content domain>). Requests are dispatched on the Host header.
pub fn router(app: AppState) -> Router {
    let portal = portal::router(app.clone());
    let content = content::router(app.clone());
    Router::new()
        .fallback(move |req: Request| {
            let (portal, content, app) = (portal.clone(), content.clone(), app.clone());
            async move {
                let host = util::request_host(req.headers(), req.uri());
                let res: Result<Response, _> = if app.cfg.artifact_id_from_host(&host).is_some() {
                    content.oneshot(req).await
                } else {
                    portal.oneshot(req).await
                };
                res.unwrap_or_else(|e: std::convert::Infallible| match e {}).into_response()
            }
        })
        .layer(TraceLayer::new_for_http())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}
