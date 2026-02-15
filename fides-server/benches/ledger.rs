use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use fides_server::domain::account::{AccountId, NormalBalance};
use fides_server::domain::entry::{Entry, EntryId, EntryStatus, EntryType};
use fides_server::domain::money::Amount;
use fides_server::domain::transaction::TransactionId;
use fides_server::domain::validation::{
    compute_account_balance, compute_balance_delta, validate_transaction_balance, TransferLeg,
};
use fides_server::storage::BalanceCache;

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

fn make_legs(n: usize) -> Vec<TransferLeg> {
    // n/2 debit legs + n/2 credit legs, each with amount 1000
    let half = n / 2;
    let mut legs = Vec::with_capacity(n);
    for i in 0..half {
        legs.push(
            TransferLeg::new(
                AccountId::new((i + 1) as i64).unwrap(),
                EntryType::Debit,
                Amount::new(1000).unwrap(),
            )
            .unwrap(),
        );
    }
    for i in 0..half {
        legs.push(
            TransferLeg::new(
                AccountId::new((half + i + 1) as i64).unwrap(),
                EntryType::Credit,
                Amount::new(1000).unwrap(),
            )
            .unwrap(),
        );
    }
    legs
}

fn bench_cache_get(c: &mut Criterion) {
    let cache = BalanceCache::new();
    for i in 1..=1000 {
        cache.set(AccountId::new(i).unwrap(), i * 100, i * 10);
    }

    c.bench_function("cache_get", |b| {
        let id = AccountId::new(500).unwrap();
        b.iter(|| {
            black_box(cache.get(black_box(id)));
        })
    });
}

fn bench_cache_apply_delta(c: &mut Criterion) {
    let cache = BalanceCache::new();
    for i in 1..=1000 {
        cache.set(AccountId::new(i).unwrap(), i * 100, i * 10);
    }

    c.bench_function("cache_apply_delta", |b| {
        let id = AccountId::new(500).unwrap();
        b.iter(|| {
            cache.apply_delta(black_box(id), black_box(100), black_box(50));
        })
    });
}

fn bench_compute_balance_delta(c: &mut Criterion) {
    let mut group = c.benchmark_group("compute_balance_delta");

    let cases = [
        ("debit_on_debit", NormalBalance::Debit, EntryType::Debit),
        ("credit_on_debit", NormalBalance::Debit, EntryType::Credit),
        ("credit_on_credit", NormalBalance::Credit, EntryType::Credit),
        ("debit_on_credit", NormalBalance::Credit, EntryType::Debit),
    ];

    for (name, normal, entry_type) in cases {
        let amount = Amount::new(1000).unwrap();
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(compute_balance_delta(
                    black_box(normal),
                    black_box(entry_type),
                    black_box(amount),
                ));
            })
        });
    }
    group.finish();
}

fn bench_validate_transaction_balance(c: &mut Criterion) {
    let mut group = c.benchmark_group("validate_transaction_balance");

    for n in [2, 4, 8] {
        let legs = make_legs(n);
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n}_legs")),
            &legs,
            |b, legs| {
                b.iter(|| {
                    let _ = black_box(validate_transaction_balance(black_box(legs)));
                })
            },
        );
    }
    group.finish();
}

fn bench_compute_account_balance(c: &mut Criterion) {
    let mut group = c.benchmark_group("compute_account_balance");

    for n in [10, 100, 1000] {
        let entries: Vec<Entry> = (1..=n)
            .map(|i| {
                let et = if i % 2 == 0 {
                    EntryType::Credit
                } else {
                    EntryType::Debit
                };
                make_entry(i, et, 100, EntryStatus::Posted)
            })
            .collect();

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n}_entries")),
            &entries,
            |b, entries| {
                b.iter(|| {
                    let _ = black_box(compute_account_balance(
                        black_box(NormalBalance::Debit),
                        black_box(entries),
                    ));
                })
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cache_get,
    bench_cache_apply_delta,
    bench_compute_balance_delta,
    bench_validate_transaction_balance,
    bench_compute_account_balance,
);
criterion_main!(benches);
