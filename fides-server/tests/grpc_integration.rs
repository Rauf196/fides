//! gRPC integration tests for the fides ledger server
//!
//! requires a running PostgreSQL database with DATABASE_URL set.
//! all tests share a single server instance to conserve db connections.

mod common;

use fides_proto::ledger_service_client::LedgerServiceClient;
use fides_proto::{
    AuthorizeRequest, CaptureRequest, CreateAccountRequest, GetAccountRequest, GetBalanceRequest,
    GetEntriesRequest, TransferLeg, VoidRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::transport::Channel;
use tonic::Code;
use uuid::Uuid;

#[tokio::test]
async fn create_account_and_get() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let resp = client
        .create_account(CreateAccountRequest {
            account_type: 1, // asset
            asset_code: "USD".into(),
            asset_scale: 2,
        })
        .await
        .unwrap()
        .into_inner();

    let account = resp.account.unwrap();
    assert!(account.id > 0);
    assert_eq!(account.account_type, 1);
    assert_eq!(account.normal_balance, 1); // debit
    assert_eq!(account.asset_code, "USD");
    assert_eq!(account.asset_scale, 2);

    // get by id
    let got = client
        .get_account(GetAccountRequest {
            account_id: account.id,
        })
        .await
        .unwrap()
        .into_inner()
        .account
        .unwrap();

    assert_eq!(got.id, account.id);
    assert_eq!(got.asset_code, "USD");
}

#[tokio::test]
async fn create_account_get_balance_zero() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let account = create_asset(&mut client).await;

    let balance = client
        .get_balance(GetBalanceRequest {
            account_id: account,
        })
        .await
        .unwrap()
        .into_inner()
        .balance
        .unwrap();

    assert_eq!(balance.posted, 0);
    assert_eq!(balance.pending, 0);
    assert_eq!(balance.available, 0);
}

#[tokio::test]
async fn get_nonexistent_account() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let err = client
        .get_account(GetAccountRequest {
            account_id: i64::MAX,
        })
        .await
        .unwrap_err();

    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn authorize_two_leg_transfer() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;

    // do_authorize: credit asset (withdrawal), debit counterparty (withdrawal)
    // both sides have 10_000 available from funding
    let resp = do_authorize(&mut client, asset, counterparty, 500).await;
    let tx = resp.transaction.unwrap();
    assert_eq!(tx.status, 1); // pending

    // asset: credit on debit-normal → pending delta = -500
    let bal = get_balance(&mut client, asset).await;
    assert_eq!(bal.posted, 10_000);
    assert_eq!(bal.pending, -500);

    // counterparty (equity, credit-normal): debit → pending delta = -500
    let bal = get_balance(&mut client, counterparty).await;
    assert_eq!(bal.posted, 10_000);
    assert_eq!(bal.pending, -500);
}

#[tokio::test]
async fn authorize_insufficient_funds() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 1_000).await;

    let err = client
        .authorize(AuthorizeRequest {
            idempotency_key: Uuid::new_v4().to_string(),
            legs: vec![
                TransferLeg {
                    account_id: asset,
                    entry_type: 2, // credit (withdrawal from asset)
                    amount: 2_000,
                },
                TransferLeg {
                    account_id: counterparty,
                    entry_type: 1, // debit
                    amount: 2_000,
                },
            ],
            metadata: String::new(),
        })
        .await
        .unwrap_err();

    assert_eq!(err.code(), Code::FailedPrecondition);
}

#[tokio::test]
async fn authorize_idempotent() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;
    let key = Uuid::new_v4().to_string();

    let resp1 = client
        .authorize(AuthorizeRequest {
            idempotency_key: key.clone(),
            legs: vec![
                TransferLeg {
                    account_id: asset,
                    entry_type: 2,
                    amount: 100,
                },
                TransferLeg {
                    account_id: counterparty,
                    entry_type: 1,
                    amount: 100,
                },
            ],
            metadata: String::new(),
        })
        .await
        .unwrap()
        .into_inner();

    let resp2 = client
        .authorize(AuthorizeRequest {
            idempotency_key: key,
            legs: vec![
                TransferLeg {
                    account_id: asset,
                    entry_type: 2,
                    amount: 100,
                },
                TransferLeg {
                    account_id: counterparty,
                    entry_type: 1,
                    amount: 100,
                },
            ],
            metadata: String::new(),
        })
        .await
        .unwrap()
        .into_inner();

    // same transaction returned, no duplicate
    assert_eq!(resp1.transaction.unwrap().id, resp2.transaction.unwrap().id);

    // balance only changed once (credit on debit-normal = -100)
    let bal = get_balance(&mut client, asset).await;
    assert_eq!(bal.pending, -100);
}

#[tokio::test]
async fn authorize_unbalanced_legs() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let asset = create_asset(&mut client).await;
    let liability = create_liability(&mut client).await;

    let err = client
        .authorize(AuthorizeRequest {
            idempotency_key: Uuid::new_v4().to_string(),
            legs: vec![
                TransferLeg {
                    account_id: asset,
                    entry_type: 1,
                    amount: 1_000,
                },
                TransferLeg {
                    account_id: liability,
                    entry_type: 2,
                    amount: 500,
                },
            ],
            metadata: String::new(),
        })
        .await
        .unwrap_err();

    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn authorize_multi_leg_transaction() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, _equity) = setup_funded_asset(&mut client, 10_000).await;
    let liability_b = create_liability(&mut client).await;
    let liability_c = create_liability(&mut client).await;

    // debit asset 100, credit liability_b 60, credit liability_c 40
    // debit on debit-normal (increases) + credit on credit-normal (increases) → no balance checks
    let resp = client
        .authorize(AuthorizeRequest {
            idempotency_key: Uuid::new_v4().to_string(),
            legs: vec![
                TransferLeg {
                    account_id: asset,
                    entry_type: 1, // debit (deposit to asset)
                    amount: 100,
                },
                TransferLeg {
                    account_id: liability_b,
                    entry_type: 2, // credit (increase liability)
                    amount: 60,
                },
                TransferLeg {
                    account_id: liability_c,
                    entry_type: 2, // credit (increase liability)
                    amount: 40,
                },
            ],
            metadata: String::new(),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.entries.len(), 3);

    // debit on debit-normal = positive pending delta
    let bal_a = get_balance(&mut client, asset).await;
    assert_eq!(bal_a.pending, 100);

    // credit on credit-normal = positive pending delta
    let bal_b = get_balance(&mut client, liability_b).await;
    assert_eq!(bal_b.pending, 60);

    let bal_c = get_balance(&mut client, liability_c).await;
    assert_eq!(bal_c.pending, 40);
}

#[tokio::test]
async fn capture_moves_pending_to_posted() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;
    let auth = do_authorize(&mut client, asset, counterparty, 500).await;
    let tx_id = auth.transaction.unwrap().id;

    let capture = client
        .capture(CaptureRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(capture.transaction.unwrap().status, 2); // posted

    // asset: posted went from 10_000 to 10_000+(-500)=9_500, pending cleared
    let bal = get_balance(&mut client, asset).await;
    assert_eq!(bal.posted, 9_500);
    assert_eq!(bal.pending, 0);
    assert_eq!(bal.available, 9_500);
}

#[tokio::test]
async fn capture_idempotent() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;
    let auth = do_authorize(&mut client, asset, counterparty, 300).await;
    let tx_id = auth.transaction.unwrap().id;

    client
        .capture(CaptureRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap();

    // second capture should succeed (idempotent)
    let resp = client
        .capture(CaptureRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.transaction.unwrap().status, 2);
}

#[tokio::test]
async fn capture_voided_fails() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;
    let auth = do_authorize(&mut client, asset, counterparty, 200).await;
    let tx_id = auth.transaction.unwrap().id;

    // void first
    client
        .void(VoidRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap();

    // capture should fail
    let err = client
        .capture(CaptureRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap_err();

    assert_eq!(err.code(), Code::FailedPrecondition);
}

#[tokio::test]
async fn void_restores_balance() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 5_000).await;

    let bal_before = get_balance(&mut client, asset).await;
    assert_eq!(bal_before.available, 5_000);

    let auth = do_authorize(&mut client, asset, counterparty, 1_000).await;
    let tx_id = auth.transaction.unwrap().id;

    // credit on debit-normal: pending=-1000, available=5000-(-1000)=6000
    let bal_pending = get_balance(&mut client, asset).await;
    assert_eq!(bal_pending.pending, -1_000);

    client
        .void(VoidRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap();

    let bal_after = get_balance(&mut client, asset).await;
    assert_eq!(bal_after.available, 5_000);
    assert_eq!(bal_after.pending, 0);
}

#[tokio::test]
async fn void_idempotent() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;
    let auth = do_authorize(&mut client, asset, counterparty, 400).await;
    let tx_id = auth.transaction.unwrap().id;

    client
        .void(VoidRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap();

    let resp = client
        .void(VoidRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(resp.transaction.unwrap().status, 3); // voided
}

#[tokio::test]
async fn void_captured_fails() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;
    let auth = do_authorize(&mut client, asset, counterparty, 300).await;
    let tx_id = auth.transaction.unwrap().id;

    client
        .capture(CaptureRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap();

    let err = client
        .void(VoidRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap_err();

    assert_eq!(err.code(), Code::FailedPrecondition);
}

#[tokio::test]
async fn concurrent_authorize_serializes_via_row_lock() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;

    // spawn 10 concurrent authorize attempts, each 1_000
    // FOR UPDATE serializes access; some may hit version conflicts
    let mut handles = Vec::new();
    for _ in 0..10 {
        let mut c = srv.client().await;
        let a = asset;
        let cp = counterparty;
        handles.push(tokio::spawn(async move {
            c.authorize(AuthorizeRequest {
                idempotency_key: Uuid::new_v4().to_string(),
                legs: vec![
                    TransferLeg {
                        account_id: a,
                        entry_type: 2, // credit (withdrawal)
                        amount: 1_000,
                    },
                    TransferLeg {
                        account_id: cp,
                        entry_type: 1, // debit
                        amount: 1_000,
                    },
                ],
                metadata: String::new(),
            })
            .await
        }));
    }

    let mut successes = 0i64;
    let mut rejections = 0i64;
    for handle in handles {
        let result = handle.await.unwrap();
        match result {
            Ok(_) => successes += 1,
            Err(e) => {
                // version conflict (ABORTED) or insufficient funds
                assert!(
                    e.code() == Code::Aborted || e.code() == Code::FailedPrecondition,
                    "unexpected error: {:?} ({})",
                    e.code(),
                    e.message()
                );
                rejections += 1;
            }
        }
    }

    assert!(successes > 0, "at least one authorize should succeed");
    assert_eq!(successes + rejections, 10, "all 10 attempts should resolve");

    // balance should be consistent after all operations
    let bal = get_balance(&mut client, asset).await;
    let expected_pending = -(successes * 1_000);
    assert_eq!(bal.pending, expected_pending);
    assert_eq!(bal.posted, 10_000);
}

#[tokio::test]
async fn get_entries_returns_transaction_entries() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    let (asset, counterparty) = setup_funded_asset(&mut client, 10_000).await;
    do_authorize(&mut client, asset, counterparty, 750).await;

    let mut stream = client
        .get_entries(GetEntriesRequest { account_id: asset })
        .await
        .unwrap()
        .into_inner();

    let mut entries = Vec::new();
    while let Some(entry) = stream.message().await.unwrap() {
        entries.push(entry);
    }

    // funding (debit 10_000, posted) + authorize (credit 750, pending)
    assert!(
        entries.len() >= 2,
        "expected at least 2 entries, got {}",
        entries.len()
    );

    // verify all entries belong to this account
    for entry in &entries {
        assert_eq!(entry.account_id, asset);
    }
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let srv = common::server().await;
    let status = http_get_status(srv.http_addr(), "/health").await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn ready_endpoint_returns_ok() {
    let srv = common::server().await;
    let status = http_get_status(srv.http_addr(), "/ready").await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn metrics_endpoint_returns_data() {
    let srv = common::server().await;
    let mut client = srv.client().await;

    // make a gRPC call to ensure metrics exist
    let _ = create_asset(&mut client).await;

    let body = http_get_response(srv.http_addr(), "/metrics").await;

    assert!(
        body.contains("fides_grpc_requests_total"),
        "metrics should contain fides_grpc_requests_total"
    );
    assert!(
        body.contains("fides_grpc_request_duration_seconds"),
        "metrics should contain fides_grpc_request_duration_seconds"
    );
}

async fn create_asset(client: &mut LedgerServiceClient<Channel>) -> i64 {
    client
        .create_account(CreateAccountRequest {
            account_type: 1,
            asset_code: "USD".into(),
            asset_scale: 2,
        })
        .await
        .unwrap()
        .into_inner()
        .account
        .unwrap()
        .id
}

async fn create_liability(client: &mut LedgerServiceClient<Channel>) -> i64 {
    client
        .create_account(CreateAccountRequest {
            account_type: 2,
            asset_code: "USD".into(),
            asset_scale: 2,
        })
        .await
        .unwrap()
        .into_inner()
        .account
        .unwrap()
        .id
}

async fn create_equity(client: &mut LedgerServiceClient<Channel>) -> i64 {
    client
        .create_account(CreateAccountRequest {
            account_type: 3,
            asset_code: "USD".into(),
            asset_scale: 2,
        })
        .await
        .unwrap()
        .into_inner()
        .account
        .unwrap()
        .id
}

/// create asset + equity pair, fund asset via authorize+capture
///
/// accounting: equity is a credit-normal account (source of funds),
/// asset is debit-normal (destination). funding = debit asset, credit equity.
async fn setup_funded_asset(client: &mut LedgerServiceClient<Channel>, amount: i64) -> (i64, i64) {
    let asset = create_asset(client).await;
    let equity = create_equity(client).await;

    // authorize funding: debit asset (increases), credit equity (increases)
    let resp = client
        .authorize(AuthorizeRequest {
            idempotency_key: Uuid::new_v4().to_string(),
            legs: vec![
                TransferLeg {
                    account_id: asset,
                    entry_type: 1, // debit
                    amount,
                },
                TransferLeg {
                    account_id: equity,
                    entry_type: 2, // credit
                    amount,
                },
            ],
            metadata: String::new(),
        })
        .await
        .unwrap()
        .into_inner();

    let tx_id = resp.transaction.unwrap().id;

    // capture to move from pending to posted
    client
        .capture(CaptureRequest {
            transaction_id: tx_id,
        })
        .await
        .unwrap();

    (asset, equity)
}

async fn do_authorize(
    client: &mut LedgerServiceClient<Channel>,
    from: i64,
    to: i64,
    amount: i64,
) -> fides_proto::AuthorizeResponse {
    client
        .authorize(AuthorizeRequest {
            idempotency_key: Uuid::new_v4().to_string(),
            legs: vec![
                TransferLeg {
                    account_id: from,
                    entry_type: 2, // credit (withdrawal from debit-normal)
                    amount,
                },
                TransferLeg {
                    account_id: to,
                    entry_type: 1, // debit
                    amount,
                },
            ],
            metadata: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
}

async fn get_balance(
    client: &mut LedgerServiceClient<Channel>,
    account_id: i64,
) -> fides_proto::Balance {
    client
        .get_balance(GetBalanceRequest { account_id })
        .await
        .unwrap()
        .into_inner()
        .balance
        .unwrap()
}

/// raw HTTP/1.1 GET, returns status code
async fn http_get_status(addr: std::net::SocketAddr, path: &str) -> u16 {
    let body = http_get_raw(addr, path).await;
    parse_status(&body)
}

/// raw HTTP/1.1 GET, returns full response (headers + body)
async fn http_get_response(addr: std::net::SocketAddr, path: &str) -> String {
    http_get_raw(addr, path).await
}

async fn http_get_raw(addr: std::net::SocketAddr, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        path
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

fn parse_status(raw: &str) -> u16 {
    // "HTTP/1.1 200 OK\r\n..."
    raw.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}
