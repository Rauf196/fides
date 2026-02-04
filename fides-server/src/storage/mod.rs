pub mod cache;
pub mod postgres;

pub use cache::BalanceCache;

use std::fmt;

/// errors that can occur in the storage layer
#[derive(Debug)]
pub enum StorageError {
    /// database connection or query failed
    Database(sqlx::Error),

    /// data in database violated domain invariants (should never happen)
    DataCorruption(String),

    /// optimistic locking conflict - version mismatch
    VersionConflict {
        entity: &'static str,
        id: i64,
        expected: i64,
        actual: i64,
    },

    /// duplicate idempotency key
    DuplicateKey(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Database(e) => write!(f, "database error: {}", e),
            StorageError::DataCorruption(msg) => write!(f, "data corruption: {}", msg),
            StorageError::VersionConflict { entity, id, expected, actual } => {
                write!(
                    f,
                    "version conflict on {} {}: expected {}, got {}",
                    entity, id, expected, actual
                )
            }
            StorageError::DuplicateKey(key) => write!(f, "duplicate idempotency key: {}", key),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Database(e) => Some(e),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for StorageError {
    fn from(e: sqlx::Error) -> Self {
        // check for unique constraint violation (duplicate key)
        if let sqlx::Error::Database(ref db_err) = e {
            if db_err.is_unique_violation() {
                // extract key if possible, otherwise use generic message
                let msg = db_err.message().to_string();
                return StorageError::DuplicateKey(msg);
            }
        }
        StorageError::Database(e)
    }
}
