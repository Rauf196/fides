use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::watch;

use fides_proto::ledger_service_server::LedgerServiceServer;

use crate::config::AppConfig;
use crate::health::HealthState;
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

/// start the ledger server (gRPC + HTTP health), block until shutdown
pub async fn serve(
    pool: PgPool,
    config: &AppConfig,
    shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let storage = Arc::new(PostgresStorage::new(pool.clone()));
    let cache = Arc::new(BalanceCache::new());

    // rehydrate cache from db
    let balances = storage.load_all_balances().await?;
    let count = balances.len();
    cache.rehydrate(balances);
    tracing::info!(accounts = count, "balance cache rehydrated");

    let ledger_service = LedgerService::new(storage, cache);

    // shutdown coordination
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // http server (health probes)
    let http_addr = SocketAddr::from(([0, 0, 0, 0], config.server.http_port));
    let health_state = HealthState::new(pool.clone());
    let health_shutdown = health_state.clone();
    let http_router = crate::health::router(health_state);

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

    // grpc server
    let grpc_addr = SocketAddr::from(([0, 0, 0, 0], config.server.grpc_port));
    let mut grpc_shutdown_rx = shutdown_rx.clone();
    let grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
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

    // notify both servers to stop accepting
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
