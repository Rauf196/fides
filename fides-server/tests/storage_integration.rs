//! Integration tests for PostgresStorage
//!
//! These tests require a running PostgreSQL database with the schema applied.
//! Set DATABASE_URL environment variable to connect.
//!
//! Each test runs in a transaction that is rolled back, ensuring test isolation.

use fides_server::domain::account::{AccountId, AccountType};
use fides_server::domain::entry::{EntryStatus, EntryType};
use fides_server::domain::money::Amount;
use fides_server::domain::transaction::TransactionStatus;
use fides_server::domain::validation::compute_balance_delta;
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

// balance storage tests

#[tokio::test]
async fn get_account_balance_returns_initial_zero() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let (posted, pending) = storage
        .get_account_balance(&mut tx, account_id)
        .await
        .unwrap()
        .expect("account should exist");

    assert_eq!(posted, 0);
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn get_account_balance_returns_none_for_missing() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let fake_id = AccountId::new(999999).unwrap();
    let result = storage.get_account_balance(&mut tx, fake_id).await.unwrap();

    assert!(result.is_none());
}

#[tokio::test]
async fn update_account_balance_applies_delta() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    // apply positive delta
    storage
        .update_account_balance(&mut tx, account_id, 0, 1000, 200)
        .await
        .unwrap();

    let (posted, pending) = storage
        .get_account_balance(&mut tx, account_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(posted, 1000);
    assert_eq!(pending, 200);
}

#[tokio::test]
async fn update_account_balance_applies_negative_delta() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    // first add some balance
    storage
        .update_account_balance(&mut tx, account_id, 0, 1000, 200)
        .await
        .unwrap();

    // then subtract
    storage
        .update_account_balance(&mut tx, account_id, 1, -300, -100)
        .await
        .unwrap();

    let (posted, pending) = storage
        .get_account_balance(&mut tx, account_id)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(posted, 700);
    assert_eq!(pending, 100);
}

#[tokio::test]
async fn update_account_balance_increments_version() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    // version 0 -> 1
    storage
        .update_account_balance(&mut tx, account_id, 0, 100, 0)
        .await
        .unwrap();

    let account = storage
        .get_account(&mut tx, account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.version(), 1);

    // version 1 -> 2
    storage
        .update_account_balance(&mut tx, account_id, 1, 100, 0)
        .await
        .unwrap();

    let account = storage
        .get_account(&mut tx, account_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(account.version(), 2);
}

#[tokio::test]
async fn update_account_balance_fails_on_version_mismatch() {
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    // version is 0, but we expect 5
    let result = storage
        .update_account_balance(&mut tx, account_id, 5, 100, 0)
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
async fn load_all_balances_empty_db() {
    let storage = setup().await;
    // don't create any accounts in a transaction - just load from existing db state
    // note: this may include accounts from other tests if not isolated
    let result = storage.load_all_balances().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn load_all_balances_returns_committed_accounts() {
    let storage = setup().await;

    // create and commit accounts
    let mut tx = storage.begin().await.unwrap();

    let id1 = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    storage
        .update_account_balance(&mut tx, id1, 0, 1000, 100)
        .await
        .unwrap();

    let id2 = storage
        .create_account(&mut tx, AccountType::Liability, "EUR", 2, now_millis())
        .await
        .unwrap();

    storage
        .update_account_balance(&mut tx, id2, 0, 2000, 200)
        .await
        .unwrap();

    tx.commit().await.unwrap();

    // now load all balances
    let balances = storage.load_all_balances().await.unwrap();

    // find our accounts (there may be others from previous test runs)
    let b1 = balances.iter().find(|(id, _, _)| *id == id1);
    let b2 = balances.iter().find(|(id, _, _)| *id == id2);

    assert!(b1.is_some(), "should find id1 in balances");
    assert!(b2.is_some(), "should find id2 in balances");

    let (_, posted1, pending1) = b1.unwrap();
    assert_eq!(*posted1, 1000);
    assert_eq!(*pending1, 100);

    let (_, posted2, pending2) = b2.unwrap();
    assert_eq!(*posted2, 2000);
    assert_eq!(*pending2, 200);

    // clean up committed test data — these accounts have materialized balances
    // but no entries, which would cause integrity check warnings on next server start
    let mut cleanup = storage.begin().await.unwrap();
    sqlx::query("DELETE FROM accounts WHERE id = $1 OR id = $2")
        .bind(id1.value())
        .bind(id2.value())
        .execute(&mut *cleanup)
        .await
        .unwrap();
    cleanup.commit().await.unwrap();
}

#[tokio::test]
async fn balance_reconciliation_matches_computed() {
    // verify that materialized balance matches balance computed from entries
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    // create an asset account (debit-normal)
    let account_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let account = storage
        .get_account(&mut tx, account_id)
        .await
        .unwrap()
        .unwrap();
    let normal_balance = account.normal_balance();

    // create a transaction
    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("reconcile-{}", now_millis()),
            TransactionStatus::Posted,
            &json!({}),
            now_millis(),
            Some(now_millis()),
        )
        .await
        .unwrap();

    // create some entries with varying types and statuses
    let entries_spec = [
        (EntryType::Debit, 1000i64, EntryStatus::Posted),
        (EntryType::Credit, 200, EntryStatus::Posted),
        (EntryType::Debit, 300, EntryStatus::Pending),
        (EntryType::Credit, 50, EntryStatus::Voided), // should not affect balance
    ];

    let mut expected_posted_delta: i64 = 0;
    let mut expected_pending_delta: i64 = 0;

    for (entry_type, amount_val, status) in entries_spec {
        storage
            .create_entry(
                &mut tx,
                txn_id,
                account_id,
                entry_type,
                Amount::new(amount_val).unwrap(),
                status,
                now_millis(),
            )
            .await
            .unwrap();

        // track expected deltas
        let delta =
            compute_balance_delta(normal_balance, entry_type, Amount::new(amount_val).unwrap());
        match status {
            EntryStatus::Posted => expected_posted_delta += delta,
            EntryStatus::Pending => expected_pending_delta += delta,
            EntryStatus::Voided => {} // voided entries don't affect balance
        }
    }

    // update materialized balance
    storage
        .update_account_balance(
            &mut tx,
            account_id,
            0,
            expected_posted_delta,
            expected_pending_delta,
        )
        .await
        .unwrap();

    // get materialized balance
    let (posted, pending) = storage
        .get_account_balance(&mut tx, account_id)
        .await
        .unwrap()
        .unwrap();

    // compute balance from entries using the validation function
    let entries = storage
        .get_entries_for_account(&mut tx, account_id)
        .await
        .unwrap();

    let computed =
        fides_server::domain::validation::compute_account_balance(normal_balance, &entries)
            .unwrap();

    // verify they match
    assert_eq!(
        posted,
        computed.posted(),
        "materialized posted {} should match computed {}",
        posted,
        computed.posted()
    );
    assert_eq!(
        pending,
        computed.pending(),
        "materialized pending {} should match computed {}",
        pending,
        computed.pending()
    );
}

#[tokio::test]
async fn balance_with_authorize_capture_void_workflow() {
    // test balance updates through full lifecycle
    let storage = setup().await;
    let mut tx = storage.begin().await.unwrap();

    // create asset and liability accounts
    let asset_id = storage
        .create_account(&mut tx, AccountType::Asset, "USD", 2, now_millis())
        .await
        .unwrap();

    let liability_id = storage
        .create_account(&mut tx, AccountType::Liability, "USD", 2, now_millis())
        .await
        .unwrap();

    let asset = storage
        .get_account(&mut tx, asset_id)
        .await
        .unwrap()
        .unwrap();
    let liability = storage
        .get_account(&mut tx, liability_id)
        .await
        .unwrap()
        .unwrap();

    // authorize: create pending transaction
    let txn_id = storage
        .create_transaction(
            &mut tx,
            &format!("balance-workflow-{}", now_millis()),
            TransactionStatus::Pending,
            &json!({}),
            now_millis(),
            None,
        )
        .await
        .unwrap();

    // entries: debit asset, credit liability (both pending)
    let amount = Amount::new(10000).unwrap(); // $100.00

    storage
        .create_entry(
            &mut tx,
            txn_id,
            asset_id,
            EntryType::Debit,
            amount,
            EntryStatus::Pending,
            now_millis(),
        )
        .await
        .unwrap();

    storage
        .create_entry(
            &mut tx,
            txn_id,
            liability_id,
            EntryType::Credit,
            amount,
            EntryStatus::Pending,
            now_millis(),
        )
        .await
        .unwrap();

    // update pending balances
    let asset_delta = compute_balance_delta(asset.normal_balance(), EntryType::Debit, amount);
    let liability_delta =
        compute_balance_delta(liability.normal_balance(), EntryType::Credit, amount);

    storage
        .update_account_balance(&mut tx, asset_id, 0, 0, asset_delta)
        .await
        .unwrap();
    storage
        .update_account_balance(&mut tx, liability_id, 0, 0, liability_delta)
        .await
        .unwrap();

    // verify pending state
    let (asset_posted, asset_pending) = storage
        .get_account_balance(&mut tx, asset_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(asset_posted, 0);
    assert_eq!(asset_pending, 10000); // pending debit on debit-normal = +pending

    let (liability_posted, liability_pending) = storage
        .get_account_balance(&mut tx, liability_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(liability_posted, 0);
    assert_eq!(liability_pending, 10000); // pending credit on credit-normal = +pending

    // capture: move from pending to posted
    storage
        .update_entry_status_by_transaction(&mut tx, txn_id, EntryStatus::Posted)
        .await
        .unwrap();
    storage
        .update_transaction_status(
            &mut tx,
            txn_id,
            TransactionStatus::Posted,
            Some(now_millis()),
        )
        .await
        .unwrap();

    // balance update: +posted, -pending (delta moves from pending to posted)
    storage
        .update_account_balance(&mut tx, asset_id, 1, asset_delta, -asset_delta)
        .await
        .unwrap();
    storage
        .update_account_balance(&mut tx, liability_id, 1, liability_delta, -liability_delta)
        .await
        .unwrap();

    // verify posted state
    let (asset_posted, asset_pending) = storage
        .get_account_balance(&mut tx, asset_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(asset_posted, 10000);
    assert_eq!(asset_pending, 0);

    let (liability_posted, liability_pending) = storage
        .get_account_balance(&mut tx, liability_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(liability_posted, 10000);
    assert_eq!(liability_pending, 0);
}
