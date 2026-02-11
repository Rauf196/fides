use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use metrics::gauge;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;

use fides_proto::ledger_service_server::LedgerServiceServer;

use crate::config::AppConfig;
use crate::health::HealthState;
use crate::observability::grpc_metrics::GrpcMetricsLayer;
use crate::observability::integrity::IntegrityChecker;
use crate::service::ledger::LedgerService;
use crate::storage::postgres::PostgresStorage;
use crate::storage::BalanceCache;

/// handle to a running server, returned by serve()
///
/// holds the bound addresses and join handles for both servers.
/// use `run()` for production (waits for shutdown signal) or
/// `shutdown()` for tests (triggers shutdown immediately).
pub struct ServerHandle {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    grpc_handle: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    http_handle: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    health_state: HealthState,
    pool: PgPool,
    shutdown_timeout: Duration,
}

impl ServerHandle {
    /// production path: wait for the signal, then graceful shutdown
    pub async fn run(self, signal: impl std::future::Future<Output = ()> + Send + 'static) -> Result<(), Box<dyn std::error::Error>> {
        signal.await;
        tracing::info!("shutdown signal received, draining requests");
        self.drain().await
    }

    /// test path: trigger shutdown immediately
    pub async fn shutdown(self) -> Result<(), Box<dyn std::error::Error>> {
        self.drain().await
    }

    async fn drain(self) -> Result<(), Box<dyn std::error::Error>> {
        self.health_state.set_shutting_down();
        let _ = self.shutdown_tx.send(true);

        match tokio::time::timeout(self.shutdown_timeout, async {
            match self.http_handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!(error = %e, "http server error"),
                Err(e) => tracing::error!(error = %e, "http server task panicked"),
            }
            match self.grpc_handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!(error = %e, "grpc server error"),
                Err(e) => tracing::error!(error = %e, "grpc server task panicked"),
            }
        })
        .await
        {
            Ok(()) => {}
            Err(_) => tracing::warn!(
                timeout = ?self.shutdown_timeout,
                "shutdown timed out, forcing exit",
            ),
        }

        self.pool.close().await;
        tracing::info!("shutdown complete");
        Ok(())
    }
}

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

/// start the ledger server (gRPC + HTTP health/metrics)
///
/// binds both listeners before spawning tasks — the returned handle
/// contains real addresses (critical for port 0 in tests).
/// server is accepting connections when this function returns.
pub async fn serve(
    pool: PgPool,
    config: &AppConfig,
) -> Result<ServerHandle, Box<dyn std::error::Error>> {
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

    // bind listeners before spawning — get real addresses
    let http_addr = SocketAddr::from(([0, 0, 0, 0], config.server.http_port));
    let http_listener = tokio::net::TcpListener::bind(http_addr).await?;
    let http_addr = http_listener.local_addr()?;

    let grpc_addr = SocketAddr::from(([0, 0, 0, 0], config.server.grpc_port));
    let grpc_listener = tokio::net::TcpListener::bind(grpc_addr).await?;
    let grpc_addr = grpc_listener.local_addr()?;

    // http server (health probes + metrics endpoint)
    let health_state = HealthState::new(pool.clone());
    let http_router = crate::health::router(health_state.clone())
        .merge(crate::observability::metrics::router(metrics_handle));

    let mut http_shutdown_rx = shutdown_rx.clone();
    let http_handle = tokio::spawn(async move {
        axum::serve(http_listener, http_router)
            .with_graceful_shutdown(async move {
                let _ = http_shutdown_rx.wait_for(|v| *v).await;
            })
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    // grpc server (with metrics layer)
    let mut grpc_shutdown_rx = shutdown_rx.clone();
    let grpc_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .layer(GrpcMetricsLayer)
            .add_service(LedgerServiceServer::new(ledger_service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(grpc_listener), async move {
                let _ = grpc_shutdown_rx.wait_for(|v| *v).await;
            })
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    let shutdown_timeout = Duration::from_secs(config.server.shutdown_timeout_secs);

    tracing::info!(
        grpc = %grpc_addr,
        http = %http_addr,
        "fides ready",
    );

    Ok(ServerHandle {
        grpc_addr,
        http_addr,
        shutdown_tx,
        grpc_handle,
        http_handle,
        health_state,
        pool,
        shutdown_timeout,
    })
}
