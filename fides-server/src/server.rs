use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use metrics::gauge;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::watch;

use fides_proto::ledger_service_server::LedgerServiceServer;

use crate::config::AppConfig;
use crate::health::HealthState;
use crate::observability::grpc_metrics::GrpcMetricsLayer;
use crate::observability::integrity::IntegrityChecker;
use crate::service::ledger::LedgerService;
use crate::storage::postgres::PostgresStorage;
use crate::storage::BalanceCache;

/// connect to postgres using config, verify connectivity
pub async fn connect_db(config: &AppConfig) -> Result<PgPool, Box<dyn std::error::Error>> {
    let pool = PgPoolOptions::new()
        .max_connections(config.database.max_connections)
        .min_connections(config.database.min_connections)
        .acquire_timeout(Duration::from_secs(config.database.acquire_timeout_secs))
        .idle_timeout(Duration::from_secs(config.database.idle_timeout_secs))
        .connect(&config.database.url)
        .await?;

    sqlx::query("SELECT 1").execute(&pool).await?;
    tracing::info!("database connected");

    Ok(pool)
}

/// run embedded migrations against the pool
pub async fn run_migrations(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("running migrations");
    sqlx::migrate!("../migrations").run(pool).await?;
    tracing::info!("migrations complete");
    Ok(())
}

/// start the ledger server (gRPC + HTTP health/metrics), block until shutdown
pub async fn serve(
    pool: PgPool,
    config: &AppConfig,
    shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    // install metrics recorder (idempotent via OnceLock)
    let metrics_handle = crate::observability::metrics::install_recorder();

    let storage = Arc::new(PostgresStorage::new(pool.clone()));
    let cache = Arc::new(BalanceCache::new());

    // rehydrate cache from db
    let balances = storage.load_all_balances().await?;
    let count = balances.len();
    cache.rehydrate(balances);
    tracing::info!(accounts = count, "balance cache rehydrated");

    let ledger_service = LedgerService::new(storage, cache.clone());

    // shutdown coordination
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // spawn gauge poll task (pool stats + cache size, every 15s)
    let gauge_pool = pool.clone();
    let gauge_cache = cache.clone();
    let mut gauge_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            let should_run = tokio::select! {
                _ = interval.tick() => true,
                _ = gauge_shutdown_rx.wait_for(|v| *v) => false,
            };

            if should_run {
                gauge!("fides_db_pool_size").set(gauge_pool.size() as f64);
                gauge!("fides_db_pool_idle").set(gauge_pool.num_idle() as f64);
                gauge!("fides_balance_cache_accounts").set(gauge_cache.len() as f64);
            } else {
                break;
            }
        }
    });

    // spawn integrity checker
    let integrity_interval = Duration::from_secs(
        config.observability.integrity_check_interval_secs,
    );
    let checker = IntegrityChecker::new(pool.clone(), cache, integrity_interval);
    let integrity_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        checker.run(integrity_shutdown_rx).await;
    });

    // http server (health probes + metrics endpoint)
    let http_addr = SocketAddr::from(([0, 0, 0, 0], config.server.http_port));
    let health_state = HealthState::new(pool.clone());
    let health_shutdown = health_state.clone();
    let http_router = crate::health::router(health_state)
        .merge(crate::observability::metrics::router(metrics_handle));

    let mut http_shutdown_rx = shutdown_rx.clone();
    let http_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await?;
        axum::serve(listener, http_router)
            .with_graceful_shutdown(async move {
                let _ = http_shutdown_rx.wait_for(|v| *v).await;
            })
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    // grpc server (with metrics layer)
    let grpc_addr = SocketAddr::from(([0, 0, 0, 0], config.server.grpc_port));
    let mut grpc_shutdown_rx = shutdown_rx.clone();
    let grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .layer(GrpcMetricsLayer)
            .add_service(LedgerServiceServer::new(ledger_service))
            .serve_with_shutdown(grpc_addr, async move {
                let _ = grpc_shutdown_rx.wait_for(|v| *v).await;
            })
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    tracing::info!(
        grpc = %grpc_addr,
        http = %http_addr,
        "fides ready",
    );

    // wait for shutdown signal
    shutdown_signal.await;
    tracing::info!("shutdown signal received, draining requests");

    // mark not ready so k8s diverts new traffic immediately
    health_shutdown.set_shutting_down();

    // notify all tasks to stop
    let _ = shutdown_tx.send(true);

    // wait for servers to drain with timeout
    let timeout = Duration::from_secs(config.server.shutdown_timeout_secs);
    match tokio::time::timeout(timeout, async {
        match http_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "http server error"),
            Err(e) => tracing::error!(error = %e, "http server task panicked"),
        }
        match grpc_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "grpc server error"),
            Err(e) => tracing::error!(error = %e, "grpc server task panicked"),
        }
    })
    .await
    {
        Ok(()) => {}
        Err(_) => tracing::warn!(
            timeout_secs = config.server.shutdown_timeout_secs,
            "shutdown timed out, forcing exit",
        ),
    }

    pool.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}
