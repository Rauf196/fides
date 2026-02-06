use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use sqlx::PgPool;

#[derive(Clone)]
pub struct HealthState {
    pool: PgPool,
    accepting: Arc<AtomicBool>,
}

impl HealthState {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            accepting: Arc::new(AtomicBool::new(true)),
        }
    }

    /// mark server as shutting down — /ready will return 503
    pub fn set_shutting_down(&self) {
        self.accepting.store(false, Ordering::Relaxed);
    }
}

pub fn router(state: HealthState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .with_state(state)
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn ready(State(state): State<HealthState>) -> StatusCode {
    // fail immediately during shutdown so k8s diverts traffic
    if !state.accepting.load(Ordering::Relaxed) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }

    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}
