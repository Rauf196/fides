use serde_json::Value as JsonValue;
use std::fmt;

/// unique identifier for a transaction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(i64);

/// lifecycle state of a transaction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionStatus {
    Pending,  // authorized, awaiting capture/void
    Posted,   // captured, finalized
    Voided,   // cancelled
    Failed,   // rejected (validation failed, insufficient funds, etc.)
}

/// groups entries that must balance (total debits = total credits)
#[derive(Debug, Clone)]
pub struct Transaction {
    id: TransactionId,
    idempotency_key: String,
    status: TransactionStatus,
    metadata: JsonValue,  // audit info: client_ip, merchant_id, correlation_id, etc.
    created_at: i64,
    posted_at: i64,  // 0 if not yet posted
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionError {
    InvalidId(i64),
    EmptyIdempotencyKey,
    InvalidTimestamp(i64),
    InvalidPostedAt { status: TransactionStatus, posted_at: i64 },
}

impl TransactionId {
    pub fn new(id: i64) -> Result<Self, TransactionError> {
        if id <= 0 {
            return Err(TransactionError::InvalidId(id));
        }
        Ok(TransactionId(id))
    }

    pub(crate) fn from_raw(id: i64) -> Self {
        TransactionId(id)
    }

    pub fn value(self) -> i64 {
        self.0
    }
}

impl TransactionStatus {
    /// returns true if this is a terminal state (no further transitions)
    pub fn is_terminal(self) -> bool {
        matches!(self, TransactionStatus::Posted | TransactionStatus::Voided | TransactionStatus::Failed)
    }
}

impl Transaction {
    pub fn new(
        id: TransactionId,
        idempotency_key: String,
        status: TransactionStatus,
        metadata: JsonValue,
        created_at: i64,
        posted_at: i64,
    ) -> Result<Self, TransactionError> {
        if idempotency_key.is_empty() {
            return Err(TransactionError::EmptyIdempotencyKey);
        }
        if created_at <= 0 {
            return Err(TransactionError::InvalidTimestamp(created_at));
        }

        // posted_at consistency: must be > 0 iff status is Posted
        match status {
            TransactionStatus::Posted if posted_at <= 0 => {
                return Err(TransactionError::InvalidPostedAt { status, posted_at });
            }
            TransactionStatus::Pending | TransactionStatus::Voided | TransactionStatus::Failed
                if posted_at != 0 =>
            {
                return Err(TransactionError::InvalidPostedAt { status, posted_at });
            }
            _ => {}
        }

        Ok(Transaction {
            id,
            idempotency_key,
            status,
            metadata,
            created_at,
            posted_at,
        })
    }

    pub fn id(&self) -> TransactionId {
        self.id
    }

    pub fn idempotency_key(&self) -> &str {
        &self.idempotency_key
    }

    pub fn status(&self) -> TransactionStatus {
        self.status
    }

    pub fn metadata(&self) -> &JsonValue {
        &self.metadata
    }

    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    pub fn posted_at(&self) -> i64 {
        self.posted_at
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransactionError::InvalidId(id) => write!(f, "invalid transaction id: {}", id),
            TransactionError::EmptyIdempotencyKey => write!(f, "idempotency key cannot be empty"),
            TransactionError::InvalidTimestamp(t) => write!(f, "invalid timestamp: {}", t),
            TransactionError::InvalidPostedAt { status, posted_at } => {
                write!(f, "invalid posted_at {} for status {:?}", posted_at, status)
            }
        }
    }
}

impl std::error::Error for TransactionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_metadata() -> JsonValue {
        json!({
            "client_ip": "192.168.1.1",
            "merchant_id": "merchant-123"
        })
    }

    #[test]
    fn transaction_id_rejects_zero() {
        assert!(TransactionId::new(0).is_err());
    }

    #[test]
    fn transaction_id_rejects_negative() {
        assert!(TransactionId::new(-1).is_err());
    }

    #[test]
    fn transaction_id_accepts_positive() {
        assert!(TransactionId::new(1).is_ok());
    }

    #[test]
    fn rejects_empty_idempotency_key() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "".to_string(),
            TransactionStatus::Pending,
            test_metadata(),
            1234567890,
            0,
        );

        assert!(matches!(result, Err(TransactionError::EmptyIdempotencyKey)));
    }

    #[test]
    fn rejects_invalid_timestamp() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Pending,
            test_metadata(),
            0,
            0,
        );

        assert!(matches!(result, Err(TransactionError::InvalidTimestamp(0))));
    }

    #[test]
    fn rejects_posted_without_posted_at() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Posted,
            test_metadata(),
            1234567890,
            0,  // invalid: posted status requires posted_at > 0
        );

        assert!(matches!(
            result,
            Err(TransactionError::InvalidPostedAt { status: TransactionStatus::Posted, .. })
        ));
    }

    #[test]
    fn rejects_pending_with_posted_at() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Pending,
            test_metadata(),
            1234567890,
            1234567890,  // invalid: pending shouldn't have posted_at
        );

        assert!(matches!(
            result,
            Err(TransactionError::InvalidPostedAt { status: TransactionStatus::Pending, .. })
        ));
    }

    #[test]
    fn rejects_voided_with_posted_at() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Voided,
            test_metadata(),
            1234567890,
            1234567890,  // invalid: voided shouldn't have posted_at
        );

        assert!(matches!(
            result,
            Err(TransactionError::InvalidPostedAt { status: TransactionStatus::Voided, .. })
        ));
    }

    #[test]
    fn rejects_failed_with_posted_at() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Failed,
            test_metadata(),
            1234567890,
            1234567890,  // invalid: failed shouldn't have posted_at
        );

        assert!(matches!(
            result,
            Err(TransactionError::InvalidPostedAt { status: TransactionStatus::Failed, .. })
        ));
    }

    #[test]
    fn accepts_valid_pending_transaction() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Pending,
            test_metadata(),
            1234567890,
            0,
        );

        assert!(result.is_ok());
        let tx = result.unwrap();
        assert_eq!(tx.status(), TransactionStatus::Pending);
        assert_eq!(tx.posted_at(), 0);
    }

    #[test]
    fn accepts_valid_posted_transaction() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Posted,
            test_metadata(),
            1234567890,
            1234567900,
        );

        assert!(result.is_ok());
        let tx = result.unwrap();
        assert_eq!(tx.status(), TransactionStatus::Posted);
        assert!(tx.posted_at() > 0);
    }

    #[test]
    fn accepts_valid_voided_transaction() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Voided,
            test_metadata(),
            1234567890,
            0,  // voided has no posted_at
        );

        assert!(result.is_ok());
        let tx = result.unwrap();
        assert_eq!(tx.status(), TransactionStatus::Voided);
        assert_eq!(tx.posted_at(), 0);
    }

    #[test]
    fn accepts_valid_failed_transaction() {
        let result = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Failed,
            test_metadata(),
            1234567890,
            0,  // failed has no posted_at
        );

        assert!(result.is_ok());
        let tx = result.unwrap();
        assert_eq!(tx.status(), TransactionStatus::Failed);
        assert_eq!(tx.posted_at(), 0);
    }

    #[test]
    fn status_terminal_states() {
        assert!(!TransactionStatus::Pending.is_terminal());
        assert!(TransactionStatus::Posted.is_terminal());
        assert!(TransactionStatus::Voided.is_terminal());
        assert!(TransactionStatus::Failed.is_terminal());
    }

    #[test]
    fn metadata_is_accessible() {
        let metadata = json!({"correlation_id": "abc-123"});
        let tx = Transaction::new(
            TransactionId::new(1).unwrap(),
            "key-123".to_string(),
            TransactionStatus::Pending,
            metadata.clone(),
            1234567890,
            0,
        )
        .unwrap();

        assert_eq!(tx.metadata(), &metadata);
        assert_eq!(tx.metadata()["correlation_id"], "abc-123");
    }
}
