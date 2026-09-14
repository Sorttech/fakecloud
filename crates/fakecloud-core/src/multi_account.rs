//! Generic multi-account state container.
//!
//! Wraps a `HashMap<AccountId, T>` so each AWS account gets its own isolated
//! state instance. Accounts are created lazily via [`MultiAccountState::get_or_create`]
//! the first time a request targets them — matching the design in #381 where
//! "an account exists because a credential resolves to it."

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Trait implemented by per-service state structs that participate in
/// multi-account isolation.
pub trait AccountState: Sized {
    /// Create a fresh, empty state for the given account.
    fn new_for_account(account_id: &str, region: &str, endpoint: &str) -> Self;

    /// Called after a new account state is created via [`MultiAccountState::get_or_create`],
    /// with a reference to an existing sibling state. Services can override
    /// this to propagate shared resources (e.g. body caches) to the new state.
    fn inherit_from(&mut self, _sibling: &Self) {}
}

/// Account-partitioned state container.
///
/// Holds one `T` per account id. The `default_account_id` is pre-created at
/// startup so unauthenticated requests (which fall back to `--account-id`)
/// always have a state to land in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiAccountState<T> {
    default_account_id: String,
    region: String,
    endpoint: String,
    accounts: HashMap<String, T>,
}

impl<T: AccountState> MultiAccountState<T> {
    /// Create a new container, pre-populating the default account.
    pub fn new(default_account_id: &str, region: &str, endpoint: &str) -> Self {
        let mut accounts = HashMap::new();
        accounts.insert(
            default_account_id.to_string(),
            T::new_for_account(default_account_id, region, endpoint),
        );
        Self {
            default_account_id: default_account_id.to_string(),
            region: region.to_string(),
            endpoint: endpoint.to_string(),
            accounts,
        }
    }

    /// Project account states while preserving the container's routing defaults.
    pub fn map<U>(&self, mut f: impl FnMut(&T) -> U) -> MultiAccountState<U> {
        MultiAccountState {
            default_account_id: self.default_account_id.clone(),
            region: self.region.clone(),
            endpoint: self.endpoint.clone(),
            accounts: self
                .accounts
                .iter()
                .map(|(k, v)| (k.clone(), f(v)))
                .collect(),
        }
    }

    /// Get or lazily create the state for `account_id`.
    ///
    /// When a new account is created, [`AccountState::inherit_from`] is called
    /// with the default account's state so services can propagate shared
    /// resources (e.g. body caches).
    pub fn get_or_create(&mut self, account_id: &str) -> &mut T {
        if !self.accounts.contains_key(account_id) {
            let mut state = T::new_for_account(account_id, &self.region, &self.endpoint);
            // Let the new state inherit shared resources from the default account.
            if let Some(sibling) = self.accounts.get(&self.default_account_id) {
                state.inherit_from(sibling);
            }
            self.accounts.insert(account_id.to_string(), state);
        }
        self.accounts.get_mut(account_id).unwrap()
    }

    /// Get or lazily create the state for `account_id`, then run `init` on
    /// the newly created state. The callback is only invoked when the account
    /// is freshly created, not on subsequent lookups.
    pub fn get_or_create_with<F>(&mut self, account_id: &str, init: F) -> &mut T
    where
        F: FnOnce(&mut T),
    {
        if !self.accounts.contains_key(account_id) {
            let mut state = T::new_for_account(account_id, &self.region, &self.endpoint);
            init(&mut state);
            self.accounts.insert(account_id.to_string(), state);
        }
        self.accounts.get_mut(account_id).unwrap()
    }

    /// Read-only lookup. Returns `None` if the account has never been seen.
    pub fn get(&self, account_id: &str) -> Option<&T> {
        self.accounts.get(account_id)
    }

    /// Mutable lookup without auto-creation.
    pub fn get_mut(&mut self, account_id: &str) -> Option<&mut T> {
        self.accounts.get_mut(account_id)
    }

    /// Iterate over all account states (read-only).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.accounts.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate over all account states (mutable).
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut T)> {
        self.accounts.iter_mut().map(|(k, v)| (k.as_str(), v))
    }

    /// The default account id configured via `--account-id`.
    pub fn default_account_id(&self) -> &str {
        &self.default_account_id
    }

    /// Mutable reference to the default account's state (always exists).
    pub fn default_mut(&mut self) -> &mut T {
        self.accounts.get_mut(&self.default_account_id).unwrap()
    }

    /// Reference to the default account's state (always exists).
    pub fn default_ref(&self) -> &T {
        self.accounts.get(&self.default_account_id).unwrap()
    }

    /// Reset all accounts back to empty state. The default account is
    /// recreated; all other accounts are dropped.
    pub fn reset(&mut self) {
        self.accounts.clear();
        self.accounts.insert(
            self.default_account_id.clone(),
            T::new_for_account(&self.default_account_id, &self.region, &self.endpoint),
        );
    }

    /// Find the first account whose state satisfies `predicate` and return
    /// the account id. Useful for resolving globally-unique resources (e.g.
    /// S3 bucket names) back to their owning account.
    pub fn find_account<F>(&self, predicate: F) -> Option<&str>
    where
        F: Fn(&T) -> bool,
    {
        self.accounts
            .iter()
            .find(|(_, v)| predicate(v))
            .map(|(k, _)| k.as_str())
    }

    /// Number of accounts with state.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }

    /// Region shared by all accounts.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Endpoint shared by all accounts.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct TestState {
        account_id: String,
        items: Vec<String>,
    }

    impl AccountState for TestState {
        fn new_for_account(account_id: &str, _region: &str, _endpoint: &str) -> Self {
            Self {
                account_id: account_id.to_string(),
                items: Vec::new(),
            }
        }
    }

    #[test]
    fn default_account_exists_on_creation() {
        let mas: MultiAccountState<TestState> =
            MultiAccountState::new("111111111111", "us-east-1", "http://localhost:4566");
        assert_eq!(mas.account_count(), 1);
        assert!(mas.get("111111111111").is_some());
    }

    #[test]
    fn get_or_create_makes_new_account() {
        let mut mas: MultiAccountState<TestState> =
            MultiAccountState::new("111111111111", "us-east-1", "http://localhost:4566");
        let state = mas.get_or_create("222222222222");
        assert_eq!(state.account_id, "222222222222");
        assert_eq!(mas.account_count(), 2);
    }

    #[test]
    fn get_returns_none_for_unknown() {
        let mas: MultiAccountState<TestState> =
            MultiAccountState::new("111111111111", "us-east-1", "http://localhost:4566");
        assert!(mas.get("999999999999").is_none());
    }

    #[test]
    fn reset_clears_all_but_default() {
        let mut mas: MultiAccountState<TestState> =
            MultiAccountState::new("111111111111", "us-east-1", "http://localhost:4566");
        mas.get_or_create("222222222222");
        mas.get_or_create("333333333333");
        assert_eq!(mas.account_count(), 3);
        mas.reset();
        assert_eq!(mas.account_count(), 1);
        assert!(mas.get("111111111111").is_some());
        assert!(mas.get("222222222222").is_none());
    }

    #[test]
    fn iter_visits_all_accounts() {
        let mut mas: MultiAccountState<TestState> =
            MultiAccountState::new("111111111111", "us-east-1", "http://localhost:4566");
        mas.get_or_create("222222222222");
        let ids: Vec<&str> = mas.iter().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"111111111111"));
        assert!(ids.contains(&"222222222222"));
    }
}
