pub mod ledger;

use std::fmt;

use crate::domain::account::AccountError;
use crate::domain::entry::EntryError;
use crate::domain::money::AmountError;
use crate::domain::transaction::{TransactionError, TransactionStatus};
use crate::domain::validation::{TransferLegError, ValidationError};
use crate::storage::StorageError;

/// service-level errors with context for gRPC mapping
#[derive(Debug)]
pub enum ServiceError {
    /// account not found
    AccountNotFound { account_id: i64 },

    /// transaction not found
    TransactionNotFound { transaction_id: i64 },

    /// insufficient available balance
    InsufficientFunds {
        account_id: i64,
        available: i64,
        requested: i64,
    },

    /// transaction already in terminal state
    InvalidTransactionState {
        transaction_id: i64,
        current: TransactionStatus,
        attempted: &'static str,
    },

    /// duplicate idempotency key (not an error for same operation)
    DuplicateTransaction { idempotency_key: String },

    /// unbalanced transaction (debits != credits)
    UnbalancedTransaction {
        total_debits: i64,
        total_credits: i64,
    },

    /// invalid request data
    InvalidArgument(String),

    /// optimistic locking conflict - retry
    VersionConflict { entity: &'static str, id: i64 },

    /// internal error (db failure, data corruption)
    Internal(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceError::AccountNotFound { account_id } => {
                write!(f, "account {} not found", account_id)
            }
            ServiceError::TransactionNotFound { transaction_id } => {
                write!(f, "transaction {} not found", transaction_id)
            }
            ServiceError::InsufficientFunds {
                account_id,
                available,
                requested,
            } => {
                write!(
                    f,
                    "insufficient funds on account {}: available {}, requested {}",
                    account_id, available, requested
                )
            }
            ServiceError::InvalidTransactionState {
                transaction_id,
                current,
                attempted,
            } => {
                write!(
                    f,
                    "cannot {} transaction {}: current state {:?}",
                    attempted, transaction_id, current
                )
            }
            ServiceError::DuplicateTransaction { idempotency_key } => {
                write!(f, "duplicate transaction with key {}", idempotency_key)
            }
            ServiceError::UnbalancedTransaction {
                total_debits,
                total_credits,
            } => {
                write!(
                    f,
                    "unbalanced transaction: debits {} != credits {}",
                    total_debits, total_credits
                )
            }
            ServiceError::InvalidArgument(msg) => write!(f, "invalid argument: {}", msg),
            ServiceError::VersionConflict { entity, id } => {
                write!(f, "version conflict on {} {}, retry", entity, id)
            }
            ServiceError::Internal(msg) => write!(f, "internal error: {}", msg),
        }
    }
}

impl std::error::Error for ServiceError {}

// conversions from domain/storage errors

impl From<StorageError> for ServiceError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::VersionConflict { entity, id, .. } => {
                ServiceError::VersionConflict { entity, id }
            }
            StorageError::DuplicateKey(key) => ServiceError::DuplicateTransaction {
                idempotency_key: key,
            },
            StorageError::DataCorruption(msg) => ServiceError::Internal(msg),
            StorageError::Database(e) => ServiceError::Internal(e.to_string()),
        }
    }
}

impl From<ValidationError> for ServiceError {
    fn from(e: ValidationError) -> Self {
        match e {
            ValidationError::InsufficientLegs { count } => ServiceError::InvalidArgument(format!(
                "transaction requires at least 2 legs, got {}",
                count
            )),
            ValidationError::UnbalancedTransaction {
                total_debits,
                total_credits,
            } => ServiceError::UnbalancedTransaction {
                total_debits,
                total_credits,
            },
            ValidationError::SumOverflow | ValidationError::BalanceOverflow => {
                ServiceError::Internal(e.to_string())
            }
        }
    }
}

impl From<TransferLegError> for ServiceError {
    fn from(e: TransferLegError) -> Self {
        ServiceError::InvalidArgument(e.to_string())
    }
}

impl From<AmountError> for ServiceError {
    fn from(e: AmountError) -> Self {
        ServiceError::InvalidArgument(e.to_string())
    }
}

impl From<AccountError> for ServiceError {
    fn from(e: AccountError) -> Self {
        ServiceError::InvalidArgument(e.to_string())
    }
}

impl From<EntryError> for ServiceError {
    fn from(e: EntryError) -> Self {
        ServiceError::InvalidArgument(e.to_string())
    }
}

impl From<TransactionError> for ServiceError {
    fn from(e: TransactionError) -> Self {
        ServiceError::InvalidArgument(e.to_string())
    }
}

// gRPC status mapping

impl From<ServiceError> for tonic::Status {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::AccountNotFound { .. } | ServiceError::TransactionNotFound { .. } => {
                tonic::Status::not_found(e.to_string())
            }
            ServiceError::InsufficientFunds { .. } => {
                tonic::Status::failed_precondition(e.to_string())
            }
            ServiceError::InvalidTransactionState { .. } => {
                tonic::Status::failed_precondition(e.to_string())
            }
            ServiceError::DuplicateTransaction { .. } => {
                tonic::Status::already_exists(e.to_string())
            }
            ServiceError::UnbalancedTransaction { .. } | ServiceError::InvalidArgument(_) => {
                tonic::Status::invalid_argument(e.to_string())
            }
            ServiceError::VersionConflict { .. } => tonic::Status::aborted(e.to_string()),
            ServiceError::Internal(ref msg) => {
                // log full details server-side, return generic message to client
                tracing::error!(error = %msg, "internal error");
                tonic::Status::internal("internal error")
            }
        }
    }
}
