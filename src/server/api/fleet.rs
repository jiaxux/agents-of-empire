//! Lazy read-only Fleet Operations projection.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::AppState;

pub async fn get_fleet(State(state): State<Arc<AppState>>) -> Response {
    if let Some(response) = super::cityhall_block(&state) {
        return response;
    }
    let instances = state.instances.read().await.clone();
    let profile = state.profile.clone();
    match tokio::task::spawn_blocking(move || {
        let max_workers = crate::session::profile_config::resolve_config_or_warn(&profile)
            .acp
            .max_concurrent_workers;
        crate::fleet::FleetReport::build(&instances, max_workers)
    })
    .await
    {
        Ok(report) => Json(report).into_response(),
        Err(error) => {
            tracing::error!(target: "http.api.fleet", %error, "fleet projection worker failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "fleet_projection_failed",
                    "message": "Fleet Operations could not build its projection"
                })),
            )
                .into_response()
        }
    }
}
