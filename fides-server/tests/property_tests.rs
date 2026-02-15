//! property tests for double-entry accounting invariants

use proptest::prelude::*;

use fides_server::domain::account::{AccountId, NormalBalance};
use fides_server::domain::entry::{Entry, EntryId, EntryStatus, EntryType};
use fides_server::domain::money::Amount;
use fides_server::domain::transaction::TransactionId;
use fides_server::domain::validation::{
    compute_account_balance, compute_balance_delta, validate_transaction_balance, TransferLeg,
};

// safe range for amounts: up to 20 entries summed, MAX/100 gives double margin
const MAX_AMOUNT: i64 = i64::MAX / 100;

fn arb_entry_type() -> impl Strategy<Value = EntryType> {
    prop_oneof![Just(EntryType::Debit), Just(EntryType::Credit)]
}

fn arb_normal_balance() -> impl Strategy<Value = NormalBalance> {
    prop_oneof![Just(NormalBalance::Debit), Just(NormalBalance::Credit)]
}

fn arb_entry_status() -> impl Strategy<Value = EntryStatus> {
    prop_oneof![
        Just(EntryStatus::Pending),
        Just(EntryStatus::Posted),
        Just(EntryStatus::Voided),
    ]
}

fn make_entry(id: i64, entry_type: EntryType, amount: i64, status: EntryStatus) -> Entry {
    Entry::new(
        EntryId::new(id).unwrap(),
        TransactionId::new(1).unwrap(),
        AccountId::new(1).unwrap(),
        entry_type,
        Amount::new(amount).unwrap(),
        status,
        1_000_000,
    )
    .unwrap()
}

proptest! {
    /// balanced round-trip: generate N debit legs summing to T, M credit legs
    /// summing to T, and verify validate_transaction_balance accepts them.
    #[test]
    fn balanced_transaction_always_validates(
        total in 1..=MAX_AMOUNT,
        n_debits in 1..=5usize,
        n_credits in 1..=5usize,
    ) {
        // partition total into n_debits debit legs
        let debit_amounts = partition(total, n_debits);
        // partition total into n_credits credit legs
        let credit_amounts = partition(total, n_credits);

        let mut legs = Vec::new();
        for (i, &amt) in debit_amounts.iter().enumerate() {
            let account_id = AccountId::new((i + 1) as i64).unwrap();
            legs.push(TransferLeg::new(account_id, EntryType::Debit, Amount::new(amt).unwrap()).unwrap());
        }
        for (i, &amt) in credit_amounts.iter().enumerate() {
            let account_id = AccountId::new((n_debits + i + 1) as i64).unwrap();
            legs.push(TransferLeg::new(account_id, EntryType::Credit, Amount::new(amt).unwrap()).unwrap());
        }

        prop_assert!(validate_transaction_balance(&legs).is_ok());
    }

    /// unbalanced detection: take a balanced set and add 1 to a debit,
    /// then verify validation fails with the correct totals.
    #[test]
    fn unbalanced_transaction_detected(
        total in 2..=MAX_AMOUNT,
    ) {
        let legs = vec![
            TransferLeg::new(AccountId::new(1).unwrap(), EntryType::Debit, Amount::new(total).unwrap()).unwrap(),
            TransferLeg::new(AccountId::new(2).unwrap(), EntryType::Credit, Amount::new(total - 1).unwrap()).unwrap(),
        ];

        let result = validate_transaction_balance(&legs);
        match result {
            Err(fides_server::domain::validation::ValidationError::UnbalancedTransaction {
                total_debits,
                total_credits,
            }) => {
                prop_assert_eq!(total_debits, total);
                prop_assert_eq!(total_credits, total - 1);
            }
            other => prop_assert!(false, "expected UnbalancedTransaction, got {:?}", other),
        }
    }

    /// delta consistency: for any (NormalBalance, EntryType, Amount), the delta
    /// from compute_balance_delta matches a single-entry balance computation.
    #[test]
    fn delta_matches_single_entry_balance(
        normal in arb_normal_balance(),
        entry_type in arb_entry_type(),
        amt in 1..=MAX_AMOUNT,
    ) {
        let amount = Amount::new(amt).unwrap();
        let delta = compute_balance_delta(normal, entry_type, amount);

        let entry = make_entry(1, entry_type, amt, EntryStatus::Posted);
        let balance = compute_account_balance(normal, &[entry]).unwrap();

        prop_assert_eq!(delta, balance.posted());
    }

    /// available = posted - pending invariant holds for arbitrary entries
    #[test]
    fn available_equals_posted_minus_pending(
        normal in arb_normal_balance(),
        entries in prop::collection::vec(
            (arb_entry_type(), 1..=MAX_AMOUNT, arb_entry_status()),
            1..=20,
        ),
    ) {
        let entries: Vec<Entry> = entries
            .into_iter()
            .enumerate()
            .map(|(i, (et, amt, status))| make_entry((i + 1) as i64, et, amt, status))
            .collect();

        if let Ok(balance) = compute_account_balance(normal, &entries) {
            prop_assert_eq!(
                balance.available(),
                balance.posted() - balance.pending(),
                "available != posted - pending"
            );
        }
        // overflow is acceptable, just skip
    }

    /// voided entries are invisible to balance computation
    #[test]
    fn voided_entries_dont_affect_balance(
        normal in arb_normal_balance(),
        base_entries in prop::collection::vec(
            (arb_entry_type(), 1..=MAX_AMOUNT),
            1..=10,
        ),
        voided_entries in prop::collection::vec(
            (arb_entry_type(), 1..=MAX_AMOUNT),
            1..=10,
        ),
    ) {
        let mut id = 1i64;

        let base: Vec<Entry> = base_entries
            .iter()
            .map(|&(et, amt)| {
                let e = make_entry(id, et, amt, EntryStatus::Posted);
                id += 1;
                e
            })
            .collect();

        let base_balance = compute_account_balance(normal, &base);

        // add voided entries
        let mut with_voided = base.clone();
        for &(et, amt) in &voided_entries {
            with_voided.push(make_entry(id, et, amt, EntryStatus::Voided));
            id += 1;
        }

        let voided_balance = compute_account_balance(normal, &with_voided);

        // both should produce the same result (or both overflow)
        match (base_balance, voided_balance) {
            (Ok(b1), Ok(b2)) => {
                prop_assert_eq!(b1.posted(), b2.posted());
                prop_assert_eq!(b1.pending(), b2.pending());
                prop_assert_eq!(b1.available(), b2.available());
            }
            (Err(_), Err(_)) => {} // both overflow, fine
            (Ok(b), Err(e)) => prop_assert!(false, "base ok {:?} but voided err {:?}", b, e),
            (Err(e), Ok(b)) => prop_assert!(false, "base err {:?} but voided ok {:?}", e, b),
        }
    }

    /// commutative ordering: shuffling entries doesn't change the balance
    #[test]
    fn entry_order_doesnt_affect_balance(
        normal in arb_normal_balance(),
        mut entries in prop::collection::vec(
            (arb_entry_type(), 1..=MAX_AMOUNT, arb_entry_status()),
            2..=20,
        ),
        seed in any::<u64>(),
    ) {
        let original: Vec<Entry> = entries
            .iter()
            .enumerate()
            .map(|(i, &(et, amt, status))| make_entry((i + 1) as i64, et, amt, status))
            .collect();

        // shuffle using seed
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        seed.hash(&mut hasher);
        let h = hasher.finish() as usize;
        let len = entries.len();
        for i in 0..len {
            entries.swap(i, (i + h) % len);
        }

        let shuffled: Vec<Entry> = entries
            .iter()
            .enumerate()
            .map(|(i, &(et, amt, status))| make_entry((i + 1) as i64, et, amt, status))
            .collect();

        let bal1 = compute_account_balance(normal, &original);
        let bal2 = compute_account_balance(normal, &shuffled);

        match (bal1, bal2) {
            (Ok(b1), Ok(b2)) => {
                prop_assert_eq!(b1.posted(), b2.posted());
                prop_assert_eq!(b1.pending(), b2.pending());
                prop_assert_eq!(b1.available(), b2.available());
            }
            (Err(_), Err(_)) => {}
            _ => prop_assert!(false, "one overflowed but not the other"),
        }
    }
}

/// partition a total amount into n positive parts
fn partition(total: i64, n: usize) -> Vec<i64> {
    if n == 1 {
        return vec![total];
    }

    let base = total / n as i64;
    let remainder = total % n as i64;

    let mut parts = vec![base; n];
    // distribute remainder across first `remainder` parts
    for part in parts.iter_mut().take(remainder as usize) {
        *part += 1;
    }

    // ensure no zero amounts (shift from larger to smaller if needed)
    for i in 0..n {
        if parts[i] == 0 {
            // find a neighbor with > 1 and take 1
            for j in 0..n {
                if parts[j] > 1 {
                    parts[j] -= 1;
                    parts[i] = 1;
                    break;
                }
            }
        }
    }

    parts
}
