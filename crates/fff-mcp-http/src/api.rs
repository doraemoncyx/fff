use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;

use crate::error::ApiError;
use crate::mcp::FindFilesServer;
use crate::params::Params;
use crate::registry::Registry;
use crate::search::SearchResponse;
use crate::stats::StatsResponse;

pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")");

pub(crate) fn router(registry: Arc<Registry>) -> Router {
    let mcp_registry = registry.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(FindFilesServer::new(mcp_registry.clone())),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    let test_hook = registry.config().test_hook;

    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/search", get(search))
        .route("/stats", get(stats))
        .route_service("/mcp", mcp);
    if test_hook {
        app = app.route("/test/evict", post(force_eviction));
    }
    app.with_state(registry)
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": VERSION }))
}

async fn search(
    State(registry): State<Arc<Registry>>,
    RawQuery(raw): RawQuery,
) -> Result<Json<SearchResponse>, ApiError> {
    let raw = raw.unwrap_or_default();
    let params = Params::from_pairs(form_urlencoded::parse(raw.as_bytes()))?;
    let response = tokio::task::spawn_blocking(move || registry.query(&params))
        .await
        .map_err(|error| ApiError::internal(format!("search task failed: {error}")))??;
    Ok(Json(response))
}

async fn stats(State(registry): State<Arc<Registry>>) -> Json<StatsResponse> {
    registry.mark_activity();
    Json(registry.stats())
}

/// Test hook: run one eviction round outside any request.
async fn force_eviction(State(registry): State<Arc<Registry>>) -> Json<serde_json::Value> {
    let evicted = tokio::task::spawn_blocking(move || registry.evict_now())
        .await
        .unwrap_or_default();
    Json(json!({ "evicted": evicted }))
}
