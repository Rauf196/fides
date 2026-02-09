use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::{gauge, histogram};
use sqlx::PgPool;
use tokio::sync::watch;

use crate::storage::BalanceCache;

/// background task that periodically verifies financial integrity.
///
/// three checks:
/// 1. global double-entry invariant (total debits == total credits)
/// 2. per-account balance reconciliation (materialized vs entry-computed)
/// 3. cache vs database consistency
pub struct IntegrityChecker {
    pool: PgPool,
    cache: Arc<BalanceCache>,
    interval: Duration,
}

impl IntegrityChecker {
    pub fn new(pool: PgPool, cache: Arc<BalanceCache>, interval: Duration) -> Self {
        Self {
            pool,
            cache,
            interval,
        }
    }

    /// run the checker loop until shutdown signal
    pub async fn run(self, mut shutdown_rx: watch::Receiver<bool>) {
        let mut interval = tokio::time::interval(self.interval);
        interval.tick().await; // skip first immediate tick

        loop {
            // bool pattern: avoid holding watch::Ref (non-Send) across await
            let should_run = tokio::select! {
                _ = interval.tick() => true,
                _ = shutdown_rx.wait_for(|v| *v) => false,
            };

            if should_run {
                self.run_all_checks().await;
            } else {
                tracing::info!("integrity checker shutting down");
                break;
            }
        }
    }

    async fn run_all_checks(&self) {
        tracing::debug!("running integrity checks");

        self.check_global_balance().await;
        self.check_account_balances().await;
        self.check_cache_consistency().await;

        let now = chrono::Utc::now().timestamp() as f64;
        gauge!("fides_integrity_last_check_timestamp").set(now);
    }

    /// check 1: global double-entry invariant.
    ///
    /// across all non-voided entries, total debits must equal total credits.
    async fn check_global_balance(&self) {
        let start = Instant::now();

        // entry_type: 1=debit, 2=credit. status: 3=voided (excluded)
        // ::BIGINT casts required — SUM(bigint) returns numeric, not bigint
        let result = sqlx::query_as::<_, (i64, i64)>(
            "SELECT \
                COALESCE(SUM(CASE WHEN entry_type = 1 THEN amount ELSE 0 END), 0)::BIGINT, \
                COALESCE(SUM(CASE WHEN entry_type = 2 THEN amount ELSE 0 END), 0)::BIGINT \
            FROM entries \
            WHERE status != 3",
        )
        .fetch_one(&self.pool)
        .await;

        let duration = start.elapsed().as_secs_f64();
        histogram!("fides_integrity_check_duration_seconds", "check" => "global_balance")
            .record(duration);

        match result {
            Ok((total_debits, total_credits)) => {
                let balanced = total_debits == total_credits;
                gauge!("fides_integrity_global_balanced").set(if balanced { 1.0 } else { 0.0 });

                if !balanced {
                    tracing::error!(
                        total_debits,
                        total_credits,
                        difference = total_debits - total_credits,
                        "INTEGRITY VIOLATION: global debits != credits",
                    );
                } else {
                    tracing::debug!(total_debits, total_credits, "global balance check passed");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "global balance check failed");
            }
        }
    }

    /// check 2: per-account balance reconciliation.
    ///
    /// recomputes posted and pending balances from entries and compares
    /// against the materialized values stored in the accounts table.
    async fn check_account_balances(&self) {
        let start = Instant::now();

        // query returns: account_id, account_type, materialized posted, materialized pending,
        // then 4 sums: posted_debits, posted_credits, pending_debits, pending_credits.
        // all SUMs cast to ::BIGINT (SUM(bigint) returns numeric).
        let result = sqlx::query_as::<_, (i64, i16, i64, i64, i64, i64, i64, i64)>(
            "SELECT \
                a.id, a.account_type, a.posted_balance, a.pending_balance, \
                COALESCE(SUM(CASE WHEN e.status = 2 AND e.entry_type = 1 THEN e.amount ELSE 0 END), 0)::BIGINT, \
                COALESCE(SUM(CASE WHEN e.status = 2 AND e.entry_type = 2 THEN e.amount ELSE 0 END), 0)::BIGINT, \
                COALESCE(SUM(CASE WHEN e.status = 1 AND e.entry_type = 1 THEN e.amount ELSE 0 END), 0)::BIGINT, \
                COALESCE(SUM(CASE WHEN e.status = 1 AND e.entry_type = 2 THEN e.amount ELSE 0 END), 0)::BIGINT \
            FROM accounts a \
            LEFT JOIN entries e ON e.account_id = a.id AND e.status != 3 \
            GROUP BY a.id, a.account_type, a.posted_balance, a.pending_balance",
        )
        .fetch_all(&self.pool)
        .await;

        let duration = start.elapsed().as_secs_f64();
        histogram!("fides_integrity_check_duration_seconds", "check" => "account_balances")
            .record(duration);

        match result {
            Ok(rows) => {
                let mut mismatches = 0u64;

                for (id, account_type, mat_posted, mat_pending, posted_debits, posted_credits, pending_debits, pending_credits) in &rows {
                    let debit_normal = is_debit_normal(*account_type);

                    let expected_posted = if debit_normal {
                        posted_debits - posted_credits
                    } else {
                        posted_credits - posted_debits
                    };

                    let expected_pending = if debit_normal {
                        pending_debits - pending_credits
                    } else {
                        pending_credits - pending_debits
                    };

                    if *mat_posted != expected_posted || *mat_pending != expected_pending {
                        mismatches += 1;
                        tracing::warn!(
                            account_id = id,
                            mat_posted,
                            expected_posted,
                            mat_pending,
                            expected_pending,
                            "account balance mismatch: materialized != computed from entries",
                        );
                    }
                }

                gauge!("fides_integrity_account_mismatches").set(mismatches as f64);

                if mismatches == 0 {
                    tracing::debug!(accounts = rows.len(), "account balance check passed");
                } else {
                    tracing::error!(mismatches, accounts = rows.len(), "account balance mismatches detected");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "account balance check failed");
            }
        }
    }

    /// check 3: cache vs database consistency.
    ///
    /// compares in-memory cache balances with materialized DB balances.
    /// DB query and cache read are not atomic, so transient mismatches
    /// can occur under load.
    async fn check_cache_consistency(&self) {
        let start = Instant::now();

        let result = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT id, posted_balance, pending_balance FROM accounts",
        )
        .fetch_all(&self.pool)
        .await;

        let duration = start.elapsed().as_secs_f64();
        histogram!("fides_integrity_check_duration_seconds", "check" => "cache_consistency")
            .record(duration);

        match result {
            Ok(db_rows) => {
                let cache_entries = self.cache.iter();

                // build lookup from cache
                let cache_map: HashMap<i64, (i64, i64)> = cache_entries
                    .into_iter()
                    .map(|(id, posted, pending)| (id.value(), (posted, pending)))
                    .collect();

                let mut mismatches = 0u64;

                // check DB accounts against cache
                for (id, db_posted, db_pending) in &db_rows {
                    match cache_map.get(id) {
                        Some((cache_posted, cache_pending)) => {
                            if *db_posted != *cache_posted || *db_pending != *cache_pending {
                                mismatches += 1;
                                tracing::warn!(
                                    account_id = id,
                                    db_posted,
                                    db_pending,
                                    cache_posted,
                                    cache_pending,
                                    "cache mismatch: cache != database",
                                );
                            }
                        }
                        None => {
                            mismatches += 1;
                            tracing::warn!(
                                account_id = id,
                                "cache mismatch: account in DB but missing from cache",
                            );
                        }
                    }
                }

                // check for accounts in cache but not in DB (shouldn't happen)
                let db_ids: std::collections::HashSet<i64> =
                    db_rows.iter().map(|(id, _, _)| *id).collect();
                for (id, _, _) in self.cache.iter() {
                    if !db_ids.contains(&id.value()) {
                        mismatches += 1;
                        tracing::warn!(
                            account_id = %id,
                            "cache mismatch: account in cache but missing from DB",
                        );
                    }
                }

                gauge!("fides_integrity_cache_mismatches").set(mismatches as f64);

                if mismatches == 0 {
                    tracing::debug!(
                        db_accounts = db_rows.len(),
                        cache_accounts = cache_map.len(),
                        "cache consistency check passed",
                    );
                } else {
                    tracing::warn!(mismatches, "cache consistency mismatches detected");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "cache consistency check failed");
            }
        }
    }
}

/// classify account type as debit-normal based on raw SMALLINT from DB.
///
/// asset=1, expense=5 are debit-normal.
/// liability=2, equity=3, revenue=4 are credit-normal.
fn is_debit_normal(account_type: i16) -> bool {
    matches!(account_type, 1 | 5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::{AccountType, NormalBalance};

    /// verify that is_debit_normal() raw integer classification matches
    /// the domain type AccountType::normal_balance() for all 5 types.
    ///
    /// this test is the safety net for the raw integer match in the integrity
    /// checker. if the domain types ever change their mapping, this test fails.
    #[test]
    fn debit_normal_classification_matches_domain_types() {
        let cases: &[(i16, AccountType)] = &[
            (1, AccountType::Asset),
            (2, AccountType::Liability),
            (3, AccountType::Equity),
            (4, AccountType::Revenue),
            (5, AccountType::Expense),
        ];

        for &(raw, account_type) in cases {
            let domain_is_debit = account_type.normal_balance() == NormalBalance::Debit;
            let checker_is_debit = is_debit_normal(raw);
            assert_eq!(
                domain_is_debit, checker_is_debit,
                "mismatch for account_type raw={} ({:?}): domain says debit_normal={}, checker says {}",
                raw, account_type, domain_is_debit, checker_is_debit,
            );
        }
    }

    #[test]
    fn unknown_account_type_is_credit_normal() {
        // unknown types default to credit-normal (conservative — won't flip balances)
        assert!(!is_debit_normal(0));
        assert!(!is_debit_normal(99));
    }
}
