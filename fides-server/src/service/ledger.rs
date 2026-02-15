use std::sync::Arc;

use metrics::counter;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use fides_proto::{
    ledger_service_server::LedgerService as LedgerServiceTrait, Account as ProtoAccount,
    AuthorizeRequest, AuthorizeResponse, Balance as ProtoBalance, CaptureRequest, CaptureResponse,
    CreateAccountRequest, CreateAccountResponse, Entry as ProtoEntry, GetAccountRequest,
    GetAccountResponse, GetBalanceRequest, GetBalanceResponse, GetEntriesRequest,
    Transaction as ProtoTransaction, TransferLeg as ProtoTransferLeg, VoidRequest, VoidResponse,
};

use crate::domain::account::{Account, AccountId, AccountType, NormalBalance};
use crate::domain::entry::{Entry, EntryStatus, EntryType};
use crate::domain::money::Amount;
use crate::domain::transaction::{Transaction, TransactionId, TransactionStatus};
use crate::domain::validation::{compute_balance_delta, validate_transaction_balance, TransferLeg};
use crate::storage::postgres::PostgresStorage;
use crate::storage::BalanceCache;

use super::ServiceError;

pub struct LedgerService {
    storage: Arc<PostgresStorage>,
    cache: Arc<BalanceCache>,
}

impl LedgerService {
    pub fn new(storage: Arc<PostgresStorage>, cache: Arc<BalanceCache>) -> Self {
        Self { storage, cache }
    }
}

#[tonic::async_trait]
impl LedgerServiceTrait for LedgerService {
    #[tracing::instrument(skip(self, request), fields(method = "CreateAccount", account_id))]
    async fn create_account(
        &self,
        request: Request<CreateAccountRequest>,
    ) -> Result<Response<CreateAccountResponse>, Status> {
        let req = request.into_inner();

        let account_type = proto_to_account_type(req.account_type)?;
        let asset_code = req.asset_code;
        let asset_scale = req.asset_scale as u8;

        if asset_code.is_empty() {
            return Err(ServiceError::InvalidArgument("asset_code is required".into()).into());
        }
        if asset_scale > 18 {
            return Err(ServiceError::InvalidArgument(format!(
                "asset_scale {} exceeds max 18",
                asset_scale
            ))
            .into());
        }

        let now = now_millis();

        let mut tx = self.storage.begin().await.map_err(ServiceError::from)?;

        let account_id = self
            .storage
            .create_account(&mut tx, account_type, &asset_code, asset_scale, now)
            .await
            .map_err(ServiceError::from)?;

        tx.commit()
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        tracing::Span::current().record("account_id", account_id.value());

        // add to cache with zero balances
        self.cache.set(account_id, 0, 0);

        let account = Account::new(account_id, account_type, asset_code, asset_scale, 0, now)
            .map_err(ServiceError::from)?;

        Ok(Response::new(CreateAccountResponse {
            account: Some(account_to_proto(&account)),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(method = "GetAccount", account_id))]
    async fn get_account(
        &self,
        request: Request<GetAccountRequest>,
    ) -> Result<Response<GetAccountResponse>, Status> {
        let req = request.into_inner();
        let account_id = AccountId::new(req.account_id).map_err(ServiceError::from)?;
        tracing::Span::current().record("account_id", account_id.value());

        let mut tx = self.storage.begin().await.map_err(ServiceError::from)?;

        let account = self
            .storage
            .get_account(&mut tx, account_id)
            .await
            .map_err(ServiceError::from)?
            .ok_or_else(|| ServiceError::AccountNotFound {
                account_id: account_id.value(),
            })?;

        tx.commit()
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        Ok(Response::new(GetAccountResponse {
            account: Some(account_to_proto(&account)),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(method = "GetBalance", account_id))]
    async fn get_balance(
        &self,
        request: Request<GetBalanceRequest>,
    ) -> Result<Response<GetBalanceResponse>, Status> {
        let req = request.into_inner();
        let account_id = AccountId::new(req.account_id).map_err(ServiceError::from)?;
        tracing::Span::current().record("account_id", account_id.value());

        // try cache first (O(1))
        let balance = self
            .cache
            .get(account_id)
            .ok_or_else(|| ServiceError::AccountNotFound {
                account_id: account_id.value(),
            })?;

        Ok(Response::new(GetBalanceResponse {
            balance: Some(ProtoBalance {
                posted: balance.posted(),
                pending: balance.pending(),
                available: balance.available(),
            }),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(method = "Authorize", idempotency_key))]
    async fn authorize(
        &self,
        request: Request<AuthorizeRequest>,
    ) -> Result<Response<AuthorizeResponse>, Status> {
        let req = request.into_inner();

        // validate idempotency key
        if req.idempotency_key.is_empty() {
            return Err(ServiceError::InvalidArgument("idempotency_key is required".into()).into());
        }
        tracing::Span::current().record("idempotency_key", &req.idempotency_key);

        // parse metadata
        let metadata: serde_json::Value = if req.metadata.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&req.metadata).map_err(|e| {
                ServiceError::InvalidArgument(format!("invalid metadata json: {}", e))
            })?
        };

        // convert and validate legs
        let legs = parse_transfer_legs(&req.legs)?;
        validate_transaction_balance(&legs).map_err(ServiceError::from)?;

        let now = now_millis();

        let mut tx = self.storage.begin().await.map_err(ServiceError::from)?;

        // check idempotency - if transaction exists, return it
        if let Some(existing) = self
            .storage
            .find_transaction_by_key(&mut tx, &req.idempotency_key)
            .await
            .map_err(ServiceError::from)?
        {
            let entries = self
                .storage
                .get_entries_for_transaction(&mut tx, existing.id())
                .await
                .map_err(ServiceError::from)?;

            tx.commit()
                .await
                .map_err(|e| ServiceError::Internal(e.to_string()))?;

            counter!("fides_idempotent_hits_total", "method" => "authorize").increment(1);
            return Ok(Response::new(AuthorizeResponse {
                transaction: Some(transaction_to_proto(&existing)),
                entries: entries.iter().map(entry_to_proto).collect(),
            }));
        }

        // collect unique accounts and lock them
        let mut account_ids: Vec<AccountId> = legs.iter().map(|l| l.account_id()).collect();
        account_ids.sort_by_key(|id| id.value());
        account_ids.dedup();

        let mut accounts = Vec::with_capacity(account_ids.len());
        for &account_id in &account_ids {
            let account = self
                .storage
                .get_account_for_update(&mut tx, account_id)
                .await
                .map_err(ServiceError::from)?
                .ok_or_else(|| ServiceError::AccountNotFound {
                    account_id: account_id.value(),
                })?;
            accounts.push(account);
        }

        // check available balance for accounts being debited (on debit-normal accounts)
        // or credited (on credit-normal accounts) - i.e., amounts leaving the account
        for leg in &legs {
            let account = accounts
                .iter()
                .find(|a| a.id() == leg.account_id())
                .unwrap();

            // check if this leg reduces available balance
            let reduces_balance = match (account.normal_balance(), leg.entry_type()) {
                // debit on debit-normal = increases balance (deposit) - no check
                // credit on debit-normal = decreases balance (withdrawal) - check
                (NormalBalance::Debit, EntryType::Credit) => true,
                // credit on credit-normal = increases balance - no check
                // debit on credit-normal = decreases balance - check
                (NormalBalance::Credit, EntryType::Debit) => true,
                _ => false,
            };

            if reduces_balance {
                let balance = self.cache.get(leg.account_id()).ok_or_else(|| {
                    ServiceError::AccountNotFound {
                        account_id: leg.account_id().value(),
                    }
                })?;

                if !balance.has_available(leg.amount()) {
                    counter!("fides_insufficient_funds_total").increment(1);
                    return Err(ServiceError::InsufficientFunds {
                        account_id: leg.account_id().value(),
                        available: balance.available(),
                        requested: leg.amount().value(),
                    }
                    .into());
                }
            }
        }

        // create transaction
        let transaction_id = self
            .storage
            .create_transaction(
                &mut tx,
                &req.idempotency_key,
                TransactionStatus::Pending,
                &metadata,
                now,
                None,
            )
            .await
            .map_err(ServiceError::from)?;

        // create entries and update balances
        let mut entries = Vec::with_capacity(legs.len());
        let mut balance_updates: Vec<(AccountId, i64, i64)> = Vec::new();

        for leg in &legs {
            let account = accounts
                .iter()
                .find(|a| a.id() == leg.account_id())
                .unwrap();

            let entry_id = self
                .storage
                .create_entry(
                    &mut tx,
                    transaction_id,
                    leg.account_id(),
                    leg.entry_type(),
                    leg.amount(),
                    EntryStatus::Pending,
                    now,
                )
                .await
                .map_err(ServiceError::from)?;

            let entry = Entry::new(
                entry_id,
                transaction_id,
                leg.account_id(),
                leg.entry_type(),
                leg.amount(),
                EntryStatus::Pending,
                now,
            )
            .map_err(ServiceError::from)?;

            entries.push(entry);

            // compute pending delta (pending entries affect pending balance)
            let delta =
                compute_balance_delta(account.normal_balance(), leg.entry_type(), leg.amount());

            // accumulate balance updates per account
            if let Some(update) = balance_updates
                .iter_mut()
                .find(|(id, _, _)| *id == leg.account_id())
            {
                update.2 += delta; // pending delta
            } else {
                balance_updates.push((leg.account_id(), 0, delta)); // (id, posted_delta, pending_delta)
            }
        }

        // apply balance updates
        for (account_id, posted_delta, pending_delta) in &balance_updates {
            let account = accounts.iter().find(|a| a.id() == *account_id).unwrap();

            self.storage
                .update_account_balance(
                    &mut tx,
                    *account_id,
                    account.version(),
                    *posted_delta,
                    *pending_delta,
                )
                .await
                .map_err(ServiceError::from)?;
        }

        tx.commit()
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        // update cache after commit
        for (account_id, posted_delta, pending_delta) in balance_updates {
            self.cache
                .apply_delta(account_id, posted_delta, pending_delta);
        }

        counter!("fides_transactions_total", "status" => "pending").increment(1);

        let transaction = Transaction::new(
            transaction_id,
            req.idempotency_key,
            TransactionStatus::Pending,
            metadata,
            now,
            0,
        )
        .map_err(ServiceError::from)?;

        Ok(Response::new(AuthorizeResponse {
            transaction: Some(transaction_to_proto(&transaction)),
            entries: entries.iter().map(entry_to_proto).collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(method = "Capture", transaction_id))]
    async fn capture(
        &self,
        request: Request<CaptureRequest>,
    ) -> Result<Response<CaptureResponse>, Status> {
        let req = request.into_inner();
        let transaction_id = TransactionId::new(req.transaction_id).map_err(ServiceError::from)?;
        tracing::Span::current().record("transaction_id", transaction_id.value());

        let now = now_millis();

        let mut tx = self.storage.begin().await.map_err(ServiceError::from)?;

        // get transaction
        let transaction = self
            .storage
            .get_transaction(&mut tx, transaction_id)
            .await
            .map_err(ServiceError::from)?
            .ok_or_else(|| ServiceError::TransactionNotFound {
                transaction_id: transaction_id.value(),
            })?;

        // idempotency: already captured = success
        if transaction.status() == TransactionStatus::Posted {
            tx.commit()
                .await
                .map_err(|e| ServiceError::Internal(e.to_string()))?;
            counter!("fides_idempotent_hits_total", "method" => "capture").increment(1);
            return Ok(Response::new(CaptureResponse {
                transaction: Some(transaction_to_proto(&transaction)),
            }));
        }

        // validate state
        if transaction.status() != TransactionStatus::Pending {
            return Err(ServiceError::InvalidTransactionState {
                transaction_id: transaction_id.value(),
                current: transaction.status(),
                attempted: "capture",
            }
            .into());
        }

        // get entries
        let entries = self
            .storage
            .get_entries_for_transaction(&mut tx, transaction_id)
            .await
            .map_err(ServiceError::from)?;

        // collect unique accounts and lock them
        let mut account_ids: Vec<AccountId> = entries.iter().map(|e| e.account_id()).collect();
        account_ids.sort_by_key(|id| id.value());
        account_ids.dedup();

        let mut accounts = Vec::with_capacity(account_ids.len());
        for &account_id in &account_ids {
            let account = self
                .storage
                .get_account_for_update(&mut tx, account_id)
                .await
                .map_err(ServiceError::from)?
                .ok_or_else(|| ServiceError::AccountNotFound {
                    account_id: account_id.value(),
                })?;
            accounts.push(account);
        }

        // compute balance deltas: move from pending to posted
        let mut balance_updates: Vec<(AccountId, i64, i64)> = Vec::new();

        for entry in &entries {
            let account = accounts
                .iter()
                .find(|a| a.id() == entry.account_id())
                .unwrap();

            let delta =
                compute_balance_delta(account.normal_balance(), entry.entry_type(), entry.amount());

            if let Some(update) = balance_updates
                .iter_mut()
                .find(|(id, _, _)| *id == entry.account_id())
            {
                update.1 += delta; // add to posted
                update.2 -= delta; // remove from pending
            } else {
                balance_updates.push((entry.account_id(), delta, -delta));
            }
        }

        // update transaction status
        self.storage
            .update_transaction_status(
                &mut tx,
                transaction_id,
                TransactionStatus::Posted,
                Some(now),
            )
            .await
            .map_err(ServiceError::from)?;

        // update entry statuses
        self.storage
            .update_entry_status_by_transaction(&mut tx, transaction_id, EntryStatus::Posted)
            .await
            .map_err(ServiceError::from)?;

        // apply balance updates
        for (account_id, posted_delta, pending_delta) in &balance_updates {
            let account = accounts.iter().find(|a| a.id() == *account_id).unwrap();

            self.storage
                .update_account_balance(
                    &mut tx,
                    *account_id,
                    account.version(),
                    *posted_delta,
                    *pending_delta,
                )
                .await
                .map_err(ServiceError::from)?;
        }

        tx.commit()
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        // update cache after commit
        for (account_id, posted_delta, pending_delta) in balance_updates {
            self.cache
                .apply_delta(account_id, posted_delta, pending_delta);
        }

        counter!("fides_transactions_total", "status" => "posted").increment(1);

        let updated_transaction = Transaction::new(
            transaction_id,
            transaction.idempotency_key().to_string(),
            TransactionStatus::Posted,
            transaction.metadata().clone(),
            transaction.created_at(),
            now,
        )
        .map_err(ServiceError::from)?;

        Ok(Response::new(CaptureResponse {
            transaction: Some(transaction_to_proto(&updated_transaction)),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(method = "Void", transaction_id))]
    async fn void(&self, request: Request<VoidRequest>) -> Result<Response<VoidResponse>, Status> {
        let req = request.into_inner();
        let transaction_id = TransactionId::new(req.transaction_id).map_err(ServiceError::from)?;
        tracing::Span::current().record("transaction_id", transaction_id.value());

        let mut tx = self.storage.begin().await.map_err(ServiceError::from)?;

        // get transaction
        let transaction = self
            .storage
            .get_transaction(&mut tx, transaction_id)
            .await
            .map_err(ServiceError::from)?
            .ok_or_else(|| ServiceError::TransactionNotFound {
                transaction_id: transaction_id.value(),
            })?;

        // idempotency: already voided = success
        if transaction.status() == TransactionStatus::Voided {
            tx.commit()
                .await
                .map_err(|e| ServiceError::Internal(e.to_string()))?;
            counter!("fides_idempotent_hits_total", "method" => "void").increment(1);
            return Ok(Response::new(VoidResponse {
                transaction: Some(transaction_to_proto(&transaction)),
            }));
        }

        // validate state
        if transaction.status() != TransactionStatus::Pending {
            return Err(ServiceError::InvalidTransactionState {
                transaction_id: transaction_id.value(),
                current: transaction.status(),
                attempted: "void",
            }
            .into());
        }

        // get entries
        let entries = self
            .storage
            .get_entries_for_transaction(&mut tx, transaction_id)
            .await
            .map_err(ServiceError::from)?;

        // collect unique accounts and lock them
        let mut account_ids: Vec<AccountId> = entries.iter().map(|e| e.account_id()).collect();
        account_ids.sort_by_key(|id| id.value());
        account_ids.dedup();

        let mut accounts = Vec::with_capacity(account_ids.len());
        for &account_id in &account_ids {
            let account = self
                .storage
                .get_account_for_update(&mut tx, account_id)
                .await
                .map_err(ServiceError::from)?
                .ok_or_else(|| ServiceError::AccountNotFound {
                    account_id: account_id.value(),
                })?;
            accounts.push(account);
        }

        // compute balance deltas: remove pending holds
        let mut balance_updates: Vec<(AccountId, i64, i64)> = Vec::new();

        for entry in &entries {
            let account = accounts
                .iter()
                .find(|a| a.id() == entry.account_id())
                .unwrap();

            let delta =
                compute_balance_delta(account.normal_balance(), entry.entry_type(), entry.amount());

            if let Some(update) = balance_updates
                .iter_mut()
                .find(|(id, _, _)| *id == entry.account_id())
            {
                update.2 -= delta; // remove from pending
            } else {
                balance_updates.push((entry.account_id(), 0, -delta));
            }
        }

        // update transaction status
        self.storage
            .update_transaction_status(&mut tx, transaction_id, TransactionStatus::Voided, None)
            .await
            .map_err(ServiceError::from)?;

        // update entry statuses
        self.storage
            .update_entry_status_by_transaction(&mut tx, transaction_id, EntryStatus::Voided)
            .await
            .map_err(ServiceError::from)?;

        // apply balance updates
        for (account_id, posted_delta, pending_delta) in &balance_updates {
            let account = accounts.iter().find(|a| a.id() == *account_id).unwrap();

            self.storage
                .update_account_balance(
                    &mut tx,
                    *account_id,
                    account.version(),
                    *posted_delta,
                    *pending_delta,
                )
                .await
                .map_err(ServiceError::from)?;
        }

        tx.commit()
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        // update cache after commit
        for (account_id, posted_delta, pending_delta) in balance_updates {
            self.cache
                .apply_delta(account_id, posted_delta, pending_delta);
        }

        counter!("fides_transactions_total", "status" => "voided").increment(1);

        let updated_transaction = Transaction::new(
            transaction_id,
            transaction.idempotency_key().to_string(),
            TransactionStatus::Voided,
            transaction.metadata().clone(),
            transaction.created_at(),
            0,
        )
        .map_err(ServiceError::from)?;

        Ok(Response::new(VoidResponse {
            transaction: Some(transaction_to_proto(&updated_transaction)),
        }))
    }

    type GetEntriesStream = ReceiverStream<Result<ProtoEntry, Status>>;

    #[tracing::instrument(skip(self, request), fields(method = "GetEntries", account_id))]
    async fn get_entries(
        &self,
        request: Request<GetEntriesRequest>,
    ) -> Result<Response<Self::GetEntriesStream>, Status> {
        let req = request.into_inner();
        let account_id = AccountId::new(req.account_id).map_err(ServiceError::from)?;
        tracing::Span::current().record("account_id", account_id.value());

        let mut tx = self.storage.begin().await.map_err(ServiceError::from)?;

        // verify account exists
        let _ = self
            .storage
            .get_account(&mut tx, account_id)
            .await
            .map_err(ServiceError::from)?
            .ok_or_else(|| ServiceError::AccountNotFound {
                account_id: account_id.value(),
            })?;

        let entries = self
            .storage
            .get_entries_for_account(&mut tx, account_id)
            .await
            .map_err(ServiceError::from)?;

        tx.commit()
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;

        let (sender, receiver) = mpsc::channel(32);

        tokio::spawn(async move {
            for entry in entries {
                if sender.send(Ok(entry_to_proto(&entry))).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

// helper functions

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn parse_transfer_legs(proto_legs: &[ProtoTransferLeg]) -> Result<Vec<TransferLeg>, ServiceError> {
    let mut legs = Vec::with_capacity(proto_legs.len());

    for leg in proto_legs {
        let account_id = AccountId::new(leg.account_id).map_err(ServiceError::from)?;
        let entry_type = proto_to_entry_type(leg.entry_type)?;
        let amount = Amount::new(leg.amount).map_err(ServiceError::from)?;

        legs.push(TransferLeg::new(account_id, entry_type, amount).map_err(ServiceError::from)?);
    }

    Ok(legs)
}

fn proto_to_account_type(value: i32) -> Result<AccountType, ServiceError> {
    match value {
        1 => Ok(AccountType::Asset),
        2 => Ok(AccountType::Liability),
        3 => Ok(AccountType::Equity),
        4 => Ok(AccountType::Revenue),
        5 => Ok(AccountType::Expense),
        _ => Err(ServiceError::InvalidArgument(format!(
            "invalid account_type: {}",
            value
        ))),
    }
}

fn proto_to_entry_type(value: i32) -> Result<EntryType, ServiceError> {
    match value {
        1 => Ok(EntryType::Debit),
        2 => Ok(EntryType::Credit),
        _ => Err(ServiceError::InvalidArgument(format!(
            "invalid entry_type: {}",
            value
        ))),
    }
}

fn account_type_to_proto(t: AccountType) -> i32 {
    match t {
        AccountType::Asset => 1,
        AccountType::Liability => 2,
        AccountType::Equity => 3,
        AccountType::Revenue => 4,
        AccountType::Expense => 5,
    }
}

fn normal_balance_to_proto(n: NormalBalance) -> i32 {
    match n {
        NormalBalance::Debit => 1,
        NormalBalance::Credit => 2,
    }
}

fn entry_type_to_proto(t: EntryType) -> i32 {
    match t {
        EntryType::Debit => 1,
        EntryType::Credit => 2,
    }
}

fn entry_status_to_proto(s: EntryStatus) -> i32 {
    match s {
        EntryStatus::Pending => 1,
        EntryStatus::Posted => 2,
        EntryStatus::Voided => 3,
    }
}

fn transaction_status_to_proto(s: TransactionStatus) -> i32 {
    match s {
        TransactionStatus::Pending => 1,
        TransactionStatus::Posted => 2,
        TransactionStatus::Voided => 3,
        TransactionStatus::Failed => 4,
    }
}

fn account_to_proto(a: &Account) -> ProtoAccount {
    ProtoAccount {
        id: a.id().value(),
        account_type: account_type_to_proto(a.account_type()),
        normal_balance: normal_balance_to_proto(a.normal_balance()),
        asset_code: a.asset_code().to_string(),
        asset_scale: a.asset_scale() as i32,
        version: a.version(),
        created_at: a.created_at(),
    }
}

fn entry_to_proto(e: &Entry) -> ProtoEntry {
    ProtoEntry {
        id: e.id().value(),
        transaction_id: e.transaction_id().value(),
        account_id: e.account_id().value(),
        entry_type: entry_type_to_proto(e.entry_type()),
        amount: e.amount().value(),
        status: entry_status_to_proto(e.status()),
        created_at: e.created_at(),
    }
}

fn transaction_to_proto(t: &Transaction) -> ProtoTransaction {
    ProtoTransaction {
        id: t.id().value(),
        idempotency_key: t.idempotency_key().to_string(),
        status: transaction_status_to_proto(t.status()),
        created_at: t.created_at(),
        posted_at: t.posted_at(),
    }
}
