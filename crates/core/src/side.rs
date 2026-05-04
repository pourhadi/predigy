//! YES / NO side and Buy / Sell action.

use serde::{Deserialize, Serialize};

/// Which side of a binary contract.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

impl Side {
    #[inline]
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
        }
    }
}

/// Buying or selling the chosen side. Kalshi's "sell YES" is economically
/// equivalent to "buy NO" at the complementary price; we keep the distinction
/// because the API requires it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Buy,
    Sell,
}

impl Action {
    #[inline]
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opposites() {
        assert_eq!(Side::Yes.opposite(), Side::No);
        assert_eq!(Side::No.opposite(), Side::Yes);
        assert_eq!(Action::Buy.opposite(), Action::Sell);
    }
}
