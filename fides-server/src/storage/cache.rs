use dashmap::DashMap;

use crate::domain::account::AccountId;
use crate::domain::validation::AccountBalance;

#[derive(Debug, Clone, Copy)]
struct CachedBalance {
    posted: i64,
    pending: i64,
}

/// in-memory balance cache backed by DashMap
///
/// mirrors posted_balance and pending_balance from the accounts table.
/// updated after db commit — microsecond staleness window before cache reflects committed state.
pub struct BalanceCache {
    balances: DashMap<AccountId, CachedBalance>,
}

impl BalanceCache {
    pub fn new() -> Self {
        Self {
            balances: DashMap::new(),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            balances: DashMap::with_capacity(capacity),
        }
    }

    /// populate cache from database on startup
    pub fn rehydrate(&self, data: Vec<(AccountId, i64, i64)>) {
        for (id, posted, pending) in data {
            self.balances.insert(id, CachedBalance { posted, pending });
        }
    }

    /// get balance (returns None if not in cache)
    pub fn get(&self, id: AccountId) -> Option<AccountBalance> {
        self.balances.get(&id).map(|entry| {
            let available = entry.posted.saturating_sub(entry.pending);
            AccountBalance::from_computed(entry.posted, entry.pending, available)
        })
    }

    /// update after db commit - apply signed deltas
    pub fn apply_delta(&self, id: AccountId, delta_posted: i64, delta_pending: i64) {
        self.balances.entry(id).and_modify(|b| {
            b.posted = b.posted.saturating_add(delta_posted);
            b.pending = b.pending.saturating_add(delta_pending);
        });
    }

    /// set absolute values (for new accounts or reset)
    pub fn set(&self, id: AccountId, posted: i64, pending: i64) {
        self.balances.insert(id, CachedBalance { posted, pending });
    }

    /// remove an account from cache
    pub fn remove(&self, id: AccountId) {
        self.balances.remove(&id);
    }

    /// snapshot all cached balances into a vec.
    ///
    /// collects immediately to release DashMap shard locks.
    pub fn iter(&self) -> Vec<(AccountId, i64, i64)> {
        self.balances
            .iter()
            .map(|entry| (*entry.key(), entry.value().posted, entry.value().pending))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.balances.len()
    }

    pub fn is_empty(&self) -> bool {
        self.balances.is_empty()
    }
}

impl Default for BalanceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_id(id: i64) -> AccountId {
        AccountId::new(id).unwrap()
    }

    #[test]
    fn cache_get_returns_none_for_missing() {
        let cache = BalanceCache::new();
        assert!(cache.get(account_id(1)).is_none());
    }

    #[test]
    fn cache_set_and_get() {
        let cache = BalanceCache::new();
        cache.set(account_id(1), 1000, 200);

        let balance = cache.get(account_id(1)).unwrap();
        assert_eq!(balance.posted(), 1000);
        assert_eq!(balance.pending(), 200);
        assert_eq!(balance.available(), 800);
    }

    #[test]
    fn cache_apply_delta_positive() {
        let cache = BalanceCache::new();
        cache.set(account_id(1), 1000, 100);

        cache.apply_delta(account_id(1), 500, 50);

        let balance = cache.get(account_id(1)).unwrap();
        assert_eq!(balance.posted(), 1500);
        assert_eq!(balance.pending(), 150);
        assert_eq!(balance.available(), 1350);
    }

    #[test]
    fn cache_apply_delta_negative() {
        let cache = BalanceCache::new();
        cache.set(account_id(1), 1000, 200);

        cache.apply_delta(account_id(1), -300, -100);

        let balance = cache.get(account_id(1)).unwrap();
        assert_eq!(balance.posted(), 700);
        assert_eq!(balance.pending(), 100);
        assert_eq!(balance.available(), 600);
    }

    #[test]
    fn cache_apply_delta_to_missing_account_is_noop() {
        let cache = BalanceCache::new();
        // should not panic, just does nothing
        cache.apply_delta(account_id(999), 100, 50);
        assert!(cache.get(account_id(999)).is_none());
    }

    #[test]
    fn cache_rehydrate_multiple_accounts() {
        let cache = BalanceCache::new();
        let data = vec![
            (account_id(1), 1000, 100),
            (account_id(2), 2000, 200),
            (account_id(3), 3000, 300),
        ];

        cache.rehydrate(data);

        assert_eq!(cache.len(), 3);

        let b1 = cache.get(account_id(1)).unwrap();
        assert_eq!(b1.posted(), 1000);
        assert_eq!(b1.pending(), 100);

        let b2 = cache.get(account_id(2)).unwrap();
        assert_eq!(b2.posted(), 2000);
        assert_eq!(b2.pending(), 200);

        let b3 = cache.get(account_id(3)).unwrap();
        assert_eq!(b3.posted(), 3000);
        assert_eq!(b3.pending(), 300);
    }

    #[test]
    fn cache_with_capacity() {
        let cache = BalanceCache::with_capacity(100);
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_remove() {
        let cache = BalanceCache::new();
        cache.set(account_id(1), 1000, 100);
        assert!(cache.get(account_id(1)).is_some());

        cache.remove(account_id(1));
        assert!(cache.get(account_id(1)).is_none());
    }

    #[test]
    fn cache_apply_delta_saturates_on_overflow() {
        let cache = BalanceCache::new();
        cache.set(account_id(1), i64::MAX - 10, 0);

        cache.apply_delta(account_id(1), 100, 0);

        let balance = cache.get(account_id(1)).unwrap();
        assert_eq!(balance.posted(), i64::MAX); // saturated
    }

    #[test]
    fn cache_apply_delta_saturates_on_underflow() {
        let cache = BalanceCache::new();
        cache.set(account_id(1), i64::MIN + 10, 0);

        cache.apply_delta(account_id(1), -100, 0);

        let balance = cache.get(account_id(1)).unwrap();
        assert_eq!(balance.posted(), i64::MIN); // saturated
    }

    #[test]
    fn cache_default() {
        let cache = BalanceCache::default();
        assert!(cache.is_empty());
    }
}
