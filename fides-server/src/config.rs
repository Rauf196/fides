use std::fmt;
use std::path::Path;

use config::{Config, File, FileFormat};
use serde::Deserialize;

// defaults
const DEFAULT_GRPC_PORT: u16 = 50051;
const DEFAULT_HTTP_PORT: u16 = 9090;
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_CONNECTIONS: u32 = 10;
const DEFAULT_MIN_CONNECTIONS: u32 = 1;
const DEFAULT_ACQUIRE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;
const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_LOG_FORMAT: &str = "json";
const DEFAULT_INTEGRITY_CHECK_INTERVAL_SECS: u64 = 60;

#[derive(Debug)]
pub enum ConfigError {
    Load(config::ConfigError),
    Validation(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub logging: LoggingConfig,
    pub observability: ObservabilityConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub grpc_port: u16,
    pub http_port: u16,
    pub shutdown_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout_secs: u64,
    pub idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    pub integrity_check_interval_secs: u64,
}

impl AppConfig {
    /// load config from defaults, file, env vars, and DATABASE_URL
    ///
    /// precedence (last wins): defaults -> config.toml -> FIDES__* env -> DATABASE_URL env
    pub fn load(config_path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut builder = Config::builder()
            // hardcoded defaults
            .set_default("server.grpc_port", DEFAULT_GRPC_PORT)?
            .set_default("server.http_port", DEFAULT_HTTP_PORT)?
            .set_default(
                "server.shutdown_timeout_secs",
                DEFAULT_SHUTDOWN_TIMEOUT_SECS as i64,
            )?
            .set_default("database.url", "")?
            .set_default("database.max_connections", DEFAULT_MAX_CONNECTIONS as i64)?
            .set_default("database.min_connections", DEFAULT_MIN_CONNECTIONS as i64)?
            .set_default(
                "database.acquire_timeout_secs",
                DEFAULT_ACQUIRE_TIMEOUT_SECS as i64,
            )?
            .set_default(
                "database.idle_timeout_secs",
                DEFAULT_IDLE_TIMEOUT_SECS as i64,
            )?
            .set_default("logging.level", DEFAULT_LOG_LEVEL)?
            .set_default("logging.format", DEFAULT_LOG_FORMAT)?
            .set_default(
                "observability.integrity_check_interval_secs",
                DEFAULT_INTEGRITY_CHECK_INTERVAL_SECS as i64,
            )?;

        // config file: --config makes it required, otherwise optional config.toml
        match config_path {
            Some(path) => {
                builder = builder.add_source(File::from(path.to_path_buf()).required(true));
            }
            None => {
                builder = builder.add_source(File::new("config", FileFormat::Toml).required(false));
            }
        }

        // FIDES__* env vars (double underscore for nesting)
        builder = builder.add_source(
            config::Environment::with_prefix("FIDES")
                .separator("__")
                .try_parsing(true),
        );

        // DATABASE_URL override (highest priority for the url field)
        if let Ok(url) = std::env::var("DATABASE_URL") {
            builder = builder.set_override("database.url", url)?;
        }

        let config: AppConfig = builder.build()?.try_deserialize()?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.database.url.is_empty() {
            return Err(ConfigError::Validation("database.url is required".into()));
        }

        if self.server.grpc_port == self.server.http_port {
            return Err(ConfigError::Validation(format!(
                "grpc_port and http_port must differ, both set to {}",
                self.server.grpc_port,
            )));
        }

        if self.database.max_connections == 0 {
            return Err(ConfigError::Validation(
                "database.max_connections must be > 0".into(),
            ));
        }

        if self.database.min_connections > self.database.max_connections {
            return Err(ConfigError::Validation(format!(
                "database.min_connections ({}) exceeds max_connections ({})",
                self.database.min_connections, self.database.max_connections,
            )));
        }

        if self.server.shutdown_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "server.shutdown_timeout_secs must be > 0".into(),
            ));
        }

        if self.observability.integrity_check_interval_secs < 10 {
            return Err(ConfigError::Validation(format!(
                "observability.integrity_check_interval_secs must be >= 10, got {}",
                self.observability.integrity_check_interval_secs,
            )));
        }

        Ok(())
    }

    /// log config summary with redacted database url
    pub fn log_summary(&self) {
        tracing::info!(
            grpc_port = self.server.grpc_port,
            http_port = self.server.http_port,
            shutdown_timeout_secs = self.server.shutdown_timeout_secs,
            database_url = %redact_db_url(&self.database.url),
            max_connections = self.database.max_connections,
            min_connections = self.database.min_connections,
            log_level = %self.logging.level,
            log_format = %self.logging.format,
            integrity_check_interval_secs = self.observability.integrity_check_interval_secs,
            "configuration loaded",
        );
    }
}

/// replace password in database url with ***
///
/// handles `://user:password@host` pattern
fn redact_db_url(url: &str) -> String {
    // find :// then look for : after user and @ after password
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };

    let after_scheme = scheme_end + 3;
    let rest = &url[after_scheme..];

    // find @ to determine if there are credentials
    let Some(at_pos) = rest.find('@') else {
        return url.to_string();
    };

    let credentials = &rest[..at_pos];

    // find : separating user from password
    let Some(colon_pos) = credentials.find(':') else {
        return url.to_string();
    };

    let user = &credentials[..colon_pos];
    let after_at = &rest[at_pos..];

    format!("{}://{}:***{}", &url[..scheme_end], user, after_at)
}

impl From<config::ConfigError> for ConfigError {
    fn from(e: config::ConfigError) -> Self {
        ConfigError::Load(e)
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Load(e) => write!(f, "configuration error: {}", e),
            ConfigError::Validation(msg) => write!(f, "configuration error: {}", msg),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Load(e) => Some(e),
            ConfigError::Validation(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_with_password() {
        assert_eq!(
            redact_db_url("postgres://user:secret@localhost/fides"),
            "postgres://user:***@localhost/fides",
        );
    }

    #[test]
    fn redact_url_with_password_and_port() {
        assert_eq!(
            redact_db_url("postgres://admin:p4ss@db.host:5432/fides"),
            "postgres://admin:***@db.host:5432/fides",
        );
    }

    #[test]
    fn redact_url_without_password() {
        let url = "postgres://localhost/fides";
        assert_eq!(redact_db_url(url), url);
    }

    #[test]
    fn redact_url_user_only_no_colon() {
        let url = "postgres://user@localhost/fides";
        assert_eq!(redact_db_url(url), url);
    }

    #[test]
    fn redact_url_no_scheme() {
        let url = "localhost/fides";
        assert_eq!(redact_db_url(url), url);
    }

    #[test]
    fn validate_empty_url() {
        let config = test_config(|c| c.database.url = String::new());
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("database.url is required"));
    }

    #[test]
    fn validate_same_ports() {
        let config = test_config(|c| {
            c.server.grpc_port = 9090;
            c.server.http_port = 9090;
        });
        let err = config.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("grpc_port and http_port must differ"));
    }

    #[test]
    fn validate_min_exceeds_max_connections() {
        let config = test_config(|c| {
            c.database.min_connections = 20;
            c.database.max_connections = 10;
        });
        let err = config.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("min_connections (20) exceeds max_connections (10)"));
    }

    #[test]
    fn validate_zero_max_connections() {
        let config = test_config(|c| c.database.max_connections = 0);
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_connections must be > 0"));
    }

    #[test]
    fn validate_zero_shutdown_timeout() {
        let config = test_config(|c| c.server.shutdown_timeout_secs = 0);
        let err = config.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("shutdown_timeout_secs must be > 0"));
    }

    #[test]
    fn validate_valid_config() {
        let config = test_config(|_| {});
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_low_integrity_interval() {
        let config = test_config(|c| c.observability.integrity_check_interval_secs = 5);
        let err = config.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("integrity_check_interval_secs must be >= 10"));
    }

    fn test_config(modify: impl FnOnce(&mut AppConfig)) -> AppConfig {
        let mut config = AppConfig {
            server: ServerConfig {
                grpc_port: DEFAULT_GRPC_PORT,
                http_port: DEFAULT_HTTP_PORT,
                shutdown_timeout_secs: DEFAULT_SHUTDOWN_TIMEOUT_SECS,
            },
            database: DatabaseConfig {
                url: "postgres://localhost/fides".into(),
                max_connections: DEFAULT_MAX_CONNECTIONS,
                min_connections: DEFAULT_MIN_CONNECTIONS,
                acquire_timeout_secs: DEFAULT_ACQUIRE_TIMEOUT_SECS,
                idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            },
            logging: LoggingConfig {
                level: DEFAULT_LOG_LEVEL.into(),
                format: DEFAULT_LOG_FORMAT.into(),
            },
            observability: ObservabilityConfig {
                integrity_check_interval_secs: DEFAULT_INTEGRITY_CHECK_INTERVAL_SECS,
            },
        };
        modify(&mut config);
        config
    }
}
