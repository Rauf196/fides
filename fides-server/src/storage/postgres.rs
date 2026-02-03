use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Postgres, Transaction as SqlxTransaction};

use crate::domain::account::{Account, AccountId, AccountType};
use crate::domain::entry::{Entry, EntryId, EntryStatus, EntryType};
use crate::domain::money::Amount;
use crate::domain::transaction::{Transaction, TransactionId, TransactionStatus};

use super::StorageError;

/// postgresql storage implementation
pub struct PostgresStorage {
    pool: PgPool,
}

/// type alias for sqlx transaction
pub type Tx<'a> = SqlxTransaction<'a, Postgres>;

impl PostgresStorage {
    /// connect to postgres with the given url
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// create with existing pool (for testing)
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// begin a new transaction
    pub async fn begin(&self) -> Result<Tx<'_>, StorageError> {
        Ok(self.pool.begin().await?)
    }

    /// create a new account, returns the generated id
    pub async fn create_account(
        &self,
        tx: &mut Tx<'_>,
        account_type: AccountType,
        asset_code: &str,
        asset_scale: u8,
        created_at: i64,
    ) -> Result<AccountId, StorageError> {
        let account_type_i16 = account_type_to_i16(account_type);

        let row = sqlx::query!(
            r#"
            INSERT INTO accounts (account_type, asset_code, asset_scale, created_at)
            VALUES ($1, $2, $3, $4)
            RETURNING id
            "#,
            account_type_i16,
            asset_code,
            asset_scale as i16,
            created_at,
        )
        .fetch_one(&mut **tx)
        .await?;

        Ok(AccountId::from_raw(row.id))
    }

    /// get an account by id
    pub async fn get_account(
        &self,
        tx: &mut Tx<'_>,
        id: AccountId,
    ) -> Result<Option<Account>, StorageError> {
        let row = sqlx::query!(
            r#"
            SELECT id, account_type, asset_code, asset_scale, version, created_at
            FROM accounts
            WHERE id = $1
            "#,
            id.value(),
        )
        .fetch_optional(&mut **tx)
        .await?;

        match row {
            Some(r) => {
                let account = build_account(
                    r.id,
                    r.account_type,
                    r.asset_code,
                    r.asset_scale,
                    r.version as i64,
                    r.created_at,
                )?;
                Ok(Some(account))
            }
            None => Ok(None),
        }
    }

    /// get an account with row lock (SELECT FOR UPDATE)
    pub async fn get_account_for_update(
        &self,
        tx: &mut Tx<'_>,
        id: AccountId,
    ) -> Result<Option<Account>, StorageError> {
        let row = sqlx::query!(
            r#"
            SELECT id, account_type, asset_code, asset_scale, version, created_at
            FROM accounts
            WHERE id = $1
            FOR UPDATE
            "#,
            id.value(),
        )
        .fetch_optional(&mut **tx)
        .await?;

        match row {
            Some(r) => {
                let account = build_account(
                    r.id,
                    r.account_type,
                    r.asset_code,
                    r.asset_scale,
                    r.version as i64,
                    r.created_at,
                )?;
                Ok(Some(account))
            }
            None => Ok(None),
        }
    }

    /// increment account version (optimistic locking)
    /// returns error if version doesn't match
    pub async fn increment_account_version(
        &self,
        tx: &mut Tx<'_>,
        id: AccountId,
        expected_version: i64,
    ) -> Result<(), StorageError> {
        let result = sqlx::query!(
            r#"
            UPDATE accounts
            SET version = version + 1
            WHERE id = $1 AND version = $2
            "#,
            id.value(),
            expected_version as i32,
        )
        .execute(&mut **tx)
        .await?;

        if result.rows_affected() == 0 {
            // fetch current version to provide useful error
            let current = sqlx::query_scalar!(
                "SELECT version FROM accounts WHERE id = $1",
                id.value()
            )
            .fetch_optional(&mut **tx)
            .await?;

            match current {
                Some(v) => Err(StorageError::VersionConflict {
                    entity: "account",
                    id: id.value(),
                    expected: expected_version,
                    actual: v as i64,
                }),
                None => Err(StorageError::DataCorruption(format!(
                    "account {} not found during version update",
                    id
                ))),
            }
        } else {
            Ok(())
        }
    }

    /// create a new transaction, returns the generated id
    pub async fn create_transaction(
        &self,
        tx: &mut Tx<'_>,
        idempotency_key: &str,
        status: TransactionStatus,
        metadata: &serde_json::Value,
        created_at: i64,
        posted_at: Option<i64>,
    ) -> Result<TransactionId, StorageError> {
        let status_i16 = transaction_status_to_i16(status);

        let row = sqlx::query!(
            r#"
            INSERT INTO transactions (idempotency_key, status, metadata, created_at, posted_at)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id
            "#,
            idempotency_key,
            status_i16,
            metadata,
            created_at,
            posted_at,
        )
        .fetch_one(&mut **tx)
        .await?;

        Ok(TransactionId::from_raw(row.id))
    }

    /// get a transaction by id
    pub async fn get_transaction(
        &self,
        tx: &mut Tx<'_>,
        id: TransactionId,
    ) -> Result<Option<Transaction>, StorageError> {
        let row = sqlx::query!(
            r#"
            SELECT id, idempotency_key, status, metadata, created_at, posted_at
            FROM transactions
            WHERE id = $1
            "#,
            id.value(),
        )
        .fetch_optional(&mut **tx)
        .await?;

        match row {
            Some(r) => {
                let txn = build_transaction(
                    r.id,
                    r.idempotency_key,
                    r.status,
                    r.metadata,
                    r.created_at,
                    r.posted_at,
                )?;
                Ok(Some(txn))
            }
            None => Ok(None),
        }
    }

    /// find a transaction by idempotency key
    pub async fn find_transaction_by_key(
        &self,
        tx: &mut Tx<'_>,
        idempotency_key: &str,
    ) -> Result<Option<Transaction>, StorageError> {
        let row = sqlx::query!(
            r#"
            SELECT id, idempotency_key, status, metadata, created_at, posted_at
            FROM transactions
            WHERE idempotency_key = $1
            "#,
            idempotency_key,
        )
        .fetch_optional(&mut **tx)
        .await?;

        match row {
            Some(r) => {
                let txn = build_transaction(
                    r.id,
                    r.idempotency_key,
                    r.status,
                    r.metadata,
                    r.created_at,
                    r.posted_at,
                )?;
                Ok(Some(txn))
            }
            None => Ok(None),
        }
    }

    /// update transaction status
    pub async fn update_transaction_status(
        &self,
        tx: &mut Tx<'_>,
        id: TransactionId,
        status: TransactionStatus,
        posted_at: Option<i64>,
    ) -> Result<(), StorageError> {
        let status_i16 = transaction_status_to_i16(status);

        let result = sqlx::query!(
            r#"
            UPDATE transactions
            SET status = $2, posted_at = $3
            WHERE id = $1
            "#,
            id.value(),
            status_i16,
            posted_at,
        )
        .execute(&mut **tx)
        .await?;

        if result.rows_affected() == 0 {
            return Err(StorageError::DataCorruption(format!(
                "transaction {} not found during status update",
                id
            )));
        }

        Ok(())
    }

    /// create a new entry, returns the generated id
    pub async fn create_entry(
        &self,
        tx: &mut Tx<'_>,
        transaction_id: TransactionId,
        account_id: AccountId,
        entry_type: EntryType,
        amount: Amount,
        status: EntryStatus,
        created_at: i64,
    ) -> Result<EntryId, StorageError> {
        let entry_type_i16 = entry_type_to_i16(entry_type);
        let status_i16 = entry_status_to_i16(status);

        let row = sqlx::query!(
            r#"
            INSERT INTO entries (transaction_id, account_id, entry_type, amount, status, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id
            "#,
            transaction_id.value(),
            account_id.value(),
            entry_type_i16,
            amount.value(),
            status_i16,
            created_at,
        )
        .fetch_one(&mut **tx)
        .await?;

        Ok(EntryId::from_raw(row.id))
    }

    /// get entries for a transaction
    pub async fn get_entries_for_transaction(
        &self,
        tx: &mut Tx<'_>,
        transaction_id: TransactionId,
    ) -> Result<Vec<Entry>, StorageError> {
        let rows = sqlx::query!(
            r#"
            SELECT id, transaction_id, account_id, entry_type, amount, status, created_at
            FROM entries
            WHERE transaction_id = $1
            ORDER BY id
            "#,
            transaction_id.value(),
        )
        .fetch_all(&mut **tx)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        for r in rows {
            let entry = build_entry(
                r.id,
                r.transaction_id,
                r.account_id,
                r.entry_type,
                r.amount,
                r.status,
                r.created_at,
            )?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// get entries for an account
    pub async fn get_entries_for_account(
        &self,
        tx: &mut Tx<'_>,
        account_id: AccountId,
    ) -> Result<Vec<Entry>, StorageError> {
        let rows = sqlx::query!(
            r#"
            SELECT id, transaction_id, account_id, entry_type, amount, status, created_at
            FROM entries
            WHERE account_id = $1
            ORDER BY id
            "#,
            account_id.value(),
        )
        .fetch_all(&mut **tx)
        .await?;

        let mut entries = Vec::with_capacity(rows.len());
        for r in rows {
            let entry = build_entry(
                r.id,
                r.transaction_id,
                r.account_id,
                r.entry_type,
                r.amount,
                r.status,
                r.created_at,
            )?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// update entry status for all entries in a transaction
    /// returns the number of rows affected
    pub async fn update_entry_status_by_transaction(
        &self,
        tx: &mut Tx<'_>,
        transaction_id: TransactionId,
        status: EntryStatus,
    ) -> Result<u64, StorageError> {
        let status_i16 = entry_status_to_i16(status);

        let result = sqlx::query!(
            r#"
            UPDATE entries
            SET status = $2
            WHERE transaction_id = $1
            "#,
            transaction_id.value(),
            status_i16,
        )
        .execute(&mut **tx)
        .await?;

        Ok(result.rows_affected())
    }
}

fn account_type_to_i16(t: AccountType) -> i16 {
    match t {
        AccountType::Asset => 1,
        AccountType::Liability => 2,
        AccountType::Equity => 3,
        AccountType::Revenue => 4,
        AccountType::Expense => 5,
    }
}

fn i16_to_account_type(i: i16) -> Result<AccountType, StorageError> {
    match i {
        1 => Ok(AccountType::Asset),
        2 => Ok(AccountType::Liability),
        3 => Ok(AccountType::Equity),
        4 => Ok(AccountType::Revenue),
        5 => Ok(AccountType::Expense),
        _ => Err(StorageError::DataCorruption(format!(
            "invalid account_type: {}",
            i
        ))),
    }
}

fn transaction_status_to_i16(s: TransactionStatus) -> i16 {
    match s {
        TransactionStatus::Pending => 1,
        TransactionStatus::Posted => 2,
        TransactionStatus::Voided => 3,
        TransactionStatus::Failed => 4,
    }
}

fn i16_to_transaction_status(i: i16) -> Result<TransactionStatus, StorageError> {
    match i {
        1 => Ok(TransactionStatus::Pending),
        2 => Ok(TransactionStatus::Posted),
        3 => Ok(TransactionStatus::Voided),
        4 => Ok(TransactionStatus::Failed),
        _ => Err(StorageError::DataCorruption(format!(
            "invalid transaction_status: {}",
            i
        ))),
    }
}

fn entry_type_to_i16(t: EntryType) -> i16 {
    match t {
        EntryType::Debit => 1,
        EntryType::Credit => 2,
    }
}

fn i16_to_entry_type(i: i16) -> Result<EntryType, StorageError> {
    match i {
        1 => Ok(EntryType::Debit),
        2 => Ok(EntryType::Credit),
        _ => Err(StorageError::DataCorruption(format!(
            "invalid entry_type: {}",
            i
        ))),
    }
}

fn entry_status_to_i16(s: EntryStatus) -> i16 {
    match s {
        EntryStatus::Pending => 1,
        EntryStatus::Posted => 2,
        EntryStatus::Voided => 3,
    }
}

fn i16_to_entry_status(i: i16) -> Result<EntryStatus, StorageError> {
    match i {
        1 => Ok(EntryStatus::Pending),
        2 => Ok(EntryStatus::Posted),
        3 => Ok(EntryStatus::Voided),
        _ => Err(StorageError::DataCorruption(format!(
            "invalid entry_status: {}",
            i
        ))),
    }
}

fn build_account(
    id: i64,
    account_type: i16,
    asset_code: String,
    asset_scale: i16,
    version: i64,
    created_at: i64,
) -> Result<Account, StorageError> {
    let account_type = i16_to_account_type(account_type)?;

    Account::new(
        AccountId::from_raw(id),
        account_type,
        asset_code,
        asset_scale as u8,
        version,
        created_at,
    )
    .map_err(|e| StorageError::DataCorruption(format!("invalid account data: {}", e)))
}

fn build_transaction(
    id: i64,
    idempotency_key: String,
    status: i16,
    metadata: serde_json::Value,
    created_at: i64,
    posted_at: Option<i64>,
) -> Result<Transaction, StorageError> {
    let status = i16_to_transaction_status(status)?;

    Transaction::new(
        TransactionId::from_raw(id),
        idempotency_key,
        status,
        metadata,
        created_at,
        posted_at.unwrap_or(0),
    )
    .map_err(|e| StorageError::DataCorruption(format!("invalid transaction data: {}", e)))
}

fn build_entry(
    id: i64,
    transaction_id: i64,
    account_id: i64,
    entry_type: i16,
    amount: i64,
    status: i16,
    created_at: i64,
) -> Result<Entry, StorageError> {
    let entry_type = i16_to_entry_type(entry_type)?;
    let status = i16_to_entry_status(status)?;

    Entry::new(
        EntryId::from_raw(id),
        TransactionId::from_raw(transaction_id),
        AccountId::from_raw(account_id),
        entry_type,
        Amount::from_raw(amount),
        status,
        created_at,
    )
    .map_err(|e| StorageError::DataCorruption(format!("invalid entry data: {}", e)))
}
