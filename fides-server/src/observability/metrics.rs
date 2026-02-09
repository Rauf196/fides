use std::sync::OnceLock;

use axum::routing::get;
use axum::Router;
use metrics::{describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static METRICS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// install the global prometheus recorder (idempotent via OnceLock)
///
/// safe to call from multiple integration tests — only the first call installs,
/// subsequent calls return the same handle.
pub fn install_recorder() -> PrometheusHandle {
    METRICS_HANDLE
        .get_or_init(|| {
            let handle = PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install metrics recorder");
            describe_metrics();
            handle
        })
        .clone()
}

/// register HELP text for all metrics
fn describe_metrics() {
    // request-level (tower middleware)
    describe_counter!(
        "fides_grpc_requests_total",
        "total gRPC requests by method and status"
    );
    describe_histogram!(
        "fides_grpc_request_duration_seconds",
        "gRPC request duration in seconds by method"
    );

    // domain (manual in handlers)
    describe_counter!(
        "fides_transactions_total",
        "total transactions by final status"
    );
    describe_counter!(
        "fides_insufficient_funds_total",
        "total authorize requests rejected for insufficient funds"
    );
    describe_counter!(
        "fides_idempotent_hits_total",
        "total idempotent duplicate requests by method"
    );

    // infrastructure (gauge poll)
    describe_gauge!("fides_db_pool_size", "total database pool connections");
    describe_gauge!("fides_db_pool_idle", "idle database pool connections");
    describe_gauge!(
        "fides_balance_cache_accounts",
        "number of accounts in the balance cache"
    );

    // integrity (background checker)
    describe_gauge!(
        "fides_integrity_global_balanced",
        "1.0 if global debits equal credits, 0.0 if violated"
    );
    describe_gauge!(
        "fides_integrity_account_mismatches",
        "number of accounts with materialized balance != entry-computed balance"
    );
    describe_gauge!(
        "fides_integrity_cache_mismatches",
        "number of accounts with cache balance != database balance"
    );
    describe_histogram!(
        "fides_integrity_check_duration_seconds",
        "duration of each integrity check in seconds"
    );
    describe_gauge!(
        "fides_integrity_last_check_timestamp",
        "unix timestamp of the last completed integrity check"
    );
}

/// axum router serving the /metrics endpoint
pub fn router(handle: PrometheusHandle) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
            let h = handle.clone();
            async move { h.render() }
        }),
    )
}
