//! Integration tests for PostgresStorage
//!
//! These tests require a running PostgreSQL database with the schema applied.
//! Set DATABASE_URL environment variable to connect.
//!
//! Each test runs in a transaction that is rolled back, ensuring test isolation.

use fides_server::domain::account::AccountType;
use fides_server::domain::entry::{EntryStatus, EntryType};
use fides_server::domain::money::Amount;
use fides_server::domain::transaction::TransactionStatus;
use fides_server::storage::postgres::PostgresStorage;
use fides_server::storage::StorageError;
use serde_json::json;

async fn setup() -> PostgresStorage {
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    PostgresStorage::connect(&url)
        .await
        .expect("failed to connect to database")
}

fn test_metadata() -> serde_json::Value {
    json!({
        "client_ip": "192.168.1.100",
        "merchant_id": "test-merchant",
        "correlation_id": "test-correlation-123"
    })
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn create_and_get_account() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let account = storage
        .get_account(&mut tx, account_id)
        .await
        .unwrap()
        .expect("account should exist");

    assert_eq!(account.id(), account_id);
    assert_eq!(account.account_type(), AccountType::Asset);
    assert_eq!(account.asset_code(), "USD");
    assert_eq!(account.asset_scale(), 2);
    assert_eq!(account.version(), 0);

    // rollback - don't commit
}

#[tokio::test]
async fn get_nonexistent_account_returns_none() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let fake_id = fides_server::domain::account::AccountId::new(999999).unwrap();
    let result = storage.get_account(&mut tx, fake_id).await.unwrap();

    assert!(result.is_none());
}

#[tokio::test]
async fn get_account_for_update_locks_row() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Liability, "EUR", 2, now_millis())
        .await
        .unwrap();

    let account = storage
        .get_account_for_update(&mut tx, account_id)
        .await
        .unwrap()
        .expect("account should exist");

    assert_eq!(account.id(), account_id);
    assert_eq!(account.account_type(), AccountType::Liability);
}

#[tokio::test]
async fn increment_account_version_succeeds() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    // version starts at 0
    storage
        .increment_account_version(&mut tx, account_id, 0)
        .await
        .unwrap();

    let account = storage
        .get_account(&mut tx, account_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(account.version(), 1);
}

#[tokio::test]
async fn increment_account_version_fails_on_mismatch() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    // version is 0, but we expect 5
    let result = storage
        .increment_account_version(&mut tx, account_id, 5)
        .await;

    match result {
        Err(StorageError::VersionConflict {
            entity,
            expected,
            actual,
            ..
        }) => {
            assert_eq!(entity, "account");
            assert_eq!(expected, 5);
            assert_eq!(actual, 0);
        }
        other => panic!("expected VersionConflict, got {:?}", other),
    }
}

#[tokio::test]
async fn create_and_get_transaction() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let txn_id = storage
        .create_transaction(
            &mut tx,
            "idem-key-001",
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    let txn = storage
        .get_transaction(&mut tx, txn_id)
        .await
        .unwrap()
        .expect("transaction should exist");

    assert_eq!(txn.id(), txn_id);
    assert_eq!(txn.idempotency_key(), "idem-key-001");
    assert_eq!(txn.status(), TransactionStatus::Pending);
    assert_eq!(txn.metadata()["merchant_id"], "test-merchant");
}

#[tokio::test]
async fn find_transaction_by_idempotency_key() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let key = format!("unique-key-{}", now_millis());

    storage
        .create_transaction(
            &mut tx,
            &key,
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    let found = storage
        .find_transaction_by_key(&mut tx, &key)
        .await
        .unwrap()
        .expect("should find by key");

    assert_eq!(found.idempotency_key(), key);
}

#[tokio::test]
async fn find_nonexistent_transaction_returns_none() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let result = storage
        .find_transaction_by_key(&mut tx, "nonexistent-key")
        .await
        .unwrap();

    assert!(result.is_none());
}

#[tokio::test]
async fn duplicate_idempotency_key_fails() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let key = format!("dup-key-{}", now_millis());

    storage
        .create_transaction(
            &mut tx,
            &key,
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    let result = storage
        .create_transaction(
            &mut tx,
            &key, // same key
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await;

    assert!(matches!(result, Err(StorageError::DuplicateKey(_))));
}

#[tokio::test]
async fn update_transaction_status_to_posted() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("post-key-{}", now_millis()),
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    let posted_at = now_millis();
    storage
        .update_transaction_status(&mut tx, txn_id, TransactionStatus::Posted, Some(posted_at))
        .await
        .unwrap();

    let txn = storage
        .get_transaction(&mut tx, txn_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(txn.status(), TransactionStatus::Posted);
    assert_eq!(txn.posted_at(), posted_at);
}

#[tokio::test]
async fn update_transaction_status_to_voided() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("void-key-{}", now_millis()),
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    storage
        .update_transaction_status(&mut tx, txn_id, TransactionStatus::Voided, None)
        .await
        .unwrap();

    let txn = storage
        .get_transaction(&mut tx, txn_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(txn.status(), TransactionStatus::Voided);
    assert_eq!(txn.posted_at(), 0);
}

#[tokio::test]
async fn create_and_get_entries_for_transaction() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    // create account and transaction first
    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("entry-key-{}", now_millis()),
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    // create entries
    let debit_id = storage
        .create_entry(
            &mut tx,
            txn_id,
            account_id,
            EntryType::Debit,
            Amount::new(10000).unwrap(),
            EntryStatus::Pending,
            now_millis(),
        )
        .await
        .unwrap();

    let credit_id = storage
        .create_entry(
            &mut tx,
            txn_id,
            account_id,
            EntryType::Credit,
            Amount::new(10000).unwrap(),
            EntryStatus::Pending,
            now_millis(),
        )
        .await
        .unwrap();

    // fetch entries
    let entries = storage
        .get_entries_for_transaction(&mut tx, txn_id)
        .await
        .unwrap();

    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|e| e.id() == debit_id));
    assert!(entries.iter().any(|e| e.id() == credit_id));
}

#[tokio::test]
async fn get_entries_for_account() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("acc-entry-key-{}", now_millis()),
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    storage
        .create_entry(
            &mut tx,
            txn_id,
            account_id,
            EntryType::Debit,
            Amount::new(5000).unwrap(),
            EntryStatus::Pending,
            now_millis(),
        )
        .await
        .unwrap();

    let entries = storage
        .get_entries_for_account(&mut tx, account_id)
        .await
        .unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].amount().value(), 5000);
}

#[tokio::test]
async fn update_entry_status_by_transaction() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("status-key-{}", now_millis()),
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    // create 3 entries
    for _ in 0..3 {
        storage
            .create_entry(
                &mut tx,
                txn_id,
                account_id,
                EntryType::Debit,
                Amount::new(1000).unwrap(),
                EntryStatus::Pending,
                now_millis(),
            )
            .await
            .unwrap();
    }

    // update all to posted
    let affected = storage
        .update_entry_status_by_transaction(&mut tx, txn_id, EntryStatus::Posted)
        .await
        .unwrap();

    assert_eq!(affected, 3);

    // verify
    let entries = storage
        .get_entries_for_transaction(&mut tx, txn_id)
        .await
        .unwrap();

    assert!(entries.iter().all(|e| e.status() == EntryStatus::Posted));
}

#[tokio::test]
async fn update_entry_status_returns_correct_count() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("count-key-{}", now_millis()),
            TransactionStatus::Pending,
            &test_metadata(),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    // create 5 entries
    for _ in 0..5 {
        storage
            .create_entry(
                &mut tx,
                txn_id,
                account_id,
                EntryType::Credit,
                Amount::new(100).unwrap(),
                EntryStatus::Pending,
                now_millis(),
            )
            .await
            .unwrap();
    }

    let affected = storage
        .update_entry_status_by_transaction(&mut tx, txn_id, EntryStatus::Voided)
        .await
        .unwrap();

    // verify count matches what we created
    assert_eq!(affected, 5);
}

#[tokio::test]
async fn full_authorize_capture_workflow() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    // setup: create two accounts
    let debit_account = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let credit_account = storage
        .create_account(&mut tx, AccountType::Liability, "USD", 2, now_millis())
        .await
        .unwrap();

    // phase 1: authorize
    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("workflow-{}", now_millis()),
            TransactionStatus::Pending,
            &json!({"description": "test payment"}),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    storage
        .create_entry(
            &mut tx,
            txn_id,
            debit_account,
            EntryType::Debit,
            Amount::new(50000).unwrap(), // $500.00
            EntryStatus::Pending,
            now_millis(),
        )
        .await
        .unwrap();

    storage
        .create_entry(
            &mut tx,
            txn_id,
            credit_account,
            EntryType::Credit,
            Amount::new(50000).unwrap(),
            EntryStatus::Pending,
            now_millis(),
        )
        .await
        .unwrap();

    // verify pending state
    let entries = storage
        .get_entries_for_transaction(&mut tx, txn_id)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.status() == EntryStatus::Pending));

    // phase 2: capture
    let posted_at = now_millis();

    storage
        .update_transaction_status(&mut tx, txn_id, TransactionStatus::Posted, Some(posted_at))
        .await
        .unwrap();

    let affected = storage
        .update_entry_status_by_transaction(&mut tx, txn_id, EntryStatus::Posted)
        .await
        .unwrap();
    assert_eq!(affected, 2);

    // verify final state
    let txn = storage
        .get_transaction(&mut tx, txn_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(txn.status(), TransactionStatus::Posted);

    let entries = storage
        .get_entries_for_transaction(&mut tx, txn_id)
        .await
        .unwrap();
    assert!(entries.iter().all(|e| e.status() == EntryStatus::Posted));
}
