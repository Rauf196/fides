use std::fmt;

use super::account::AccountId;
use super::money::Amount;
use super::transaction::TransactionId;

/// unique identifier for an entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(i64);

/// debit or credit
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryType {
    Debit,
    Credit,
}

/// lifecycle state of an entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryStatus {
    Pending,  // authorized but not settled
    Posted,   // settled
    Voided,   // cancelled
}

/// a single leg of a double-entry transaction (immutable once created)
#[derive(Debug, Clone)]
pub struct Entry {
    id: EntryId,
    transaction_id: TransactionId,
    account_id: AccountId,
    entry_type: EntryType,
    amount: Amount,
    status: EntryStatus,
    created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    InvalidId(i64),
    InvalidTimestamp(i64),
    ZeroAmount,
}

impl EntryId {
    pub fn new(id: i64) -> Result<Self, EntryError> {
        if id <= 0 {
            return Err(EntryError::InvalidId(id));
        }
        Ok(EntryId(id))
    }

    pub(crate) fn from_raw(id: i64) -> Self {
        EntryId(id)
    }

    pub fn value(self) -> i64 {
        self.0
    }
}

impl Entry {
    pub fn new(
        id: EntryId,
        transaction_id: TransactionId,
        account_id: AccountId,
        entry_type: EntryType,
        amount: Amount,
        status: EntryStatus,
        created_at: i64,
    ) -> Result<Self, EntryError> {
        if amount.value() == 0 {
            return Err(EntryError::ZeroAmount);
        }
        if created_at <= 0 {
            return Err(EntryError::InvalidTimestamp(created_at));
        }

        Ok(Entry {
            id,
            transaction_id,
            account_id,
            entry_type,
            amount,
            status,
            created_at,
        })
    }

    pub fn id(&self) -> EntryId {
        self.id
    }

    pub fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    pub fn entry_type(&self) -> EntryType {
        self.entry_type
    }

    pub fn amount(&self) -> Amount {
        self.amount
    }

    pub fn status(&self) -> EntryStatus {
        self.status
    }

    pub fn created_at(&self) -> i64 {
        self.created_at
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryError::InvalidId(id) => write!(f, "invalid entry id: {}", id),
            EntryError::InvalidTimestamp(t) => write!(f, "invalid timestamp: {}", t),
            EntryError::ZeroAmount => write!(f, "entry amount cannot be zero"),
        }
    }
}

impl std::error::Error for EntryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::AccountId;
    use crate::domain::money::Amount;
    use crate::domain::transaction::TransactionId;

    fn valid_entry() -> Entry {
        Entry::new(
            EntryId::new(1).unwrap(),
            TransactionId::new(1).unwrap(),
            AccountId::new(1).unwrap(),
            EntryType::Debit,
            Amount::new(1000).unwrap(),
            EntryStatus::Pending,
            1234567890,
        )
        .unwrap()
    }

    #[test]
    fn entry_id_rejects_zero() {
        assert!(EntryId::new(0).is_err());
    }

    #[test]
    fn entry_id_rejects_negative() {
        assert!(EntryId::new(-1).is_err());
    }

    #[test]
    fn entry_id_accepts_positive() {
        assert!(EntryId::new(1).is_ok());
    }

    #[test]
    fn rejects_zero_amount() {
        let result = Entry::new(
            EntryId::new(1).unwrap(),
            TransactionId::new(1).unwrap(),
            AccountId::new(1).unwrap(),
            EntryType::Debit,
            Amount::new(0).unwrap(),
            EntryStatus::Pending,
            1234567890,
        );

        assert!(matches!(result, Err(EntryError::ZeroAmount)));
    }

    #[test]
    fn rejects_zero_timestamp() {
        let result = Entry::new(
            EntryId::new(1).unwrap(),
            TransactionId::new(1).unwrap(),
            AccountId::new(1).unwrap(),
            EntryType::Debit,
            Amount::new(1000).unwrap(),
            EntryStatus::Pending,
            0,
        );

        assert!(matches!(result, Err(EntryError::InvalidTimestamp(0))));
    }

    #[test]
    fn rejects_negative_timestamp() {
        let result = Entry::new(
            EntryId::new(1).unwrap(),
            TransactionId::new(1).unwrap(),
            AccountId::new(1).unwrap(),
            EntryType::Debit,
            Amount::new(1000).unwrap(),
            EntryStatus::Pending,
            -1000,
        );

        assert!(matches!(result, Err(EntryError::InvalidTimestamp(-1000))));
    }

    #[test]
    fn accepts_valid_entry() {
        let entry = valid_entry();
        assert_eq!(entry.amount().value(), 1000);
        assert_eq!(entry.entry_type(), EntryType::Debit);
        assert_eq!(entry.status(), EntryStatus::Pending);
    }

    #[test]
    fn accepts_all_entry_types() {
        for entry_type in [EntryType::Debit, EntryType::Credit] {
            let result = Entry::new(
                EntryId::new(1).unwrap(),
                TransactionId::new(1).unwrap(),
                AccountId::new(1).unwrap(),
                entry_type,
                Amount::new(1000).unwrap(),
                EntryStatus::Posted,
                1234567890,
            );
            assert!(result.is_ok(), "entry_type {:?} should be valid", entry_type);
        }
    }

    #[test]
    fn accepts_all_entry_statuses() {
        for status in [EntryStatus::Pending, EntryStatus::Posted, EntryStatus::Voided] {
            let result = Entry::new(
                EntryId::new(1).unwrap(),
                TransactionId::new(1).unwrap(),
                AccountId::new(1).unwrap(),
                EntryType::Debit,
                Amount::new(1000).unwrap(),
                status,
                1234567890,
            );
            assert!(result.is_ok(), "status {:?} should be valid", status);
        }
    }
}
