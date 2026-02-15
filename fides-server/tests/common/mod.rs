use std::net::SocketAddr;
use std::sync::OnceLock;

use fides_proto::ledger_service_client::LedgerServiceClient;
use tonic::transport::Channel;

use fides_server::config::{
    AppConfig, DatabaseConfig, LoggingConfig, ObservabilityConfig, ServerConfig,
};
use fides_server::server;

/// shared test server — one instance for all gRPC integration tests
///
/// uses a dedicated leaked runtime so the server outlives individual test runtimes.
/// shared to conserve database connections: each server uses 5 connections,
/// and PostgreSQL max_connections defaults to 100.
static TEST_SERVER: OnceLock<TestServer> = OnceLock::new();

pub struct TestServer {
    grpc_addr: SocketAddr,
    http_addr: SocketAddr,
}

impl TestServer {
    async fn start() -> Self {
        dotenvy::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

        let config = AppConfig {
            server: ServerConfig {
                grpc_port: 0,
                http_port: 0,
                shutdown_timeout_secs: 5,
            },
            database: DatabaseConfig {
                url,
                max_connections: 5,
                min_connections: 1,
                acquire_timeout_secs: 5,
                idle_timeout_secs: 600,
            },
            logging: LoggingConfig {
                level: "warn".into(),
                format: "json".into(),
            },
            observability: ObservabilityConfig {
                integrity_check_interval_secs: 3600,
            },
        };

        let pool = server::connect_db(&config)
            .await
            .expect("failed to connect db");
        server::run_migrations(&pool)
            .await
            .expect("failed to run migrations");

        let handle = server::serve(pool, &config)
            .await
            .expect("failed to start server");
        let grpc_addr = handle.grpc_addr;
        let http_addr = handle.http_addr;

        // keep the handle alive for the lifetime of the test process
        Box::leak(Box::new(handle));

        TestServer {
            grpc_addr,
            http_addr,
        }
    }

    pub async fn client(&self) -> LedgerServiceClient<Channel> {
        let endpoint = format!("http://127.0.0.1:{}", self.grpc_addr.port());
        LedgerServiceClient::connect(endpoint)
            .await
            .expect("failed to connect gRPC client")
    }

    pub fn http_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.http_addr.port()))
    }
}

/// get or initialize the shared test server
///
/// the server is started on a dedicated runtime (spawned on a separate thread)
/// to avoid nesting tokio runtimes. the runtime is leaked so its tasks
/// (grpc, http, integrity checker, gauge poll) outlive individual test runtimes.
pub async fn server() -> &'static TestServer {
    TEST_SERVER.get_or_init(|| {
        // spawn on a separate thread to avoid "cannot start a runtime from within a runtime"
        std::thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build server runtime");

            let test_server = rt.block_on(TestServer::start());

            // leak the runtime so the server's spawned tasks keep running
            Box::leak(Box::new(rt));

            test_server
        })
        .join()
        .expect("server init thread panicked")
    })
}
