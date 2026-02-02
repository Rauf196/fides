use std::fmt;

/// unique identifier for an account
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountId(i64);

/// the 5 fundamental account categories
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

/// which side increases the account balance
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormalBalance {
    Debit,
    Credit,
}

/// a balance-holding account in the ledger
#[derive(Debug, Clone)]
pub struct Account {
    id: AccountId,
    account_type: AccountType,
    asset_code: String,
    asset_scale: u8,
    version: i64,
    created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    InvalidId(i64),
    EmptyAssetCode,
    InvalidScale(u8),
    InvalidVersion(i64),
    InvalidTimestamp(i64),
}

impl AccountId {
    /// create a new account id, must be positive
    pub fn new(id: i64) -> Result<Self, AccountError> {
        if id <= 0 {
            return Err(AccountError::InvalidId(id));
        }
        Ok(AccountId(id))
    }

    /// create from trusted source (db reads)
    pub(crate) fn from_raw(id: i64) -> Self {
        AccountId(id)
    }

    pub fn value(self) -> i64 {
        self.0
    }
}

impl AccountType {
    /// returns the normal balance for this account type
    /// this is a fixed rule from accounting standards
    pub fn normal_balance(self) -> NormalBalance {
        match self {
            AccountType::Asset | AccountType::Expense => NormalBalance::Debit,
            AccountType::Liability | AccountType::Equity | AccountType::Revenue => {
                NormalBalance::Credit
            }
        }
    }
}

impl Account {
    pub fn new(
        id: AccountId,
        account_type: AccountType,
        asset_code: String,
        asset_scale: u8,
        version: i64,
        created_at: i64,
    ) -> Result<Self, AccountError> {
        if asset_code.is_empty() {
            return Err(AccountError::EmptyAssetCode);
        }
        if asset_scale > 18 {
            return Err(AccountError::InvalidScale(asset_scale));
        }
        if version < 0 {
            return Err(AccountError::InvalidVersion(version));
        }
        if created_at <= 0 {
            return Err(AccountError::InvalidTimestamp(created_at));
        }

        Ok(Account {
            id,
            account_type,
            asset_code,
            asset_scale,
            version,
            created_at,
        })
    }

    pub fn id(&self) -> AccountId {
        self.id
    }

    pub fn account_type(&self) -> AccountType {
        self.account_type
    }

    pub fn normal_balance(&self) -> NormalBalance {
        self.account_type.normal_balance()
    }

    pub fn asset_code(&self) -> &str {
        &self.asset_code
    }

    pub fn asset_scale(&self) -> u8 {
        self.asset_scale
    }

    pub fn version(&self) -> i64 {
        self.version
    }

    pub fn created_at(&self) -> i64 {
        self.created_at
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::InvalidId(id) => write!(f, "invalid account id: {}", id),
            AccountError::EmptyAssetCode => write!(f, "asset code cannot be empty"),
            AccountError::InvalidScale(s) => write!(f, "invalid scale {}, max is 18", s),
            AccountError::InvalidVersion(v) => write!(f, "invalid version: {}", v),
            AccountError::InvalidTimestamp(t) => write!(f, "invalid timestamp: {}", t),
        }
    }
}

impl std::error::Error for AccountError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_rejects_zero() {
        assert!(AccountId::new(0).is_err());
    }

    #[test]
    fn account_id_rejects_negative() {
        assert!(AccountId::new(-1).is_err());
        assert!(AccountId::new(-100).is_err());
    }

    #[test]
    fn account_id_accepts_positive() {
        assert!(AccountId::new(1).is_ok());
        assert!(AccountId::new(i64::MAX).is_ok());
    }

    #[test]
    fn account_type_normal_balance() {
        assert_eq!(AccountType::Asset.normal_balance(), NormalBalance::Debit);
        assert_eq!(AccountType::Expense.normal_balance(), NormalBalance::Debit);
        assert_eq!(AccountType::Liability.normal_balance(), NormalBalance::Credit);
        assert_eq!(AccountType::Equity.normal_balance(), NormalBalance::Credit);
        assert_eq!(AccountType::Revenue.normal_balance(), NormalBalance::Credit);
    }

    #[test]
    fn account_derives_normal_balance() {
        let account = Account::new(
            AccountId::new(1).unwrap(),
            AccountType::Asset,
            "USD".to_string(),
            2,
            0,
            1000,
        )
        .unwrap();

        assert_eq!(account.normal_balance(), NormalBalance::Debit);
    }

    #[test]
    fn rejects_empty_asset_code() {
        let result = Account::new(
            AccountId::new(1).unwrap(),
            AccountType::Asset,
            "".to_string(),
            2,
            0,
            1000,
        );

        assert!(matches!(result, Err(AccountError::EmptyAssetCode)));
    }

    #[test]
    fn rejects_invalid_scale() {
        let result = Account::new(
            AccountId::new(1).unwrap(),
            AccountType::Asset,
            "USD".to_string(),
            19,
            0,
            1000,
        );

        assert!(matches!(result, Err(AccountError::InvalidScale(19))));
    }

    #[test]
    fn rejects_negative_version() {
        let result = Account::new(
            AccountId::new(1).unwrap(),
            AccountType::Asset,
            "USD".to_string(),
            2,
            -1,
            1000,
        );

        assert!(matches!(result, Err(AccountError::InvalidVersion(-1))));
    }

    #[test]
    fn rejects_invalid_timestamp() {
        let result = Account::new(
            AccountId::new(1).unwrap(),
            AccountType::Asset,
            "USD".to_string(),
            2,
            0,
            0,
        );

        assert!(matches!(result, Err(AccountError::InvalidTimestamp(0))));
    }

    #[test]
    fn accepts_valid_account() {
        let result = Account::new(
            AccountId::new(1).unwrap(),
            AccountType::Liability,
            "BTC".to_string(),
            8,
            1,
            1234567890,
        );

        assert!(result.is_ok());
        let account = result.unwrap();
        assert_eq!(account.asset_code(), "BTC");
        assert_eq!(account.asset_scale(), 8);
    }
}
