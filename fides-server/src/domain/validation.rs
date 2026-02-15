use std::fmt;

use super::account::{AccountId, NormalBalance};
use super::entry::{Entry, EntryStatus, EntryType};
use super::money::Amount;

/// input for creating a transaction leg (before entry IDs exist)
#[derive(Debug, Clone)]
pub struct TransferLeg {
    account_id: AccountId,
    entry_type: EntryType,
    amount: Amount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferLegError {
    ZeroAmount,
}

/// computed balance for an account, derived from entries
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountBalance {
    posted: i64,
    pending: i64,
    available: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    InsufficientLegs {
        count: usize,
    },
    UnbalancedTransaction {
        total_debits: i64,
        total_credits: i64,
    },
    SumOverflow,
    BalanceOverflow,
}

impl TransferLeg {
    /// create a new transfer leg, amount must be > 0
    pub fn new(
        account_id: AccountId,
        entry_type: EntryType,
        amount: Amount,
    ) -> Result<Self, TransferLegError> {
        if amount.value() == 0 {
            return Err(TransferLegError::ZeroAmount);
        }
        Ok(TransferLeg {
            account_id,
            entry_type,
            amount,
        })
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
}

impl AccountBalance {
    /// construct from computed values
    pub fn from_computed(posted: i64, pending: i64, available: i64) -> Self {
        AccountBalance {
            posted,
            pending,
            available,
        }
    }

    pub fn posted(&self) -> i64 {
        self.posted
    }

    pub fn pending(&self) -> i64 {
        self.pending
    }

    pub fn available(&self) -> i64 {
        self.available
    }

    /// check if there are sufficient available funds
    pub fn has_available(&self, amount: Amount) -> bool {
        self.available >= amount.value()
    }
}

/// validate that transfer legs form a balanced transaction
pub fn validate_transaction_balance(legs: &[TransferLeg]) -> Result<(), ValidationError> {
    if legs.len() < 2 {
        return Err(ValidationError::InsufficientLegs { count: legs.len() });
    }

    let mut total_debits: i64 = 0;
    let mut total_credits: i64 = 0;

    for leg in legs {
        match leg.entry_type() {
            EntryType::Debit => {
                total_debits = total_debits
                    .checked_add(leg.amount().value())
                    .ok_or(ValidationError::SumOverflow)?;
            }
            EntryType::Credit => {
                total_credits = total_credits
                    .checked_add(leg.amount().value())
                    .ok_or(ValidationError::SumOverflow)?;
            }
        }
    }

    if total_debits != total_credits {
        return Err(ValidationError::UnbalancedTransaction {
            total_debits,
            total_credits,
        });
    }

    Ok(())
}

/// compute account balance from entries
pub fn compute_account_balance(
    normal_balance: NormalBalance,
    entries: &[Entry],
) -> Result<AccountBalance, ValidationError> {
    let mut posted_debits: i64 = 0;
    let mut posted_credits: i64 = 0;
    let mut pending_debits: i64 = 0;
    let mut pending_credits: i64 = 0;

    for entry in entries {
        if entry.status() == EntryStatus::Voided {
            continue;
        }

        let amount = entry.amount().value();

        match (entry.status(), entry.entry_type()) {
            (EntryStatus::Posted, EntryType::Debit) => {
                posted_debits = posted_debits
                    .checked_add(amount)
                    .ok_or(ValidationError::SumOverflow)?;
            }
            (EntryStatus::Posted, EntryType::Credit) => {
                posted_credits = posted_credits
                    .checked_add(amount)
                    .ok_or(ValidationError::SumOverflow)?;
            }
            (EntryStatus::Pending, EntryType::Debit) => {
                pending_debits = pending_debits
                    .checked_add(amount)
                    .ok_or(ValidationError::SumOverflow)?;
            }
            (EntryStatus::Pending, EntryType::Credit) => {
                pending_credits = pending_credits
                    .checked_add(amount)
                    .ok_or(ValidationError::SumOverflow)?;
            }
            (EntryStatus::Voided, _) => unreachable!(),
        }
    }

    let (posted, pending) = match normal_balance {
        NormalBalance::Debit => {
            let posted = posted_debits
                .checked_sub(posted_credits)
                .ok_or(ValidationError::BalanceOverflow)?;
            let pending = pending_debits
                .checked_sub(pending_credits)
                .ok_or(ValidationError::BalanceOverflow)?;
            (posted, pending)
        }
        NormalBalance::Credit => {
            let posted = posted_credits
                .checked_sub(posted_debits)
                .ok_or(ValidationError::BalanceOverflow)?;
            let pending = pending_credits
                .checked_sub(pending_debits)
                .ok_or(ValidationError::BalanceOverflow)?;
            (posted, pending)
        }
    };

    let available = posted
        .checked_sub(pending)
        .ok_or(ValidationError::BalanceOverflow)?;

    Ok(AccountBalance::from_computed(posted, pending, available))
}

/// compute the signed delta to apply to an account's balance
///
/// positive = increases balance, negative = decreases balance
///
/// this follows the accounting sign convention:
/// - debit-normal accounts (asset, expense): debits increase, credits decrease
/// - credit-normal accounts (liability, equity, revenue): credits increase, debits decrease
pub fn compute_balance_delta(
    normal_balance: NormalBalance,
    entry_type: EntryType,
    amount: Amount,
) -> i64 {
    let raw = amount.value();
    match (normal_balance, entry_type) {
        // same direction as normal balance = positive delta
        (NormalBalance::Debit, EntryType::Debit) => raw,
        (NormalBalance::Credit, EntryType::Credit) => raw,
        // opposite direction = negative delta
        (NormalBalance::Debit, EntryType::Credit) => -raw,
        (NormalBalance::Credit, EntryType::Debit) => -raw,
    }
}

impl fmt::Display for TransferLegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransferLegError::ZeroAmount => write!(f, "transfer leg amount cannot be zero"),
        }
    }
}

impl std::error::Error for TransferLegError {}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::InsufficientLegs { count } => {
                write!(f, "transaction requires at least 2 legs, got {}", count)
            }
            ValidationError::UnbalancedTransaction {
                total_debits,
                total_credits,
            } => {
                write!(
                    f,
                    "unbalanced transaction: debits {} != credits {}",
                    total_debits, total_credits
                )
            }
            ValidationError::SumOverflow => write!(f, "amount sum overflow"),
            ValidationError::BalanceOverflow => write!(f, "balance computation overflow"),
        }
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::AccountId;
    use crate::domain::entry::{Entry, EntryId, EntryStatus, EntryType};
    use crate::domain::money::Amount;
    use crate::domain::transaction::TransactionId;

    fn leg(account: i64, entry_type: EntryType, amount: i64) -> TransferLeg {
        TransferLeg::new(
            AccountId::new(account).unwrap(),
            entry_type,
            Amount::new(amount).unwrap(),
        )
        .unwrap()
    }

    fn entry(
        id: i64,
        account: i64,
        entry_type: EntryType,
        amount: i64,
        status: EntryStatus,
    ) -> Entry {
        Entry::new(
            EntryId::new(id).unwrap(),
            TransactionId::new(1).unwrap(),
            AccountId::new(account).unwrap(),
            entry_type,
            Amount::new(amount).unwrap(),
            status,
            1000000,
        )
        .unwrap()
    }

    // TransferLeg tests

    #[test]
    fn transfer_leg_rejects_zero_amount() {
        let result = TransferLeg::new(
            AccountId::new(1).unwrap(),
            EntryType::Debit,
            Amount::new(0).unwrap(),
        );
        assert!(matches!(result, Err(TransferLegError::ZeroAmount)));
    }

    #[test]
    fn transfer_leg_accepts_positive_amount() {
        let leg = TransferLeg::new(
            AccountId::new(1).unwrap(),
            EntryType::Debit,
            Amount::new(1000).unwrap(),
        );
        assert!(leg.is_ok());
    }

    // validate_transaction_balance tests

    #[test]
    fn rejects_empty_legs() {
        let result = validate_transaction_balance(&[]);
        assert!(matches!(
            result,
            Err(ValidationError::InsufficientLegs { count: 0 })
        ));
    }

    #[test]
    fn rejects_single_leg() {
        let legs = vec![leg(1, EntryType::Debit, 1000)];
        let result = validate_transaction_balance(&legs);
        assert!(matches!(
            result,
            Err(ValidationError::InsufficientLegs { count: 1 })
        ));
    }

    #[test]
    fn accepts_balanced_two_leg_transaction() {
        let legs = vec![
            leg(1, EntryType::Debit, 1000),
            leg(2, EntryType::Credit, 1000),
        ];
        assert!(validate_transaction_balance(&legs).is_ok());
    }

    #[test]
    fn accepts_balanced_multi_leg_transaction() {
        // split payment: 1 debit, 2 credits
        let legs = vec![
            leg(1, EntryType::Debit, 1000),
            leg(2, EntryType::Credit, 600),
            leg(3, EntryType::Credit, 400),
        ];
        assert!(validate_transaction_balance(&legs).is_ok());
    }

    #[test]
    fn rejects_unbalanced_transaction() {
        let legs = vec![
            leg(1, EntryType::Debit, 1000),
            leg(2, EntryType::Credit, 500),
        ];
        let result = validate_transaction_balance(&legs);
        assert!(matches!(
            result,
            Err(ValidationError::UnbalancedTransaction {
                total_debits: 1000,
                total_credits: 500
            })
        ));
    }

    #[test]
    fn detects_overflow_on_sum() {
        let legs = vec![
            leg(1, EntryType::Debit, i64::MAX),
            leg(2, EntryType::Debit, 1),
            leg(3, EntryType::Credit, i64::MAX),
            leg(4, EntryType::Credit, 1),
        ];
        let result = validate_transaction_balance(&legs);
        assert!(matches!(result, Err(ValidationError::SumOverflow)));
    }

    // compute_account_balance tests

    #[test]
    fn empty_entries_returns_zero_balance() {
        let result = compute_account_balance(NormalBalance::Debit, &[]);
        assert!(result.is_ok());
        let balance = result.unwrap();
        assert_eq!(balance.posted(), 0);
        assert_eq!(balance.pending(), 0);
        assert_eq!(balance.available(), 0);
    }

    #[test]
    fn single_posted_debit_on_debit_normal_account() {
        let entries = vec![entry(1, 1, EntryType::Debit, 1000, EntryStatus::Posted)];
        let balance = compute_account_balance(NormalBalance::Debit, &entries).unwrap();
        assert_eq!(balance.posted(), 1000);
        assert_eq!(balance.pending(), 0);
        assert_eq!(balance.available(), 1000);
    }

    #[test]
    fn single_posted_credit_on_debit_normal_account() {
        let entries = vec![entry(1, 1, EntryType::Credit, 1000, EntryStatus::Posted)];
        let balance = compute_account_balance(NormalBalance::Debit, &entries).unwrap();
        assert_eq!(balance.posted(), -1000);
        assert_eq!(balance.pending(), 0);
        assert_eq!(balance.available(), -1000);
    }

    #[test]
    fn credit_normal_account_balance() {
        let entries = vec![
            entry(1, 1, EntryType::Credit, 1000, EntryStatus::Posted),
            entry(2, 1, EntryType::Debit, 300, EntryStatus::Posted),
        ];
        let balance = compute_account_balance(NormalBalance::Credit, &entries).unwrap();
        assert_eq!(balance.posted(), 700); // 1000 - 300
        assert_eq!(balance.pending(), 0);
        assert_eq!(balance.available(), 700);
    }

    #[test]
    fn pending_entries_affect_pending_balance() {
        let entries = vec![
            entry(1, 1, EntryType::Debit, 1000, EntryStatus::Posted),
            entry(2, 1, EntryType::Debit, 200, EntryStatus::Pending),
        ];
        let balance = compute_account_balance(NormalBalance::Debit, &entries).unwrap();
        assert_eq!(balance.posted(), 1000);
        assert_eq!(balance.pending(), 200);
        assert_eq!(balance.available(), 800); // 1000 - 200
    }

    #[test]
    fn voided_entries_excluded() {
        let entries = vec![
            entry(1, 1, EntryType::Debit, 1000, EntryStatus::Posted),
            entry(2, 1, EntryType::Debit, 500, EntryStatus::Voided),
        ];
        let balance = compute_account_balance(NormalBalance::Debit, &entries).unwrap();
        assert_eq!(balance.posted(), 1000); // voided entry ignored
    }

    #[test]
    fn has_available_check() {
        let balance = AccountBalance::from_computed(1000, 200, 800);
        assert!(balance.has_available(Amount::new(800).unwrap()));
        assert!(balance.has_available(Amount::new(500).unwrap()));
        assert!(!balance.has_available(Amount::new(801).unwrap()));
    }

    #[test]
    fn handles_negative_balance() {
        // overdraft scenario: more credits than debits on asset account
        let entries = vec![
            entry(1, 1, EntryType::Debit, 100, EntryStatus::Posted),
            entry(2, 1, EntryType::Credit, 500, EntryStatus::Posted),
        ];
        let balance = compute_account_balance(NormalBalance::Debit, &entries).unwrap();
        assert_eq!(balance.posted(), -400);
        assert_eq!(balance.available(), -400);
    }

    #[test]
    fn balance_computation_detects_overflow() {
        let entries = vec![
            entry(1, 1, EntryType::Debit, i64::MAX, EntryStatus::Posted),
            entry(2, 1, EntryType::Debit, 1, EntryStatus::Posted),
        ];
        let result = compute_account_balance(NormalBalance::Debit, &entries);
        assert!(matches!(result, Err(ValidationError::SumOverflow)));
    }

    #[test]
    fn mixed_pending_and_posted_entries() {
        // realistic scenario: settled balance + pending hold
        let entries = vec![
            entry(1, 1, EntryType::Debit, 1000, EntryStatus::Posted),
            entry(2, 1, EntryType::Credit, 200, EntryStatus::Posted),
            entry(3, 1, EntryType::Credit, 150, EntryStatus::Pending),
        ];
        let balance = compute_account_balance(NormalBalance::Debit, &entries).unwrap();
        assert_eq!(balance.posted(), 800); // 1000 - 200
        assert_eq!(balance.pending(), -150); // 0 - 150 (pending credit on debit-normal)
        assert_eq!(balance.available(), 950); // 800 - (-150) = 950
    }

    #[test]
    fn pending_credit_on_credit_normal_account() {
        // liability account with pending credit (e.g., pending charge)
        let entries = vec![
            entry(1, 1, EntryType::Credit, 500, EntryStatus::Posted),
            entry(2, 1, EntryType::Credit, 100, EntryStatus::Pending),
        ];
        let balance = compute_account_balance(NormalBalance::Credit, &entries).unwrap();
        assert_eq!(balance.posted(), 500);
        assert_eq!(balance.pending(), 100);
        assert_eq!(balance.available(), 400); // 500 - 100
    }

    // compute_balance_delta tests

    #[test]
    fn compute_balance_delta_debit_on_debit_normal() {
        let delta = compute_balance_delta(
            NormalBalance::Debit,
            EntryType::Debit,
            Amount::new(1000).unwrap(),
        );
        assert_eq!(delta, 1000); // same direction = positive
    }

    #[test]
    fn compute_balance_delta_credit_on_debit_normal() {
        let delta = compute_balance_delta(
            NormalBalance::Debit,
            EntryType::Credit,
            Amount::new(1000).unwrap(),
        );
        assert_eq!(delta, -1000); // opposite direction = negative
    }

    #[test]
    fn compute_balance_delta_credit_on_credit_normal() {
        let delta = compute_balance_delta(
            NormalBalance::Credit,
            EntryType::Credit,
            Amount::new(1000).unwrap(),
        );
        assert_eq!(delta, 1000); // same direction = positive
    }

    #[test]
    fn compute_balance_delta_debit_on_credit_normal() {
        let delta = compute_balance_delta(
            NormalBalance::Credit,
            EntryType::Debit,
            Amount::new(1000).unwrap(),
        );
        assert_eq!(delta, -1000); // opposite direction = negative
    }

    #[test]
    fn compute_balance_delta_matches_compute_account_balance() {
        // verify that applying deltas produces the same result as compute_account_balance
        // for each normal_balance + entry_type combination
        let test_cases = [
            (NormalBalance::Debit, EntryType::Debit, 500),
            (NormalBalance::Debit, EntryType::Credit, 300),
            (NormalBalance::Credit, EntryType::Credit, 500),
            (NormalBalance::Credit, EntryType::Debit, 300),
        ];

        for (normal, entry_type, amt) in test_cases {
            let amount = Amount::new(amt).unwrap();
            let delta = compute_balance_delta(normal, entry_type, amount);

            // create a single posted entry and compute balance
            let entries = vec![entry(1, 1, entry_type, amt, EntryStatus::Posted)];
            let balance = compute_account_balance(normal, &entries).unwrap();

            assert_eq!(
                delta,
                balance.posted(),
                "delta {} should match posted balance {} for {:?}/{:?}",
                delta,
                balance.posted(),
                normal,
                entry_type
            );
        }
    }

    #[test]
    fn available_is_posted_minus_pending_invariant() {
        // verify the mathematical invariant holds for various scenarios
        let test_cases = vec![
            // (posted_debits, posted_credits, pending_debits, pending_credits, normal)
            (1000, 0, 200, 0, NormalBalance::Debit),
            (1000, 300, 0, 100, NormalBalance::Debit),
            (0, 500, 0, 100, NormalBalance::Credit),
            (200, 800, 50, 150, NormalBalance::Credit),
        ];

        for (pd, pc, pend_d, pend_c, normal) in test_cases {
            let mut entries = vec![];
            let mut id = 1;

            if pd > 0 {
                entries.push(entry(id, 1, EntryType::Debit, pd, EntryStatus::Posted));
                id += 1;
            }
            if pc > 0 {
                entries.push(entry(id, 1, EntryType::Credit, pc, EntryStatus::Posted));
                id += 1;
            }
            if pend_d > 0 {
                entries.push(entry(id, 1, EntryType::Debit, pend_d, EntryStatus::Pending));
                id += 1;
            }
            if pend_c > 0 {
                entries.push(entry(
                    id,
                    1,
                    EntryType::Credit,
                    pend_c,
                    EntryStatus::Pending,
                ));
            }

            let balance = compute_account_balance(normal, &entries).unwrap();

            // the invariant: available = posted - pending
            assert_eq!(
                balance.available(),
                balance.posted() - balance.pending(),
                "invariant violated for entries: {:?}",
                entries
            );
        }
    }
}
