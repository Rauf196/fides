use std::fmt;

/// money amount in smallest unit (cents, satoshis, etc.)
/// always non-negative - direction is determined by EntryType
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Amount(i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmountError {
    Negative(i64),
    Overflow,
    Underflow,
}

impl Amount {
    /// create a new amount, rejecting negative values
    pub fn new(value: i64) -> Result<Self, AmountError> {
        if value < 0 {
            return Err(AmountError::Negative(value));
        }
        Ok(Amount(value))
    }

    /// create from trusted source (db reads, internal calculations)
    /// caller is responsible for ensuring value is valid
    pub(crate) fn from_raw(value: i64) -> Self {
        Amount(value)
    }

    /// zero amount
    pub const fn zero() -> Self {
        Amount(0)
    }

    /// get the raw i64 value
    pub fn value(self) -> i64 {
        self.0
    }

    /// checked addition - returns error on overflow
    pub fn checked_add(self, other: Amount) -> Result<Amount, AmountError> {
        self.0
            .checked_add(other.0)
            .map(Amount)
            .ok_or(AmountError::Overflow)
    }

    /// checked subtraction - returns error if result would be negative
    pub fn checked_sub(self, other: Amount) -> Result<Amount, AmountError> {
        self.0
            .checked_sub(other.0)
            .filter(|&v| v >= 0)
            .map(Amount)
            .ok_or(AmountError::Underflow)
    }
}

impl AmountError {
    fn message(&self) -> &'static str {
        match self {
            AmountError::Negative(_) => "amount cannot be negative",
            AmountError::Overflow => "amount overflow",
            AmountError::Underflow => "amount underflow (would be negative)",
        }
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for AmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AmountError::Negative(v) => write!(f, "{}: {}", self.message(), v),
            _ => write!(f, "{}", self.message()),
        }
    }
}

impl std::error::Error for AmountError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_negative() {
        assert!(Amount::new(-1).is_err());
        assert!(Amount::new(-100).is_err());
    }

    #[test]
    fn new_accepts_zero_and_positive() {
        assert!(Amount::new(0).is_ok());
        assert!(Amount::new(1).is_ok());
        assert!(Amount::new(i64::MAX).is_ok());
    }

    #[test]
    fn checked_add_works() {
        let a = Amount::new(100).unwrap();
        let b = Amount::new(50).unwrap();
        assert_eq!(a.checked_add(b).unwrap().value(), 150);
    }

    #[test]
    fn checked_add_overflow() {
        let a = Amount::new(i64::MAX).unwrap();
        let b = Amount::new(1).unwrap();
        assert!(a.checked_add(b).is_err());
    }

    #[test]
    fn checked_sub_works() {
        let a = Amount::new(100).unwrap();
        let b = Amount::new(30).unwrap();
        assert_eq!(a.checked_sub(b).unwrap().value(), 70);
    }

    #[test]
    fn checked_sub_underflow() {
        let a = Amount::new(50).unwrap();
        let b = Amount::new(100).unwrap();
        assert!(a.checked_sub(b).is_err());
    }

    #[test]
    fn checked_sub_to_zero() {
        let a = Amount::new(100).unwrap();
        let b = Amount::new(100).unwrap();
        let result = a.checked_sub(b).unwrap();
        assert_eq!(result.value(), 0);
    }
}
