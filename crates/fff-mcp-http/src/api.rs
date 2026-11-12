use std::sync::Arc;

use axum::extract::{RawQuery, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
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

#[derive(Clone)]
struct BearerToken(Arc<str>);

pub(crate) fn router(registry: Arc<Registry>) -> Router {
    let config = registry.config().clone();
    let mcp_registry = registry.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(FindFilesServer::new(mcp_registry.clone())),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(config.resolved_allowed_hosts()),
    );

    // /healthz stays outside the token gate; everything else needs it when set.
    let mut protected = Router::new()
        .route("/search", get(search))
        .route("/stats", get(stats))
        .route_service("/mcp", mcp);
    if config.test_hook {
        protected = protected.route("/test/evict", post(force_eviction));
    }
    if let Some(token) = config.bearer_token() {
        protected = protected.route_layer(axum::middleware::from_fn_with_state(
            BearerToken(Arc::from(token)),
            require_token,
        ));
    }

    Router::new()
        .route("/healthz", get(healthz))
        .merge(protected)
        .with_state(registry)
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

async fn require_token(
    State(BearerToken(expected)): State<BearerToken>,
    request: Request,
    next: Next,
) -> Response {
    let provided = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_value);
    match provided {
        Some(provided) if token_matches(&expected, provided) => next.run(request).await,
        _ => {
            tracing::debug!(path = %request.uri().path(), "rejected request with a missing or wrong token");
            ApiError::unauthorized().into_response()
        }
    }
}

fn bearer_value(header: &str) -> Option<&str> {
    let (scheme, value) = header.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| value.trim())
}

/// Byte-at-a-time compare that never returns early, so it is not a timing oracle.
fn token_matches(expected: &str, provided: &str) -> bool {
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();
    let mut difference = expected.len() ^ provided.len();
    for (index, byte) in expected.iter().enumerate() {
        difference |= usize::from(byte ^ provided.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_header_needs_the_scheme_and_a_value() {
        assert_eq!(bearer_value("Bearer abc"), Some("abc"));
        assert_eq!(bearer_value("bearer abc "), Some("abc"));
        assert_eq!(bearer_value("Basic abc"), None);
        assert_eq!(bearer_value("abc"), None);
    }

    #[test]
    fn token_compare_rejects_any_difference() {
        assert!(token_matches("s3cret", "s3cret"));
        assert!(!token_matches("s3cret", "s3cre"));
        assert!(!token_matches("s3cret", "s3cret2"));
        assert!(!token_matches("s3cret", "s3cres"));
        assert!(!token_matches("s3cret", ""));
    }
}
