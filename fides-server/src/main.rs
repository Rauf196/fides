use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use fides_server::config::{AppConfig, LoggingConfig};
use fides_server::server;

#[derive(Parser)]
#[command(name = "fides", about = "production-grade double-entry ledger")]
struct Cli {
    /// path to config file (default: config.toml if present)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// run database migrations and exit
    Migrate,
    /// start the ledger server
    Serve {
        /// run migrations before starting the server
        #[arg(long)]
        run_migrations: bool,
    },
}

fn main() {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    let config = match AppConfig::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fatal: {}", e);
            std::process::exit(1);
        }
    };

    init_tracing(&config.logging);
    config.log_summary();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(e) = runtime.block_on(run(cli.command, config)) {
        tracing::error!(error = %e, "fatal error");
        std::process::exit(1);
    }
}

fn init_tracing(logging: &LoggingConfig) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&logging.level));

    if logging.format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .pretty()
            .init();
    }
}

async fn run(command: Command, config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let pool = server::connect_db(&config).await?;

    match command {
        Command::Migrate => {
            server::run_migrations(&pool).await?;
            pool.close().await;
            Ok(())
        }
        Command::Serve { run_migrations } => {
            if run_migrations {
                server::run_migrations(&pool).await?;
            }
            server::serve(pool, &config, os_shutdown_signal()).await
        }
    }
}

async fn os_shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("failed to listen for ctrl+c");
    }
}
